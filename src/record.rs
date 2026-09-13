//! Recording layout and retention.
//!
//! One directory per codex session, one file set per request:
//!
//! ```text
//! <log_dir>/<session-uuid>/260912-231425-user-r0001.req.hdr
//!                         260912-231425-user-r0001.req.out.hdr
//!                         260912-231425-user-r0001.req.body      (raw, only when compressed)
//!                         260912-231425-user-r0001.req.json      (decoded)
//!                         260912-231425-user-r0001.req.out.json  (only when rewritten)
//!                         260912-231425-user-r0001.req.summary.json
//!                         260912-231425-user-r0001.resp.hdr
//!                         260912-231425-user-r0001.resp.stream.sse
//!                         260912-231425-user-r0001.resp.summary.json
//! <log_dir>/_nosession/...                                            (no session-id header)
//! ```
//!
//! The directory name is the **codex session uuid**, so it lines up with the session-capture files
//! (`sessions/ss-<date>-<uuid>-<title>.jsonl`). `user|system` is the turn's thread source (the
//! TITLE/RECAP helpers run as system threads), `rNNNN` is the per-session request number, reused by
//! the matching response. Empty files are never created.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::config::{LimitsSection, RecordSection};
use crate::timeutil;

/// Maximum number of sessions whose directory + counter we remember in memory.
const MAX_TRACKED_SESSIONS: usize = 4096;

#[derive(Clone)]
pub struct Plan {
    dir: PathBuf,
    stem: String,
}

impl Plan {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn stem(&self) -> &str {
        &self.stem
    }

    pub fn req(&self, role: &str) -> PathBuf {
        self.dir.join(self.req_name(role))
    }

    pub fn resp(&self, role: &str) -> PathBuf {
        self.dir.join(self.resp_name(role))
    }

    pub fn req_name(&self, role: &str) -> String {
        format!("{}.req.{role}", self.stem)
    }

    pub fn resp_name(&self, role: &str) -> String {
        format!("{}.resp.{role}", self.stem)
    }
}

struct SessionState {
    dir: PathBuf,
    next_seq: u32,
    last_seen_ms: u128,
}

pub struct Recorder {
    root: PathBuf,
    pub record: RecordSection,
    #[allow(dead_code)]
    pub limits: LimitsSection,
    state: Mutex<HashMap<String, SessionState>>,
}

impl Recorder {
    pub fn new(root: PathBuf, record: RecordSection, limits: LimitsSection) -> Self {
        Self {
            root,
            record,
            limits,
            state: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.record.enabled
    }


    /// `true` when a request body has to be decompressed: recording the decoded body or its summary,
    /// or producing a rewritten body.
    pub fn needs_request_decode(&self) -> bool {
        self.enabled()
            && (self.record.request_body_json
                || self.record.request_summary
                || self.record.request_body_raw
                || self.record.request_body_out)
    }

    /// `true` when the response body has to be inspected at all.
    pub fn tracks_response(&self) -> bool {
        self.enabled() && (self.record.response_stream || self.record.response_summary)
    }

    pub fn dir_for(&self, session: Option<&str>) -> PathBuf {
        match session {
            Some(s) if !s.is_empty() => self.root.join(sanitize(s)),
            _ => self.root.join("_nosession"),
        }
    }

    /// Allocates the next request number for this session and returns the naming plan.
    pub fn plan(&self, session: Option<&str>, thread: &str) -> Plan {
        let key = session
            .filter(|s| !s.is_empty())
            .unwrap_or("_nosession")
            .to_owned();
        let mut state = self.state.lock().expect("recorder state");
        if state.len() >= MAX_TRACKED_SESSIONS && !state.contains_key(&key) {
            // Evict the least recently used half so long runs cannot grow without bound.
            let mut seen: Vec<(String, u128)> =
                state.iter().map(|(k, v)| (k.clone(), v.last_seen_ms)).collect();
            seen.sort_by_key(|(_, t)| *t);
            for (k, _) in seen.into_iter().take(MAX_TRACKED_SESSIONS / 2) {
                state.remove(&k);
            }
        }
        let dir = self.dir_for(session);
        let now = timeutil::now_ms();
        let entry = state.entry(key).or_insert_with(|| {
            let _ = fs::create_dir_all(&dir);
            SessionState {
                dir: dir.clone(),
                next_seq: 1,
                last_seen_ms: now,
            }
        });
        entry.last_seen_ms = now;
        let seq = entry.next_seq;
        entry.next_seq += 1;
        let stem = format!("{}-{}-r{:04}", timeutil::stamp(), thread_slug(thread), seq);
        Plan {
            dir: entry.dir.clone(),
            stem,
        }
    }

    /// Deletes files past `retention_days`, gzips files past `gzip_after_days`, and drops the
    /// oldest files until the tree fits into `max_total_bytes`.
    pub fn prune(&self) -> String {
        let mut removed = 0u64;
        let mut freed = 0u64;
        let mut gzipped = 0u64;
        let now = SystemTime::now();

        let mut files = Vec::new();
        collect(&self.root, &mut files);

        if let Some(days) = self.record.retention_days {
            let cutoff = Duration::from_secs(days.saturating_mul(86_400));
            for (path, size, mtime) in &files {
                if now.duration_since(*mtime).map(|d| d > cutoff).unwrap_or(false)
                    && fs::remove_file(path).is_ok()
                {
                    removed += 1;
                    freed += size;
                }
            }
            files.retain(|(p, _, _)| p.exists());
        }

        if let Some(days) = self.record.gzip_after_days {
            let cutoff = Duration::from_secs(days.saturating_mul(86_400));
            for (path, _, mtime) in &files {
                if path.extension().map(|e| e == "gz").unwrap_or(false) {
                    continue;
                }
                if now.duration_since(*mtime).map(|d| d > cutoff).unwrap_or(false)
                    && std::process::Command::new("gzip")
                        .arg("-f")
                        .arg(path)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                {
                    gzipped += 1;
                }
            }
            files.clear();
            collect(&self.root, &mut files);
        }

        if let Some(max) = self.record.max_total_bytes {
            let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
            files.sort_by_key(|(_, _, mtime)| *mtime);
            for (path, size, _) in &files {
                if total <= max {
                    break;
                }
                if fs::remove_file(path).is_ok() {
                    total -= size;
                    removed += 1;
                    freed += size;
                }
            }
        }

        format!("retention: removed {removed} file(s) ({freed} bytes), gzipped {gzipped}")
    }
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect(&path, out);
        } else {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((path, meta.len(), mtime));
        }
    }
}

pub fn thread_slug(thread: &str) -> String {
    let lower = thread.to_ascii_lowercase();
    if lower.contains("system") {
        "system".to_owned()
    } else if lower.contains("user") {
        "user".to_owned()
    } else {
        "user".to_owned()
    }
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

/// Reads `thread_source` out of the `x-codex-turn-metadata` header value.
pub fn thread_from_metadata(metadata: Option<&str>) -> String {
    let Some(raw) = metadata else {
        return "user".to_owned();
    };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => v
            .get("thread_source")
            .and_then(|s| s.as_str())
            .unwrap_or("user")
            .to_owned(),
        Err(_) => "user".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder(record: RecordSection, limits: LimitsSection) -> Recorder {
        Recorder::new(std::env::temp_dir().join("codex-rec-test-layout"), record, limits)
    }

    #[test]
    fn plan_names_carry_session_dir_thread_and_sequence() {
        let rec = recorder(RecordSection::default(), LimitsSection::default());
        let p1 = rec.plan(Some("01a097d1-19b4-79d2-a2a2-fb5dc004dbae"), "user");
        let p2 = rec.plan(Some("01a097d1-19b4-79d2-a2a2-fb5dc004dbae"), "system");
        let p3 = rec.plan(None, "user");
        assert!(p1.dir().to_string_lossy().ends_with("01a097d1-19b4-79d2-a2a2-fb5dc004dbae"));
        assert!(p3.dir().to_string_lossy().ends_with("_nosession"));
        assert!(p1.stem().ends_with("-user-r0001"), "{}", p1.stem());
        assert!(p2.stem().ends_with("-system-r0002"), "{}", p2.stem());
        assert!(p1
            .req("out.hdr")
            .to_string_lossy()
            .ends_with(&format!("{}.req.out.hdr", p1.stem())));
        assert!(p1
            .resp("stream.sse")
            .to_string_lossy()
            .ends_with(&format!("{}.resp.stream.sse", p1.stem())));
    }

    #[test]
    fn plan_is_deterministic_per_session_and_dirs_created() {
        let rec = recorder(RecordSection::default(), LimitsSection::default());
        let a = rec.plan(Some("sess-a"), "user");
        let b = rec.plan(Some("sess-a"), "user");
        assert_eq!(a.dir(), b.dir());
        assert!(a.dir().is_dir());
        // sequence numbers keep counting and file names sort chronologically
        assert_ne!(a.stem(), b.stem());
    }

    #[test]
    fn thread_metadata_parsing() {
        assert_eq!(thread_from_metadata(None), "user");
        assert_eq!(thread_from_metadata(Some("not json")), "user");
        assert_eq!(
            thread_from_metadata(Some(r#"{"thread_source":"system","sandbox_mode":"read-only"}"#)),
            "system"
        );
        assert_eq!(thread_slug("system"), "system");
        assert_eq!(thread_slug("SYSTEM"), "system");
        assert_eq!(thread_slug("user"), "user");
    }

    #[test]
    fn prune_removes_by_age_and_size() {
        let dir = std::env::temp_dir().join("codex-rec-test-prune");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sess")).unwrap();
        for i in 0..5 {
            fs::write(dir.join("sess").join(format!("f{i}.json")), vec![b'x'; 100]).unwrap();
        }
        let mut record = RecordSection::default();
        record.max_total_bytes = Some(250);
        let rec = Recorder::new(dir.clone(), record, LimitsSection::default());
        let report = rec.prune();
        let left: u64 = fs::read_dir(dir.join("sess")).unwrap().count() as u64;
        assert!(left <= 3, "left {left}: {report}");
        let _ = fs::remove_dir_all(&dir);
    }
}
