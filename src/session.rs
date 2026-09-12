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
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub struct Sessions {
    dir: PathBuf,
    titles: Mutex<HashMap<String, String>>,
}

impl Sessions {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            titles: Mutex::new(HashMap::new()),
        }
    }

    /// Remembers the title for a session (first non-empty title wins).
    pub fn note_title(&self, session: &str, title: &str) {
        let slug = slugify(title);
        if slug.is_empty() {
            return;
        }
        if let Ok(mut map) = self.titles.lock() {
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

    /// The file this session writes to (created lazily on the first append).
    pub fn path(&self, session: &str) -> PathBuf {
        let title = self
            .titles
            .lock()
            .ok()
            .and_then(|map| map.get(session).cloned())
            .unwrap_or_else(|| "untitled".to_owned());
        let id = sanitize(session);
        self.dir.join(format!("ss-{}-{}-{}.jsonl", today(), id, title))
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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch -> (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn date_conversion_matches_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
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
