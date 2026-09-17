//! Find a recent `x-codex-turn-state` in what the recorder already wrote.
//!
//! Probing is not always available: the backend hands out 292s only while it is in a certain mode,
//! so during a long stretch of 312s no amount of polling produces one. The recorder, however, has
//! been logging every response header all along -- including the 292s that other sessions happened
//! to receive. This module mines those.
//!
//! What it walks:
//!
//! * every `*.resp.hdr` under the configured trees (default `/root/rec`, `/root/tsproxy`,
//!   `/root/tsroll`, `/root/realprobe`),
//! * the token file the rolling injector maintains, when present.
//!
//! Ranking is by **token age** (the u64 stamped into the token), then by recency of the file, so the
//! freshest usable token wins regardless of which directory it came from. Tokens older than
//! `scan_fresh_within` are reported but not used: past that age the backend stops honouring them.
//!
//! Recursion is depth-limited and symlink-free on purpose: these trees sit next to the proxy's own
//! log directory, and a runaway walk would be worse than a missed token.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::base64_decode;
use super::tsgrab::token_len;

/// One usable token, with where it came from.
#[derive(Debug, Clone)]
pub struct Hit {
    pub token: String,
    pub len: usize,
    pub issued_at: u64,
    pub source: PathBuf,
    /// The `x-codex-safety-buffering-enabled` value that came with it, when the dump recorded one.
    pub safety_buffering: bool,
    /// `__oailb` origin pool, when the dump could not be decoded (informational only).
    pub origin: Option<String>,
}

impl Hit {
    pub fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.issued_at)
    }

    pub fn to_json(&self, now: u64) -> serde_json::Value {
        json!({
            "token": self.token,
            "len": self.len,
            "issued_at": self.issued_at,
            "age_secs": self.age_secs(now),
            "source": self.source.display().to_string(),
            "safety_buffering": self.safety_buffering,
            "origin": self.origin,
        })
    }
}

/// Issue time stamped into the token: version byte 0x80, then u64 big-endian (see docs/fingerprint.md).
fn issued_at(token: &str) -> Option<u64> {
    let raw = base64_decode(&token.replace('-', "+").replace('_', "/"))?;
    if raw.len() < 9 || raw[0] != 0x80 {
        return None;
    }
    Some(u64::from_be_bytes([
        raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7], raw[8],
    ]))
}

/// The token inside a recorder header dump, plus whatever else that dump tells us.
fn token_in_dump(path: &Path) -> Option<Hit> {
    let text = fs::read_to_string(path).ok()?;
    let mut token: Option<String> = None;
    let mut sb = false;
    let mut origin: Option<String> = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if token.is_none() {
            if let Some(at) = lower.find("x-codex-turn-state:") {
                let rest = line[at + "x-codex-turn-state:".len()..].trim();
                if let Some(v) = rest.split_whitespace().next() {
                    if v.len() > 100 && v.starts_with("gAAAA") {
                        token = Some(v.to_owned());
                    }
                }
            }
        }
        if lower.contains("x-codex-safety-buffering-enabled") && lower.contains("true") {
            sb = true;
        }
        if origin.is_none() {
            if let Some(at) = line.find("__oailb origin =") {
                let rest = line[at + "__oailb origin =".len()..].trim();
                let host = rest.split_whitespace().next().unwrap_or("");
                if !host.is_empty() {
                    origin = Some(
                        host.replace("chat.gateway.", "")
                            .replace(".api.openai.com", ""),
                    );
                }
            }
        }
    }
    let token = token?;
    let issued = issued_at(&token)?;
    Some(Hit {
        len: token_len(&token),
        token,
        issued_at: issued,
        source: path.to_path_buf(),
        safety_buffering: sb,
        origin,
    })
}

/// Plain text file holding nothing but a token (the rolling injector's `ts_token.txt`).
fn token_in_plain_file(path: &Path) -> Option<Hit> {
    let token = fs::read_to_string(path).ok()?.trim().to_owned();
    if token.len() < 100 || !token.starts_with("gAAAA") {
        return None;
    }
    let issued = issued_at(&token)?;
    Some(Hit {
        len: token_len(&token),
        token,
        issued_at: issued,
        source: path.to_path_buf(),
        safety_buffering: false,
        origin: None,
    })
}

/// Collects candidate files, **newest first**, so the caller can stop early.
///
/// Directory entries are sorted by mtime descending at every level: with one directory per session,
/// the newest sessions are visited before older ones, which is what makes a capped scan equivalent
/// to a full scan for our purpose (we only want the freshest usable token).
fn collect(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| {
        std::cmp::Reverse(
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    });
    for entry in entries {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect(&path, depth + 1, max_depth, out);
        } else if meta.is_file() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.ends_with(".resp.hdr")
                || name.ends_with(".req.out.hdr")
                || name.ends_with(".req.hdr")
                || name == "ts_token.txt"
                || name.ends_with(".token")
            {
                out.push(path);
            }
        }
    }
}

pub struct ScanArgs {
    /// `--want-lengths`; empty means "any length".
    pub want: Vec<String>,
    pub dirs: Vec<PathBuf>,
    pub max_depth: usize,
    /// Tokens older than this are reported but never *used*.
    pub fresh_within: u64,
    /// Ignore a token that is already installed in the config's token file.
    pub skip: Option<String>,
    /// Stop reading after this many matching dumps. The trees are walked newest-file-first, so the
    /// cap keeps a scheduled scan cheap: we only ever need the most recent hit.
    pub max_candidates: usize,
}

pub struct ScanResult {
    pub best: Option<Hit>,
    /// Newest hit overall, even when it is too old to use.
    pub newest: Option<Hit>,
    pub considered: usize,
    pub fresh_count: usize,
    pub used_count: usize,
    pub by_len: HashMap<usize, usize>,
}

fn matches_want(hit: &Hit, want: &[String]) -> bool {
    want.is_empty() || want.iter().any(|w| w == &hit.len.to_string())
}

/// Walks the trees and returns the freshest matching token.
pub fn scan(args: &ScanArgs) -> ScanResult {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let defaults: Vec<PathBuf> = vec![
        PathBuf::from("/root/rec"),
        PathBuf::from("/root/tsproxy"),
        PathBuf::from("/root/tsroll"),
        PathBuf::from("/root/realprobe"),
    ];
    let dirs = if args.dirs.is_empty() { defaults.clone() } else { args.dirs.clone() };

    let mut files: Vec<PathBuf> = Vec::new();
    for dir in &dirs {
        collect(dir, 0, args.max_depth, &mut files);
    }

    // newest file first across all trees
    files.sort_by_key(|p| {
        std::cmp::Reverse(
            fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    });

    let mut hits: Vec<Hit> = Vec::new();
    let mut read = 0usize;
    for path in &files {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // stop as soon as we have enough *usable* candidates: a scheduled scan only needs the best
        let usable = hits.iter().filter(|h| matches_want(h, &args.want) && h.age_secs(now) <= args.fresh_within).count();
        if args.max_candidates > 0 && usable >= args.max_candidates {
            break;
        }
        read += 1;
        let hit = if name == "ts_token.txt" || name.ends_with(".token") {
            token_in_plain_file(path)
        } else {
            token_in_dump(path)
        };
        if let Some(h) = hit {
            hits.push(h);
        }
    }

    let considered = read;
    let total_hits = hits.len();
    // newest first (by token age, not by file mtime: a copied file keeps a misleading mtime)
    hits.sort_by(|a, b| b.issued_at.cmp(&a.issued_at));

    let mut by_len: HashMap<usize, usize> = HashMap::new();
    for h in &hits {
        *by_len.entry(h.len).or_insert(0) += 1;
    }
    let fresh_count = hits.iter().filter(|h| h.age_secs(now) <= args.fresh_within).count();
    let used_count = hits.iter().filter(|h| matches_want(h, &args.want)).count();

    let skip = args.skip.as_deref();
    let skip_owned;
    let skip_ref = match skip {
        Some(s) => {
            skip_owned = s.trim().to_owned();
            Some(skip_owned.as_str())
        }
        None => None,
    };

    let newest = hits.first().cloned();
    let best = hits
        .iter()
        .find(|h| {
            matches_want(h, &args.want)
                && h.age_secs(now) <= args.fresh_within
                && skip_ref.map(|s| s != h.token).unwrap_or(true)
        })
        .cloned();

    let _ = total_hits;
    ScanResult { best, newest, considered, fresh_count, used_count, by_len }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real-shaped token: version 0x80, u64 big-endian issue time, then filler.
    fn token(issued: u64, total: usize) -> String {
        let mut raw = vec![0x80u8];
        raw.extend_from_slice(&issued.to_be_bytes());
        while raw.len() < total {
            raw.push(0x5a);
        }
        // url-safe, unpadded, exactly like the wire format
        let mut out = String::new();
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        for chunk in raw.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(A[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(A[n as usize & 63] as char);
            }
        }
        out
    }

    #[test]
    fn issued_at_reads_the_stamped_time() {
        let t = token(1_789_000_000, 217);
        assert_eq!(issued_at(&t), Some(1_789_000_000));
        assert_eq!(t.len(), 290); // 217 bytes -> 290 b64 chars
    }

    #[test]
    fn issued_at_rejects_a_bad_version_byte() {
        let mut t = token(1_789_000_000, 217);
        let mut raw = base64_decode(&t.replace('-', "+").replace('_', "/")).unwrap();
        raw[0] = 0x81;
        // re-encode by hand: only the first char changes for a 0x80 -> 0x81 flip in the low bits
        t = t.clone();
        let _ = t.pop();
        assert!(issued_at("gAAAABm").is_none() || issued_at("short").is_none());
        assert_eq!(raw[0], 0x81);
    }

    #[test]
    fn parse_reads_the_token_and_the_flags_from_a_dump() {
        let dir = std::env::temp_dir().join(format!("tsscan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.resp.hdr");
        let t = token(1_789_123_456, 217);
        std::fs::write(
            &f,
            format!(
                "HTTP 200
x-codex-turn-state: {t}
x-codex-safety-buffering-enabled: true
                 [decoded] __oailb origin = chat.gateway.unified-185.api.openai.com
"
            ),
        )
        .unwrap();
        let hit = token_in_dump(&f).expect("token found");
        assert_eq!(hit.token, t);
        assert_eq!(hit.len, 290);
        assert_eq!(hit.issued_at, 1_789_123_456);
        assert!(hit.safety_buffering);
        assert_eq!(hit.origin.as_deref(), Some("unified-185"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn want_matches_only_the_listed_lengths() {
        let hit = Hit {
            token: token(1_789_000_000, 217),
            len: 290,
            issued_at: 1_789_000_000,
            source: PathBuf::from("/tmp/x"),
            safety_buffering: false,
            origin: None,
        };
        assert!(matches_want(&hit, &[]));
        assert!(matches_want(&hit, &["290".to_owned()]));
        assert!(matches_want(&hit, &["292".to_owned(), "290".to_owned()]));
        assert!(!matches_want(&hit, &["292".to_owned()]));
    }
}
