//! Per-session capture: one JSONL file per codex session under `session_dir`,
//! named `ss-<date>-<session-uuid>-<title>.jsonl`.
//!
//! Every line is one JSON object:
//!   * `{"type":"request",  "ts":…, "uri":…, "persona":[…], "request":{…}}`
//!   * `{"type":"response", "ts":…, "status":…, "origin":…, "summary":{…}}`
//!
//! The title comes from the first user message of the session (slugified, capped at 40 chars);
//! the file name is fixed the first time a session is written.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use crate::timeutil;

const MAX_TITLES: usize = 4096;

pub struct Sessions {
    dir: PathBuf,
    titles: Mutex<HashMap<String, String>>,
    /// The file name is pinned the first time a session is written, so learning the title later
    /// does not scatter one session across two files.
    paths: Mutex<HashMap<String, PathBuf>>,
}

impl Sessions {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            titles: Mutex::new(HashMap::new()),
            paths: Mutex::new(HashMap::new()),
        }
    }

    /// Remembers the title for a session (first non-empty title wins).
    ///
    /// The map is capped so a long-running recorder cannot grow without bound: once it is full the
    /// oldest half of the entries is dropped (a stale title only affects the file name of a session
    /// that has not been written to in a very long time).
    pub fn note_title(&self, session: &str, title: &str) {
        let slug = slugify(title);
        if slug.is_empty() {
            return;
        }
        if let Ok(mut map) = self.titles.lock() {
            if map.len() >= MAX_TITLES && !map.contains_key(session) {
                let keys: Vec<String> = map.keys().take(MAX_TITLES / 2).cloned().collect();
                for key in keys {
                    map.remove(&key);
                }
            }
            map.entry(session.to_owned()).or_insert(slug);
        }
    }

    /// Appends one event line to this session's file.
    pub fn append(&self, session: &str, event: &Value) {
        let path = self.path(session);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{event}");
        }
    }

    /// The file this session writes to. The name is decided on first use (title included when it is
    /// already known) and then kept for the lifetime of the process.
    pub fn path(&self, session: &str) -> PathBuf {
        if let Ok(map) = self.paths.lock() {
            if let Some(existing) = map.get(session) {
                return existing.clone();
            }
        }
        let title = self
            .titles
            .lock()
            .ok()
            .and_then(|map| map.get(session).cloned())
            .unwrap_or_else(|| "untitled".to_owned());
        let path = self
            .dir
            .join(format!("ss-{}-{}-{}.jsonl", today(), sanitize(session), title));
        if let Ok(mut map) = self.paths.lock() {
            if map.len() >= MAX_TITLES && !map.contains_key(session) {
                let keys: Vec<String> = map.keys().take(MAX_TITLES / 2).cloned().collect();
                for key in keys {
                    map.remove(&key);
                }
            }
            map.entry(session.to_owned()).or_insert(path.clone());
        }
        path
    }
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    out.trim_matches('-').to_owned()
}

/// UTC date as `YYYY-MM-DD`.
fn today() -> String {
    let (year, month, day) = timeutil::civil_from_days((timeutil::now_ms() / 1000 / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_file_name_is_pinned_on_first_write() {
        let sessions = Sessions::new(std::env::temp_dir().join("codex-rec-test-pin"));
        let first = sessions.path("sess-pin");
        sessions.note_title("sess-pin", "A Real Title");
        let second = sessions.path("sess-pin");
        assert_eq!(first, second, "the file must not move once a session has been written");
        assert!(first.to_string_lossy().contains("untitled"), "{first:?}");
        // a title that is already known when the session is first written *does* name the file
        sessions.note_title("sess-early", "Known Early");
        assert!(sessions.path("sess-early").to_string_lossy().contains("known-early"));
    }

    #[test]
    fn date_conversion_matches_known_days() {
        assert_eq!(timeutil::civil_from_days(0), (1970, 1, 1));
        assert_eq!(timeutil::civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn slug_and_file_name() {
        assert_eq!(slugify("Hello, World! This is a test"), "hello-world-this-is-a-test");
        let sessions = Sessions::new(std::env::temp_dir().join("codex-rec-test-sessions"));
        sessions.note_title("01a0-1234", "Explain the dreamer lease");
        let path = sessions.path("01a0-1234");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("ss-"), "{name}");
        assert!(name.ends_with("-01a0-1234-explain-the-dreamer-lease.jsonl"), "{name}");
        sessions.append("01a0-1234", &json!({"type": "request"}));
        assert!(path.is_file());
    }
}
