//! Minute segments (spec §5): one gzipped ACMI text file per mission-clock
//! minute, holding the tray's header and the ownship's frames.
//!
//! `minute = floor(missionMs / 60000)` with `missionMs = ReferenceTime (epoch
//! ms) + # × 1000` — the same label the viewer's `SimClock.missionMsFor`
//! computes for the server recording, which is what lets a reader find a
//! segment from the playhead.
//!
//! The first frame of every segment restates every static property the
//! exporter has sent for the object so far (merged into the object's first
//! line), so a segment is self-contained like a server snapshot chunk.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

pub const FILE_TYPE_LINE: &str = "FileType=text/acmi/tacview";

/// What the exporter told us at connection start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamHeader {
    /// ISO-8601 as the exporter wrote it.
    pub reference_time: String,
    pub data_source: String,
    /// epoch ms of `reference_time`.
    pub reference_ms: i64,
}

impl StreamHeader {
    pub fn parse(reference_time: &str, data_source: &str) -> Option<Self> {
        let reference_ms = parse_reference_time(reference_time)?;
        Some(Self {
            reference_time: reference_time.to_string(),
            data_source: data_source.to_string(),
            reference_ms,
        })
    }
}

/// ACMI `ReferenceTime` is ISO-8601, normally `2026-10-28T19:00:00Z`; Tacview
/// also writes fractional seconds. chrono's RFC 3339 parser covers both.
pub fn parse_reference_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp_millis());
    }
    // A bare date-time without zone: Tacview means UTC.
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|t| t.and_utc().timestamp_millis())
}

pub fn mission_ms(reference_ms: i64, seconds: f64) -> i64 {
    reference_ms + (seconds * 1000.0).round() as i64
}

pub fn minute_of(mission_ms: i64) -> i64 {
    mission_ms.div_euclid(60_000)
}

/// A finished segment, ready to spool or send.
#[derive(Debug, Clone)]
pub struct Segment {
    pub minute: i64,
    pub pilot: String,
    pub object_id: String,
    pub gz: Vec<u8>,
    pub sha256: String,
    pub frames: usize,
}

/// Accumulates one minute of the ownship's lines and seals it.
pub struct SegmentWriter {
    object_id: String,
    minute: i64,
    text: String,
    frames: usize,
    /// The ownship's first line of this segment still needs the statics merged.
    restate_pending: bool,
    pending_frame: Option<f64>,
}

impl SegmentWriter {
    pub fn new(
        header: StreamHeader,
        edition: &str,
        object_id: &str,
        pilot: &str,
        minute: i64,
    ) -> Self {
        let mut text = String::with_capacity(64 * 1024);
        text.push_str(FILE_TYPE_LINE);
        text.push('\n');
        text.push_str("FileVersion=2.2\n");
        text.push_str(&format!("0,ReferenceTime={}\n", header.reference_time));
        if !header.data_source.is_empty() {
            text.push_str(&format!("0,DataSource={}\n", escape(&header.data_source)));
        }
        text.push_str(&format!("0,DataRecorder=Wolf2 Tray {edition}\n"));
        text.push_str(&format!("0,OwnshipId={object_id}\n"));
        text.push_str(&format!("0,OwnshipPilot={}\n", escape(pilot)));
        Self {
            object_id: object_id.to_string(),
            minute,
            text,
            frames: 0,
            restate_pending: true,
            pending_frame: None,
        }
    }

    pub fn minute(&self) -> i64 {
        self.minute
    }

    /// A `#` frame: written lazily, only once an ownship line follows it, so
    /// frames in which only other objects moved leave no trace.
    pub fn frame(&mut self, seconds: f64) {
        self.pending_frame = Some(seconds);
    }

    /// An ownship line, raw as received. `statics` is the accumulated static
    /// property set, merged into the first line of the segment.
    pub fn object_line(&mut self, raw: &str, statics: &[(String, String)]) {
        if let Some(seconds) = self.pending_frame.take() {
            self.text.push_str(&format!("#{}\n", trim_float(seconds)));
            self.frames += 1;
        }
        let line = normalise_id(raw, &self.object_id);
        if self.restate_pending {
            self.restate_pending = false;
            self.text.push_str(&merge_statics(&line, statics));
        } else {
            self.text.push_str(&line);
        }
        self.text.push('\n');
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    pub fn finish(self, pilot: &str) -> Segment {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(self.text.as_bytes()).expect("gzip to memory");
        let gz = enc.finish().expect("gzip to memory");
        let sha256 = hex::encode(Sha256::digest(&gz));
        Segment {
            minute: self.minute,
            pilot: pilot.to_string(),
            object_id: self.object_id,
            gz,
            sha256,
            frames: self.frames,
        }
    }
}

/// The exporter may write the id in upper-case hex; the segment uses the
/// lower-case form everywhere (`OwnshipId` and every line).
fn normalise_id(raw: &str, object_id: &str) -> String {
    match raw.split_once(',') {
        Some((id, rest)) if id.eq_ignore_ascii_case(object_id) => format!("{object_id},{rest}"),
        _ => raw.to_string(),
    }
}

/// Append every static the line doesn't already carry.
fn merge_statics(line: &str, statics: &[(String, String)]) -> String {
    let present: Vec<&str> = crate::exporter::split_props(line)
        .into_iter()
        .skip(1)
        .filter_map(|f| f.split_once('=').map(|(k, _)| k))
        .collect();
    let mut out = line.to_string();
    for (k, v) in statics {
        if !present.contains(&k.as_str()) {
            out.push(',');
            out.push_str(k);
            out.push('=');
            out.push_str(&escape(v));
        }
    }
    out
}

pub fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            ',' => out.push_str("\\,"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

fn trim_float(seconds: f64) -> String {
    let s = format!("{seconds:.3}");
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

/// Inflate and check a segment the way the route will (spec §5): first line,
/// `OwnshipId` present, every object line carries it. Returns (object id,
/// pilot, line count).
pub fn validate(gz: &[u8]) -> Result<(String, String, usize), String> {
    use std::io::Read;
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut text)
        .map_err(|e| format!("not gzip: {e}"))?;
    let mut lines = text.lines();
    if lines.next() != Some(FILE_TYPE_LINE) {
        return Err("first line is not the ACMI FileType".into());
    }
    let mut object_id = None;
    let mut pilot = String::new();
    let mut count = 1;
    for line in lines {
        count += 1;
        if let Some(v) = line.strip_prefix("0,OwnshipId=") {
            object_id = Some(v.to_string());
            continue;
        }
        if let Some(v) = line.strip_prefix("0,OwnshipPilot=") {
            pilot = crate::exporter::unescape(v);
            continue;
        }
        if line.starts_with('#') || line.starts_with("0,") || !line.contains(',') {
            continue;
        }
        let id = line.split_once(',').map(|(id, _)| id).unwrap_or("");
        if Some(id) != object_id.as_deref() {
            return Err(format!("line {count}: object id {id:?} is not the ownship"));
        }
    }
    let object_id = object_id.ok_or("no OwnshipId line")?;
    Ok((object_id, pilot, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> StreamHeader {
        StreamHeader::parse("2026-10-28T19:00:00Z", "DCS 2.9.30.28536").unwrap()
    }

    fn statics() -> Vec<(String, String)> {
        [
            ("Type", "Air+FixedWing"),
            ("Name", "F-16C_50"),
            ("Pilot", "Airboss"),
            ("Group", "Viper, 1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn minute_math_matches_the_viewers_mission_label() {
        let h = header();
        assert_eq!(h.reference_ms, 1_793_214_000_000);
        let m = mission_ms(h.reference_ms, 1234.56);
        assert_eq!(m, 1_793_214_000_000 + 1_234_560);
        assert_eq!(minute_of(m), 29_886_920);
        assert_eq!(
            parse_reference_time("2026-10-28T19:00:00.5Z"),
            Some(1_793_214_000_500)
        );
        assert_eq!(
            parse_reference_time("2026-10-28T19:00:00"),
            Some(1_793_214_000_000)
        );
        assert_eq!(parse_reference_time("nope"), None);
    }

    #[test]
    fn golden_segment() {
        let mut w = SegmentWriter::new(header(), "0.1.0", "a02", "Airboss", 29_886_920);
        w.frame(1234.56);
        w.object_line("A02,T=1.5|2.5|300,AOA=3.1,Pilot=Airboss", &statics());
        w.frame(1234.76);
        w.object_line("A02,T=1.5|2.6|301", &statics());
        w.frame(1234.96); // nothing from the ownship in this frame
        let seg = w.finish("Airboss");
        let (id, pilot, _) = validate(&seg.gz).unwrap();
        assert_eq!((id.as_str(), pilot.as_str()), ("a02", "Airboss"));
        use std::io::Read;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&seg.gz[..])
            .read_to_string(&mut text)
            .unwrap();
        let expected = "FileType=text/acmi/tacview\n\
FileVersion=2.2\n\
0,ReferenceTime=2026-10-28T19:00:00Z\n\
0,DataSource=DCS 2.9.30.28536\n\
0,DataRecorder=Wolf2 Tray 0.1.0\n\
0,OwnshipId=a02\n\
0,OwnshipPilot=Airboss\n\
#1234.56\n\
a02,T=1.5|2.5|300,AOA=3.1,Pilot=Airboss,Type=Air+FixedWing,Name=F-16C_50,Group=Viper\\, 1\n\
#1234.76\n\
a02,T=1.5|2.6|301\n";
        assert_eq!(text, expected);
        assert_eq!(seg.frames, 2);
        assert_eq!(seg.sha256.len(), 64);
        assert_eq!(seg.minute, 29_886_920);
    }

    #[test]
    fn validate_rejects_foreign_objects_and_bad_headers() {
        let mut w = SegmentWriter::new(header(), "0.1.0", "a02", "Me", 1);
        w.frame(1.0);
        w.object_line("a02,T=1", &[]);
        w.object_line("b03,T=1", &[]); // the writer trusts its caller; the validator does not
        let seg = w.finish("Me");
        assert!(validate(&seg.gz).unwrap_err().contains("b03"));

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"FileVersion=2.2\n").unwrap();
        assert!(
            validate(&enc.finish().unwrap())
                .unwrap_err()
                .contains("FileType")
        );
        assert!(validate(b"not gzip").is_err());
    }

    #[test]
    fn empty_segment_is_detectable() {
        let w = SegmentWriter::new(header(), "0.1.0", "a02", "Me", 1);
        assert!(w.is_empty());
    }
}
