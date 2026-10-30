//! What the tray shows: one line derived from a handful of facts that the
//! exporter, the recorder and the pusher update independently.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

#[derive(Debug, Default, Clone)]
pub struct Facts {
    pub signed_in: bool,
    /// A 401 happened since the last sign-in: say so until the member acts.
    pub sign_in_needed: bool,
    pub exporter_connected: bool,
    /// The ownship's pilot label once chosen.
    pub pilot: Option<String>,
    /// `{partition}/{recording}/{instant}` of the recording being pushed to.
    pub live_recording: Option<String>,
    pub spooled: usize,
    pub pushed: usize,
    pub last_error: Option<String>,
}

impl Facts {
    pub fn line(&self) -> String {
        if self.sign_in_needed {
            return "Sign in needed".into();
        }
        if !self.signed_in {
            return "Signed out".into();
        }
        if !self.exporter_connected {
            return match self.spooled {
                0 => "Waiting for DCS".into(),
                n => format!("Waiting for DCS · {n} minute{} spooled", plural(n)),
            };
        }
        let Some(pilot) = &self.pilot else {
            return "Connected, no aircraft".into();
        };
        match (&self.live_recording, self.spooled) {
            (Some(rec), _) => format!("Pushing {pilot} to {}", short_recording(rec)),
            (None, 0) => format!("Flying as {pilot}, no live recording"),
            (None, n) => format!(
                "Spooling {pilot}, no live recording ({n} minute{})",
                plural(n)
            ),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `202610/shadowlands/20261028_192125` → `shadowlands 20261028_192125`.
fn short_recording(scope: &str) -> String {
    let parts: Vec<&str> = scope.split('/').collect();
    match parts.as_slice() {
        [_, tenant, instant] => format!("{tenant} {instant}"),
        _ => scope.to_string(),
    }
}

#[derive(Clone)]
pub struct State {
    facts: Arc<Mutex<Facts>>,
    tx: watch::Sender<String>,
}

impl State {
    pub fn new() -> (Self, watch::Receiver<String>) {
        let facts = Facts {
            signed_in: crate::auth::current_session().is_some(),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(facts.line());
        (
            Self {
                facts: Arc::new(Mutex::new(facts)),
                tx,
            },
            rx,
        )
    }

    pub fn update(&self, f: impl FnOnce(&mut Facts)) {
        let line = {
            let mut facts = self.facts.lock().expect("state lock");
            f(&mut facts);
            facts.line()
        };
        self.tx.send_if_modified(|current| {
            if *current == line {
                false
            } else {
                *current = line;
                true
            }
        });
    }

    pub fn snapshot(&self) -> Facts {
        self.facts.lock().expect("state lock").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_reads_the_facts_in_priority_order() {
        let mut f = Facts::default();
        assert_eq!(f.line(), "Signed out");
        f.signed_in = true;
        assert_eq!(f.line(), "Waiting for DCS");
        f.spooled = 2;
        assert_eq!(f.line(), "Waiting for DCS · 2 minutes spooled");
        f.exporter_connected = true;
        assert_eq!(f.line(), "Connected, no aircraft");
        f.pilot = Some("Airboss".into());
        assert_eq!(f.line(), "Spooling Airboss, no live recording (2 minutes)");
        f.live_recording = Some("202610/shadowlands/20261028_192125".into());
        assert_eq!(f.line(), "Pushing Airboss to shadowlands 20261028_192125");
        f.sign_in_needed = true;
        assert_eq!(f.line(), "Sign in needed");
    }
}
