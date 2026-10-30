//! Tacview real-time telemetry client for the pilot's OWN exporter on
//! `localhost`. The same protocol Wolf2 Desktop speaks to the server's
//! exporter (`desktop/crates/recording/src/acmi.rs`): a fixed handshake, then
//! ACMI 2.x text, one record per line.
//!
//! Lessons carried over from that recorder, each of which was a broken
//! recording first:
//! - a trailing partial line at disconnect is DISCARDED, never emitted — the
//!   socket closes mid-record on every reconnect, and a `T=` with the wrong
//!   field count poisons whatever consumes it;
//! - a silent socket is logged, not acted on: a paused mission is legitimate;
//! - reconnect with a fixed 5 s backoff, forever — DCS starts and stops around
//!   the tray.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const HANDSHAKE: &[u8] = b"XtraLib.Stream.0\nTacview.RealTimeTelemetry.0\nHost wolf2-tray\n\0";
pub const RETRY_DELAY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const QUIET_WARN: Duration = Duration::from_secs(30);
/// A line longer than this is not telemetry; drop the connection's buffer
/// rather than grow without bound.
const MAX_LINE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The connection's header is complete: `#` frames follow. `reference_time`
    /// is the `0,ReferenceTime=` value verbatim (ISO-8601), `data_source` the
    /// `0,DataSource=` value verbatim.
    Header {
        reference_time: String,
        data_source: String,
    },
    /// A `#` line: mission seconds since `ReferenceTime`.
    Frame {
        seconds: f64,
    },
    /// An object line. `props` are the `k=v` pairs after the id with ACMI
    /// unescaping applied; `raw` is the line exactly as received (what a
    /// segment writes).
    Object {
        id: String,
        props: Vec<(String, String)>,
        raw: String,
    },
    /// `-id`: the object is gone.
    Removed {
        id: String,
    },
    Connected,
    Disconnected,
}

/// Parse one normalised line. `None` for lines that carry nothing (empty,
/// handshake echo, unknown shape). Header globals accumulate in `header`
/// until the first `#`, when the caller emits `Event::Header`.
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim_matches(|c| c == '\r' || c == '\n' || c == '\0' || c == '\u{feff}');
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix('#') {
        return rest.trim().parse::<f64>().ok().map(Line::Frame);
    }
    if let Some(rest) = line.strip_prefix('-') {
        let id = rest.trim();
        return (!id.is_empty()).then(|| Line::Removed(id.to_ascii_lowercase()));
    }
    if let Some((k, v)) = line.split_once('=')
        && !k.contains(',')
    {
        return Some(Line::Global(k.to_string(), v.to_string()));
    }
    let (id, rest) = line.split_once(',')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let props = split_props(rest)
        .into_iter()
        .filter_map(|field| {
            let (k, v) = field.split_once('=')?;
            Some((k.to_string(), unescape(v)))
        })
        .collect();
    if id == "0" {
        // `0,Key=Value` globals.
        let mut globals = Vec::new();
        for field in split_props(rest) {
            if let Some((k, v)) = field.split_once('=') {
                globals.push((k.to_string(), unescape(v)));
            }
        }
        return Some(Line::Globals(globals));
    }
    Some(Line::Object {
        id: id.to_ascii_lowercase(),
        props,
        raw: line.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Frame(f64),
    Removed(String),
    /// `Key=Value` without a leading id (FileType, FileVersion).
    Global(String, String),
    /// `0,Key=Value,...`.
    Globals(Vec<(String, String)>),
    Object {
        id: String,
        props: Vec<(String, String)>,
        raw: String,
    },
}

/// Split `k=v,k=v` on commas that are not escaped (`\,`).
pub fn split_props(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b',' => {
                out.push(&s[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&s[start..]);
    out
}

/// ACMI escaping: `\,` is a comma, `\\` a backslash, `\n` a newline.
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(',') => out.push(','),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Run the client until `events` closes: connect, handshake, stream, reconnect.
pub async fn run(port: u16, events: mpsc::Sender<Event>) {
    let address = format!("127.0.0.1:{port}");
    loop {
        if events.is_closed() {
            return;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
            Ok(Ok(mut stream)) => {
                tracing::info!(address, "exporter connected");
                if let Err(e) = stream.write_all(HANDSHAKE).await {
                    tracing::warn!(address, "handshake failed: {e}");
                } else {
                    let _ = events.send(Event::Connected).await;
                    stream_lines(&mut stream, &events).await;
                    let _ = events.send(Event::Disconnected).await;
                }
            }
            Ok(Err(e)) => tracing::debug!(address, "exporter not reachable: {e}"),
            Err(_) => tracing::debug!(address, "exporter connect timed out"),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn stream_lines(stream: &mut TcpStream, events: &mpsc::Sender<Event>) {
    let mut buf = [0u8; 8192];
    let mut line_buf: Vec<u8> = Vec::new();
    let mut header: Option<HeaderAccumulator> = Some(HeaderAccumulator::default());
    let mut last_byte = tokio::time::Instant::now();
    let mut quiet_warned = false;
    loop {
        if events.is_closed() {
            return;
        }
        match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                tracing::info!("exporter closed the socket");
                break;
            }
            Ok(Ok(n)) => {
                if quiet_warned {
                    tracing::info!("exporter feed resumed");
                    quiet_warned = false;
                }
                last_byte = tokio::time::Instant::now();
                line_buf.extend_from_slice(&buf[..n]);
                if line_buf.len() > MAX_LINE {
                    tracing::warn!("exporter line exceeds {MAX_LINE} bytes; dropping buffer");
                    line_buf.clear();
                    continue;
                }
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = line_buf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&raw);
                    let Some(line) = parse_line(&text) else {
                        continue;
                    };
                    if !emit(line, &mut header, events).await {
                        return;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("exporter read error: {e}");
                break;
            }
            Err(_) => {
                if !quiet_warned && last_byte.elapsed() >= QUIET_WARN {
                    tracing::warn!("exporter feed quiet for {}s", last_byte.elapsed().as_secs());
                    quiet_warned = true;
                }
            }
        }
    }
    // The trailing partial line is the FRONT of a record cut mid-stream:
    // worthless, and poisonous if written. Dropped.
    if !line_buf.is_empty() {
        tracing::debug!(
            bytes = line_buf.len(),
            "discarding partial line at disconnect"
        );
        line_buf.clear();
    }
}

#[derive(Default)]
struct HeaderAccumulator {
    reference_time: Option<String>,
    data_source: Option<String>,
}

/// Returns false when the receiver is gone.
async fn emit(
    line: Line,
    header: &mut Option<HeaderAccumulator>,
    events: &mpsc::Sender<Event>,
) -> bool {
    let event = match line {
        Line::Global(_, _) => return true,
        Line::Globals(globals) => {
            if let Some(h) = header.as_mut() {
                for (k, v) in globals {
                    match k.as_str() {
                        "ReferenceTime" => h.reference_time = Some(v),
                        "DataSource" => h.data_source = Some(v),
                        _ => {}
                    }
                }
            }
            return true;
        }
        Line::Frame(seconds) => {
            if let Some(h) = header.take() {
                let ok = events
                    .send(Event::Header {
                        reference_time: h.reference_time.unwrap_or_default(),
                        data_source: h.data_source.unwrap_or_default(),
                    })
                    .await
                    .is_ok();
                if !ok {
                    return false;
                }
            }
            Event::Frame { seconds }
        }
        Line::Removed(id) => Event::Removed { id },
        Line::Object { id, props, raw } => Event::Object { id, props, raw },
    };
    events.send(event).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_frames_objects_and_removals() {
        assert_eq!(
            parse_line("\u{feff}FileType=text/acmi/tacview\r\n"),
            Some(Line::Global("FileType".into(), "text/acmi/tacview".into()))
        );
        assert_eq!(
            parse_line("0,ReferenceTime=2026-10-28T19:00:00Z,DataSource=DCS 2.9"),
            Some(Line::Globals(vec![
                ("ReferenceTime".into(), "2026-10-28T19:00:00Z".into()),
                ("DataSource".into(), "DCS 2.9".into()),
            ]))
        );
        assert_eq!(parse_line("#12.5"), Some(Line::Frame(12.5)));
        assert_eq!(parse_line("-A02"), Some(Line::Removed("a02".into())));
        let Some(Line::Object { id, props, raw }) =
            parse_line("A02,T=1|2|3,Name=F-16C\\, Block 50,Pilot=Airboss")
        else {
            panic!("expected object");
        };
        assert_eq!(id, "a02");
        assert_eq!(
            props[1],
            ("Name".to_string(), "F-16C, Block 50".to_string())
        );
        assert_eq!(props[2], ("Pilot".to_string(), "Airboss".to_string()));
        assert!(raw.starts_with("A02,T="));
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("garbage without structure"), None);
    }

    #[test]
    fn split_respects_escaped_commas() {
        assert_eq!(split_props("a=1\\,2,b=3"), vec!["a=1\\,2", "b=3"]);
        assert_eq!(unescape("x\\\\y\\,z"), "x\\y,z");
    }
}
