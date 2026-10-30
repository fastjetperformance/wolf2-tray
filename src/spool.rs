//! The on-disk spool (spec §5): `%LOCALAPPDATA%\Wolf2 Tray\spool\<session>\
//! <minute>.acmi.gz` with a `<minute>.meta.json` sidecar. Every finished
//! segment lands here first and leaves only after the site has it (or has
//! refused it for good). Entries older than 24 h are deleted.

use std::path::{Path, PathBuf};

use crate::segment::Segment;

pub const TTL_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub v: u8,
    pub session: String,
    pub minute: i64,
    pub pilot: String,
    pub object_id: String,
    pub sha256: String,
    /// Wall clock (epoch ms) when the segment was sealed.
    pub received_at_ms: i64,
    pub frames: usize,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub meta: Meta,
    pub gz_path: PathBuf,
    pub meta_path: PathBuf,
}

pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn put(&self, session: &str, seg: &Segment, received_at_ms: i64) -> std::io::Result<Entry> {
        let sdir = self.dir.join(session);
        std::fs::create_dir_all(&sdir)?;
        let gz_path = sdir.join(format!("{}.acmi.gz", seg.minute));
        let meta_path = sdir.join(format!("{}.meta.json", seg.minute));
        let meta = Meta {
            v: 1,
            session: session.to_string(),
            minute: seg.minute,
            pilot: seg.pilot.clone(),
            object_id: seg.object_id.clone(),
            sha256: seg.sha256.clone(),
            received_at_ms,
            frames: seg.frames,
        };
        // Bytes first, sidecar last: a sidecar without bytes is never seen.
        std::fs::write(&gz_path, &seg.gz)?;
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
        Ok(Entry {
            meta,
            gz_path,
            meta_path,
        })
    }

    /// Every spooled entry, oldest minute first. Sidecars that don't parse or
    /// whose bytes are missing are removed on sight.
    pub fn list(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        let Ok(sessions) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for s in sessions.flatten() {
            let sdir = s.path();
            if !sdir.is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&sdir) else {
                continue;
            };
            for f in files.flatten() {
                let meta_path = f.path();
                let Some(name) = meta_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(stem) = name.strip_suffix(".meta.json") else {
                    continue;
                };
                let gz_path = sdir.join(format!("{stem}.acmi.gz"));
                let meta = std::fs::read(&meta_path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Meta>(&b).ok());
                match meta {
                    Some(meta) if gz_path.is_file() => out.push(Entry {
                        meta,
                        gz_path,
                        meta_path,
                    }),
                    _ => {
                        let _ = std::fs::remove_file(&meta_path);
                        let _ = std::fs::remove_file(&gz_path);
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.meta
                .minute
                .cmp(&b.meta.minute)
                .then_with(|| a.meta.session.cmp(&b.meta.session))
        });
        out
    }

    pub fn remove(&self, entry: &Entry) {
        let _ = std::fs::remove_file(&entry.gz_path);
        let _ = std::fs::remove_file(&entry.meta_path);
        if let Some(parent) = entry.gz_path.parent() {
            let _ = std::fs::remove_dir(parent); // only succeeds when empty
        }
    }

    /// Delete entries sealed more than `TTL_MS` before `now_ms`.
    pub fn expire(&self, now_ms: i64) -> usize {
        let mut n = 0;
        for e in self.list() {
            if now_ms - e.meta.received_at_ms > TTL_MS {
                self.remove(&e);
                n += 1;
            }
        }
        n
    }

    pub fn read(&self, entry: &Entry) -> std::io::Result<Vec<u8>> {
        std::fs::read(&entry.gz_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(minute: i64) -> Segment {
        Segment {
            minute,
            pilot: "Me".into(),
            object_id: "a02".into(),
            gz: vec![1, 2, 3],
            sha256: "ab".repeat(32),
            frames: 2,
        }
    }

    #[test]
    fn put_list_expire_remove() {
        let dir = std::env::temp_dir().join(format!("wolf2-tray-spool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let spool = Spool::new(dir.clone());
        spool.put("s1", &seg(10), 1_000).unwrap();
        spool.put("s1", &seg(9), 2_000).unwrap();
        spool.put("s2", &seg(11), TTL_MS + 5_000).unwrap();
        let all = spool.list();
        assert_eq!(
            all.iter().map(|e| e.meta.minute).collect::<Vec<_>>(),
            vec![9, 10, 11]
        );
        assert_eq!(spool.read(&all[0]).unwrap(), vec![1, 2, 3]);
        // A sidecar without bytes is dropped on sight.
        std::fs::remove_file(&all[1].gz_path).unwrap();
        assert_eq!(spool.list().len(), 2);
        assert_eq!(
            spool.expire(TTL_MS + 5_000),
            1,
            "minute 9 sealed at 2_000 is past TTL"
        );
        let left = spool.list();
        assert_eq!(left.len(), 1);
        spool.remove(&left[0]);
        assert!(spool.list().is_empty());
        assert!(!dir.join("s2").exists(), "empty session dir removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
