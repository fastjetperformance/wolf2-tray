//! The engine: exporter events → ownship selection → minute segments → spool
//! → the site. Two tasks share the spool directory and the status facts:
//!
//! - the RECORDER consumes exporter events and seals a segment whenever the
//!   mission minute changes, the ownship disappears, or the socket drops;
//! - the PUSHER wakes when a segment is spooled or the member signs in (and on
//!   a backoff timer only while segments are left waiting), asks
//!   `/api/recordings` which recordings are live, and PUTs every spooled
//!   minute that one of their windows covers. An empty spool makes no request.
//!
//! The session is read per request, never snapshotted (spec §6): a sign-in
//! while the tray runs reaches the next upload, and a 401 is never retried —
//! it flips the status to "Sign in needed" until the member acts.

use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};

use crate::api::{Api, ApiError, PutOutcome, RecordingEntry};
use crate::config::Config;
use crate::exporter::{self, Event};
use crate::ownship::OwnshipTracker;
use crate::segment::{SegmentWriter, StreamHeader, minute_of, mission_ms};
use crate::spool::Spool;
use crate::state::State;

/// Retry cadence while segments are waiting and the last pass left some behind:
/// starts here and doubles to `RETRY_MAX`. An empty spool costs no requests at
/// all — the pusher only wakes for a new segment or a sign-in.
pub const RETRY_MIN: Duration = Duration::from_secs(30);
pub const RETRY_MAX: Duration = Duration::from_secs(600);
/// Spooled minutes sealed this long before a recording's `startedMs` still
/// belong to it (spec §5): the recorder's clock and the pilot's differ a little.
pub const START_SLACK_MS: i64 = 120_000;
/// A recording is assumed never to run longer than this: a minute sealed later
/// than `startedMs + MAX_RECORDING_MS` does not belong to it even if the index
/// still says `live` (a recorder that died without finalizing leaves exactly
/// that). Such minutes wait for a newer recording whose window covers them.
pub const MAX_RECORDING_MS: i64 = 2 * 3600 * 1000;
/// Frames of a single-object stream to see before adopting it as the ownship.
const SINGLE_OBJECT_FRAMES: usize = 3;

pub struct Engine {
    pub config: Config,
    pub state: State,
    pub spool: Spool,
    pub api: Api,
    pub edition: String,
    /// Poked by the recorder (segment spooled) and by the tray (signed in).
    pub wake: std::sync::Arc<Notify>,
}

impl Engine {
    /// Runs until `shutdown` turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let (events_tx, events_rx) = mpsc::channel::<Event>(1024);
        let port = self.config.port();
        let exporter = tokio::spawn(exporter::run(port, events_tx));
        let recorder = tokio::spawn(record(
            events_rx,
            self.state.clone(),
            self.spool_handle(),
            self.edition.clone(),
            self.config.pilot_name(),
            self.wake.clone(),
        ));
        let pusher = tokio::spawn(push_loop(
            self.api.clone(),
            self.config.tenant.clone(),
            self.state.clone(),
            self.spool_handle(),
            self.wake.clone(),
            shutdown.clone(),
        ));
        let _ = shutdown.wait_for(|stop| *stop).await;
        exporter.abort();
        recorder.abort();
        pusher.abort();
    }

    fn spool_handle(&self) -> Spool {
        Spool::new(self.spool.dir().to_path_buf())
    }
}

/// Session ids: `<UTC yyyymmddThhmmssZ>-<4 hex>` (spec §5).
pub fn mint_session_id(now: chrono::DateTime<chrono::Utc>) -> String {
    let r: u16 = rand::random();
    format!("{}-{:04x}", now.format("%Y%m%dT%H%M%SZ"), r)
}

struct Recorder {
    state: State,
    spool: Spool,
    edition: String,
    tracker: OwnshipTracker,
    header: Option<StreamHeader>,
    session: Option<String>,
    writer: Option<SegmentWriter>,
    current_seconds: Option<f64>,
    frames_seen: usize,
    wake: std::sync::Arc<Notify>,
}

async fn record(
    mut events: mpsc::Receiver<Event>,
    state: State,
    spool: Spool,
    edition: String,
    pilot_name: Option<String>,
    wake: std::sync::Arc<Notify>,
) {
    let mut r = Recorder {
        state,
        spool,
        edition,
        tracker: OwnshipTracker::new(pilot_name),
        header: None,
        session: None,
        writer: None,
        current_seconds: None,
        frames_seen: 0,
        wake,
    };
    while let Some(event) = events.recv().await {
        r.handle(event);
    }
    r.seal();
}

impl Recorder {
    fn handle(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.state.update(|f| f.exporter_connected = true);
            }
            Event::Disconnected => {
                self.seal();
                self.header = None;
                self.tracker.reset();
                self.current_seconds = None;
                self.frames_seen = 0;
                self.state.update(|f| {
                    f.exporter_connected = false;
                    f.pilot = None;
                });
            }
            Event::Header {
                reference_time,
                data_source,
            } => {
                self.seal();
                self.tracker.reset();
                self.session = None;
                self.frames_seen = 0;
                match StreamHeader::parse(&reference_time, &data_source) {
                    Some(h) => {
                        tracing::info!(reference_time, data_source, "exporter header");
                        self.header = Some(h);
                    }
                    None => {
                        tracing::warn!(
                            reference_time,
                            "exporter header has no usable ReferenceTime; ignoring this connection"
                        );
                        self.header = None;
                    }
                }
                self.state.update(|f| f.pilot = None);
            }
            Event::Frame { seconds } => {
                self.frames_seen += 1;
                self.current_seconds = Some(seconds);
                if self.frames_seen >= SINGLE_OBJECT_FRAMES
                    && self.tracker.chosen().is_none()
                    && self.tracker.adopt_single_object()
                {
                    // Statics arrive with the next line; the session starts now.
                    self.start_session();
                }
                let Some(h) = &self.header else { return };
                let minute = minute_of(mission_ms(h.reference_ms, seconds));
                if self.writer.as_ref().is_some_and(|w| w.minute() != minute) {
                    self.seal();
                }
                if let Some(w) = self.writer.as_mut() {
                    w.frame(seconds);
                }
            }
            Event::Removed { id } => {
                if self.tracker.chosen() == Some(id.as_str()) {
                    self.seal();
                    self.session = None;
                    self.state.update(|f| f.pilot = None);
                }
                self.tracker.removed(&id);
            }
            Event::Object { id, props, raw } => {
                let was_chosen = self.tracker.chosen().is_some();
                if !self.tracker.observe(&id, &props) {
                    return;
                }
                if !was_chosen {
                    self.start_session();
                }
                let (Some(h), Some(seconds)) = (&self.header, self.current_seconds) else {
                    return;
                };
                let pilot = self.tracker.pilot().unwrap_or("").to_string();
                if self.writer.is_none() {
                    let minute = minute_of(mission_ms(h.reference_ms, seconds));
                    let mut w = SegmentWriter::new(h.clone(), &self.edition, &id, &pilot, minute);
                    w.frame(seconds);
                    self.writer = Some(w);
                }
                let statics = self.tracker.statics().to_vec();
                if let Some(w) = self.writer.as_mut() {
                    w.object_line(&raw, &statics);
                }
                if self.state.snapshot().pilot.as_deref() != Some(pilot.as_str())
                    && !pilot.is_empty()
                {
                    self.state.update(|f| f.pilot = Some(pilot.clone()));
                }
            }
        }
    }

    fn start_session(&mut self) {
        if self.session.is_none() {
            let id = mint_session_id(chrono::Utc::now());
            tracing::info!(session = id, "ownship session started");
            self.session = Some(id);
        }
    }

    /// Seal the open segment into the spool.
    fn seal(&mut self) {
        let Some(w) = self.writer.take() else { return };
        if w.is_empty() {
            return;
        }
        let pilot = self.tracker.pilot().unwrap_or("").to_string();
        let Some(session) = self.session.clone() else {
            return;
        };
        let seg = w.finish(&pilot);
        // The route's own checks, run here first: a segment the site would
        // refuse must never sit in the spool being retried.
        if let Err(e) = crate::segment::validate(&seg.gz) {
            tracing::error!(
                minute = seg.minute,
                "segment failed self-validation ({e}); dropped"
            );
            self.state
                .update(|f| f.last_error = Some(format!("segment: {e}")));
            return;
        }
        match self
            .spool
            .put(&session, &seg, chrono::Utc::now().timestamp_millis())
        {
            Ok(_) => {
                tracing::info!(
                    minute = seg.minute,
                    frames = seg.frames,
                    bytes = seg.gz.len(),
                    "segment spooled"
                );
                let n = self.spool.list().len();
                self.state.update(|f| f.spooled = n);
                self.wake.notify_one();
            }
            Err(e) => {
                tracing::error!("spool write failed: {e}");
                self.state
                    .update(|f| f.last_error = Some(format!("spool: {e}")));
            }
        }
    }
}

async fn push_loop(
    api: Api,
    configured_tenant: Option<String>,
    state: State,
    spool: Spool,
    wake: std::sync::Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tenant = configured_tenant;
    let mut retry = RETRY_MIN;
    loop {
        if *shutdown.borrow() {
            return;
        }
        spool.expire(chrono::Utc::now().timestamp_millis());
        let remaining = push_once(&api, &mut tenant, &state, &spool).await;
        // No requests while nothing waits: sleep until a segment is spooled or
        // the member signs in. With leftovers, retry on a backoff that a wake
        // resets — a new minute while flying is the natural cadence.
        if remaining == 0 {
            retry = RETRY_MIN;
            tokio::select! {
                _ = wake.notified() => {}
                _ = shutdown.changed() => {}
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(retry) => {
                    retry = (retry * 2).min(RETRY_MAX);
                }
                _ = wake.notified() => {
                    retry = RETRY_MIN;
                }
                _ = shutdown.changed() => {}
            }
        }
    }
}

/// One pass: push every spooled minute that a live (or finalizing) recording's
/// window covers. Returns how many entries are still spooled afterwards; makes
/// no request when that would be zero anyway.
async fn push_once(api: &Api, tenant: &mut Option<String>, state: &State, spool: &Spool) -> usize {
    let signed_in = crate::auth::current_session().is_some();
    let entries = spool.list();
    state.update(|f| {
        f.signed_in = signed_in;
        f.spooled = entries.len();
    });
    if !signed_in || entries.is_empty() {
        return entries.len();
    }
    if tenant.is_none() {
        match api.tenants().await {
            Ok(list) => {
                let first = list
                    .iter()
                    .find(|t| t.visible)
                    .or(list.first())
                    .map(|t| t.id.clone());
                match first {
                    Some(id) => {
                        tracing::info!(tenant = id, "tenant resolved from membership");
                        *tenant = Some(id);
                    }
                    None => {
                        state
                            .update(|f| f.last_error = Some("no tenant on this membership".into()));
                        return entries.len();
                    }
                }
            }
            Err(e) => {
                note_error(state, e);
                return entries.len();
            }
        }
    }
    let tenant_id = tenant.clone().expect("tenant resolved");
    let recordings = match api.recordings(&tenant_id).await {
        Ok(r) => r,
        Err(e) => {
            note_error(state, e);
            return entries.len();
        }
    };
    // The status line names the recording a minute sealed now would go to.
    let now = chrono::Utc::now().timestamp_millis();
    state.update(|f| {
        f.live_recording = choose_target(&recordings, now)
            .filter(|t| t.is_live())
            .map(|t| t.scope());
        f.sign_in_needed = false;
        f.last_error = None;
    });
    for entry in entries {
        let Some(target) = choose_target(&recordings, entry.meta.received_at_ms) else {
            continue;
        };
        let scope = target.scope();
        let gz = match spool.read(&entry) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("spool read failed: {e}");
                spool.remove(&entry);
                continue;
            }
        };
        let m = &entry.meta;
        match api
            .put_ownship_segment(&scope, &m.session, m.minute, gz, &m.pilot, &m.object_id)
            .await
        {
            Ok(PutOutcome::Stored) | Ok(PutOutcome::AlreadyStored) => {
                tracing::info!(minute = m.minute, session = m.session, "segment pushed");
                spool.remove(&entry);
                state.update(|f| f.pushed += 1);
            }
            Ok(PutOutcome::Conflict) => {
                tracing::warn!(
                    minute = m.minute,
                    "segment conflicts with a stored one; dropped"
                );
                spool.remove(&entry);
            }
            Ok(PutOutcome::Rejected(why)) => {
                tracing::warn!(
                    minute = m.minute,
                    "segment rejected by the site ({why}); dropped"
                );
                spool.remove(&entry);
            }
            Ok(PutOutcome::Gone) => {
                tracing::warn!(
                    minute = m.minute,
                    scope,
                    "recording gone; segment kept until it expires"
                );
                break;
            }
            Ok(PutOutcome::NotLive) => {
                tracing::info!(scope, "recording no longer live; segments kept until they expire");
                break;
            }
            Err(e) => {
                note_error(state, e);
                break;
            }
        }
    }
    let n = spool.list().len();
    state.update(|f| f.spooled = n);
    n
}

/// The recording a minute sealed at `sealed_ms` belongs to: the newest live
/// one whose window covers it, else the newest finalizing one (so an in-flight
/// push finishes). None → the minute waits for a recording that fits.
pub fn choose_target(recordings: &[RecordingEntry], sealed_ms: i64) -> Option<RecordingEntry> {
    let covering = |r: &&RecordingEntry| covers(r, sealed_ms);
    recordings
        .iter()
        .filter(|r| r.is_live())
        .filter(covering)
        .max_by_key(|r| r.started_ms)
        .or_else(|| {
            recordings
                .iter()
                .filter(|r| r.accepts_pushes())
                .filter(covering)
                .max_by_key(|r| r.started_ms)
        })
        .cloned()
}

/// Spec §5: a recording's window is `startedMs − START_SLACK_MS` to
/// `startedMs + MAX_RECORDING_MS`, in the pilot's wall clock.
pub fn covers(target: &RecordingEntry, sealed_ms: i64) -> bool {
    sealed_ms >= target.started_ms - START_SLACK_MS
        && sealed_ms <= target.started_ms + MAX_RECORDING_MS
}

fn note_error(state: &State, e: ApiError) {
    match e {
        ApiError::Expired(err) => {
            tracing::warn!("{err}");
            state.update(|f| {
                f.sign_in_needed = true;
                f.live_recording = None;
            });
        }
        ApiError::Denied(msg) | ApiError::Other(msg) => {
            tracing::warn!("{msg}");
            state.update(|f| f.last_error = Some(msg));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(status: &str, started_ms: i64) -> RecordingEntry {
        RecordingEntry {
            partition: "202610".into(),
            recording: "shadowlands".into(),
            instant: format!("i{started_ms}"),
            started_ms,
            status: Some(status.into()),
        }
    }

    #[test]
    fn session_ids_follow_the_spec_grammar() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-29T10:11:12Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let id = mint_session_id(now);
        assert!(id.starts_with("20261029T101112Z-"), "{id}");
        assert_eq!(id.len(), "20261029T101112Z-".len() + 4);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn the_newest_live_recording_wins_then_finalizing() {
        let list = vec![
            rec("complete", 1),
            rec("live", 5),
            rec("live", 9),
            rec("finalizing", 20),
        ];
        assert_eq!(choose_target(&list, 25).unwrap().started_ms, 9);
        let list = vec![rec("complete", 1), rec("finalizing", 20)];
        assert_eq!(choose_target(&list, 25).unwrap().started_ms, 20);
        assert!(choose_target(&[rec("complete", 1)], 25).is_none());
    }

    #[test]
    fn a_stale_live_recording_is_not_a_target_for_a_minute_sealed_two_hours_on() {
        // A recorder that died at startedMs=0 and never finalized: the index
        // still says live. A minute sealed 3 h later waits instead.
        let stale = rec("live", 0);
        let three_hours = 3 * 3600 * 1000;
        assert!(choose_target(&[stale.clone()], three_hours).is_none());
        assert!(covers(&stale, MAX_RECORDING_MS));
        assert!(!covers(&stale, MAX_RECORDING_MS + 1));
        // ...and goes to the newer recording whose window covers it, even
        // though the stale one is still listed.
        let fresh = rec("live", three_hours - 60_000);
        let list = vec![stale, fresh];
        assert_eq!(
            choose_target(&list, three_hours).unwrap().started_ms,
            three_hours - 60_000
        );
        // A minute sealed before the fresh one started still goes nowhere.
        assert!(choose_target(&list, three_hours - START_SLACK_MS - 60_001).is_none());
    }

    #[test]
    fn spooled_minutes_from_before_the_recording_are_left_alone() {
        let target = rec("live", 1_000_000);
        assert!(covers(&target, 1_000_000 - START_SLACK_MS));
        assert!(!covers(&target, 1_000_000 - START_SLACK_MS - 1));
    }
}
