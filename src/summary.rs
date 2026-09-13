//! Request and response summaries.
//!
//! Requests are summarized after they are decoded (one function, no state).
//!
//! Responses are summarized **incrementally**: for `text/event-stream` bodies every line is parsed
//! as it arrives, so a live stream never has to be held in memory in full. Non-SSE bodies (such as
//! the `/models` catalog) are accumulated up to `limits.summary_buffer_bytes` and summarized from
//! the parsed JSON — that path marks itself truncated when the cap is hit.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Longest SSE line we are willing to assemble (a runaway frame is dropped instead of buffered).
const MAX_LINE: usize = 1024 * 1024;
/// Answer/reasoning text kept in the summary.
const TEXT_CAP: usize = 4000;
/// Model slugs listed in a catalog summary.
const SLUG_CAP: usize = 40;

pub fn short_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Finds the `zstd` CLI: `$ZSTD` first, then a copy next to our own executable, then `PATH`.
///
/// The CLI is only needed to decode/encode compressed request bodies; on Windows it is frequently
/// installed somewhere that is not on `PATH` (e.g. a conda environment), which used to make the
/// environment rewrite silently skip compressed requests.
fn zstd_program() -> std::ffi::OsString {
    if let Some(path) = std::env::var_os("ZSTD") {
        if std::path::Path::new(&path).is_file() {
            return path;
        }
    }
    let exe = if cfg!(windows) { "zstd.exe" } else { "zstd" };
    if let Ok(own) = std::env::current_exe() {
        if let Some(dir) = own.parent() {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }
    for dir in [
        "C:/ProgramData/miniconda3/Library/bin",
        "C:/ProgramData/miniconda3/Scripts",
        "/usr/local/bin",
        "/opt/homebrew/bin",
    ] {
        let candidate = std::path::Path::new(dir).join(exe);
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    std::ffi::OsString::from(exe)
}

/// Runs the system `zstd` CLI (`mode` is `-d` or `-3`).
pub fn zstd_cli(mode: &str, input: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut child = std::process::Command::new(zstd_program())
        .arg(mode)
        .arg("-q")
        .arg("-c")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
        drop(stdin);
    });
    let out = child.wait_with_output().ok()?;
    let _ = feeder.join();
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}

/// Decompresses a body according to its `content-encoding` (only zstd is handled).
pub fn decode_body(raw: &[u8], cenc: Option<&str>) -> Option<Vec<u8>> {
    match cenc {
        Some(e) if e.to_ascii_lowercase().contains("zstd") => zstd_cli("-d", raw),
        _ => Some(raw.to_vec()),
    }
}

/// Summary of a decoded request body.
pub fn summarize_request(body: &Value) -> Value {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut msg_chars = 0usize;
    let mut last_user = String::new();
    let mut system_item_chars = 0usize;
    if let Some(items) = body.get("input").and_then(|v| v.as_array()) {
        for it in items {
            let role = it.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let kind = it
                .get("type")
                .and_then(|v| v.as_str())
                .or_else(|| it.get("role").and_then(|v| v.as_str()))
                .unwrap_or("?")
                .to_owned();
            *kinds.entry(kind).or_insert(0) += 1;
            let mut text = String::new();
            if let Some(parts) = it.get("content").and_then(|v| v.as_array()) {
                for c in parts {
                    if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
            }
            msg_chars += text.len();
            if matches!(role, "system" | "developer") {
                system_item_chars += text.len();
            }
            if role == "user" && !text.is_empty() {
                last_user = text.chars().take(400).collect();
            }
        }
    }
    let instructions = body.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
    // responses-lite turns carry the system prompt as an input item instead of the top-level field.
    let instructions_source = if !instructions.is_empty() {
        "field"
    } else if system_item_chars > 0 {
        "item"
    } else {
        "none"
    };
    json!({
        "model": body.get("model"),
        "service_tier": body.get("service_tier"),
        "reasoning": body.get("reasoning"),
        "stream": body.get("stream"),
        "instructions_len": instructions.len(),
        "instructions_sha": short_hash(instructions.as_bytes()),
        "instructions_source": instructions_source,
        "system_item_chars": system_item_chars,
        "prompt_cache_key": body.get("prompt_cache_key"),
        "tools": body.get("tools").and_then(|v| v.as_array()).map(|a| a.len()),
        "items": body.get("input").and_then(|v| v.as_array()).map(|a| a.len()),
        "item_kinds": kinds,
        "message_chars": msg_chars,
        "client_metadata_keys": body
            .get("client_metadata")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect::<Vec<_>>()),
        "last_user": last_user,
    })
}

/// Incremental response summarizer.
pub struct Summarizer {
    sse: bool,
    cap: usize,
    etag: Option<String>,
    response_encoding: Option<String>,
    stream_skipped: bool,
    pending: Vec<u8>,
    raw: Vec<u8>,
    raw_truncated: bool,
    bytes: u64,
    models: Vec<String>,
    tiers: Vec<String>,
    ids: Vec<String>,
    events: BTreeMap<String, usize>,
    errors: Vec<String>,
    metadata_frames: Vec<String>,
    answer: String,
    reasoning: String,
    usage: Option<Value>,
}

impl Summarizer {
    pub fn new(content_type: Option<&str>, cap: usize, _path: impl Into<String>) -> Self {
        let sse = content_type
            .map(|c| c.to_ascii_lowercase().contains("event-stream"))
            .unwrap_or(false);
        Self {
            sse,
            cap,
            etag: None,
            response_encoding: None,
            stream_skipped: false,
            pending: Vec::new(),
            raw: Vec::new(),
            raw_truncated: false,
            bytes: 0,
            models: Vec::new(),
            tiers: Vec::new(),
            ids: Vec::new(),
            events: BTreeMap::new(),
            errors: Vec::new(),
            metadata_frames: Vec::new(),
            answer: String::new(),
            reasoning: String::new(),
            usage: None,
        }
    }

    pub fn set_etag(&mut self, etag: Option<String>) {
        self.etag = etag;
    }

    /// The response `content-encoding`, used to decode a non-SSE body before summarizing it.
    pub fn set_response_encoding(&mut self, encoding: Option<String>) {
        self.response_encoding = encoding;
    }

    /// Marks that the payload itself was intentionally not recorded (the `/models` catalog case).
    pub fn mark_stream_skipped(&mut self) {
        self.stream_skipped = true;
    }

    pub fn is_sse(&self) -> bool {
        self.sse
    }

    /// Feeds one chunk of the response body.
    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        if self.sse {
            self.pending.extend_from_slice(chunk);
            while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                self.parse_line(text.trim());
            }
            if self.pending.len() > MAX_LINE {
                self.pending.clear();
                self.errors.push("dropped an over-long SSE line".to_owned());
            }
        } else if self.cap > 0 {
            let take = self.cap.saturating_sub(self.raw.len()).min(chunk.len());
            self.raw.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                self.raw_truncated = true;
            }
        } else if !chunk.is_empty() {
            self.raw_truncated = true;
        }
    }

    fn parse_line(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        let t = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if !t.starts_with('{') {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else {
            return;
        };
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            *self.events.entry(ty.to_owned()).or_insert(0) += 1;
            if ty == "codex.response.metadata" {
                if let Some(h) = v.get("headers") {
                    self.metadata_frames.push(h.to_string());
                }
            }
            if ty.contains("error") || ty.contains("failed") {
                self.errors.push(t.chars().take(300).collect());
            }
            if ty == "response.output_text.delta" {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    if self.answer.len() < TEXT_CAP {
                        self.answer.push_str(d);
                    }
                }
            }
            if ty.contains("reasoning") && ty.ends_with(".delta") {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    if self.reasoning.len() < TEXT_CAP {
                        self.reasoning.push_str(d);
                    }
                }
            }
        }
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            if !self.models.iter().any(|e| e == m) {
                self.models.push(m.to_owned());
            }
        }
        if let Some(s) = v.get("service_tier").and_then(|x| x.as_str()) {
            if !self.tiers.iter().any(|e| e == s) {
                self.tiers.push(s.to_owned());
            }
        }
        if let Some(u) = v.get("usage") {
            if u.get("output_tokens").is_some() {
                self.usage = Some(u.clone());
            }
        }
        for key in ["id", "response_id"] {
            if let Some(id) = v.get(key).and_then(|x| x.as_str()) {
                if id.starts_with("resp_") && !self.ids.iter().any(|e| e == id) {
                    self.ids.push(id.to_owned());
                }
            }
        }
    }

    /// The finished summary. `recorded` is the relative file name, or `None` when the body was not
    /// written (e.g. the `/models` catalog, which is summarized by etag + hash instead).
    pub fn finish(&mut self, recorded: Option<String>) -> Value {
        let catalog = if self.sse {
            Value::Null
        } else {
            self.catalog_summary()
        };
        json!({
            "sse": self.sse,
            "bytes": self.bytes,
            "recorded": recorded,
            "stream_skipped": self.stream_skipped,
            "etag": self.etag,
            "content_encoding": self.response_encoding,
            "models_seen": self.models,
            "service_tiers_seen": self.tiers,
            "response_ids": self.ids,
            "events": self.events,
            "errors": self.errors,
            "metadata_frames": self.metadata_frames,
            "usage": self.usage,
            "answer_chars": self.answer.len(),
            "answer": self.answer,
            "reasoning_chars": self.reasoning.len(),
            "reasoning": self.reasoning,
            "catalog": catalog,
        })
    }

    /// For non-SSE bodies: identify a model catalog and describe it without keeping the payload.
    /// When the body arrived zstd-compressed, the accumulated prefix is decoded first.
    fn catalog_summary(&self) -> Value {
        let truncated = self.raw_truncated || (self.cap == 0 && self.bytes > 0);
        let mut decoded_from_zstd = false;
        let mut raw = self.raw.clone();
        if serde_json::from_slice::<Value>(&raw).is_err() && !raw.is_empty() {
            if let Some(e) = self.response_encoding.as_deref() {
                if e.to_ascii_lowercase().contains("zstd") {
                    if let Some(plain) = zstd_cli("-d", &raw) {
                        raw = plain;
                        decoded_from_zstd = true;
                    }
                }
            }
        }
        if raw.is_empty() {
            return json!({ "parsed": false, "truncated": truncated });
        }
        match serde_json::from_slice::<Value>(&raw) {
            Ok(v) => {
                let slugs: Vec<String> = v
                    .get("models")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.get("slug").and_then(|s| s.as_str()).map(str::to_owned))
                            .take(SLUG_CAP)
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "parsed": true,
                    "truncated": truncated,
                    "decoded_from_zstd": decoded_from_zstd,
                    "sha256": sha256_hex(&raw),
                    "bytes_seen": raw.len(),
                    "model_count": if slugs.is_empty() { Value::Null } else { json!(slugs.len()) },
                    "slugs": slugs,
                })
            }
            Err(_) => json!({
                "parsed": false,
                "truncated": truncated,
                "sha256": sha256_hex(&raw),
                "bytes_seen": raw.len(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Summarizer, frames: &[&str]) {
        for frame in frames {
            s.push(format!("{frame}\n").as_bytes());
        }
    }

    #[test]
    fn request_summary_detects_lite_and_classic_instruction_shapes() {
        let classic = json!({
            "model": "gpt-6-astra",
            "instructions": "You are Codex",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        });
        let s = summarize_request(&classic);
        assert_eq!(s["instructions_source"], "field");
        assert_eq!(s["instructions_len"], 13);

        let lite = json!({
            "model": "gpt-6-astra",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex, ..."}]},
                {"type": "additional_tools", "tools": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            ],
        });
        let s = summarize_request(&lite);
        assert_eq!(s["instructions_source"], "item");
        assert_eq!(s["instructions_len"], 0);
        assert_eq!(s["last_user"], "hi");
        assert_eq!(s["item_kinds"]["additional_tools"], 1);
    }

    #[test]
    fn sse_frames_are_parsed_incrementally_across_chunk_boundaries() {
        let mut s = Summarizer::new(Some("text/event-stream"), 4 * 1024 * 1024, "resp.stream.sse");
        // split a frame in the middle of a line to prove the pending buffer works
        s.push(b"data: {\"type\":\"response.created\",\"model\":\"gpt-6-astra\",\"serv");
        s.push(b"ice_tier\":\"auto\"}\n");
        feed(
            &mut s,
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"he\"}",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}",
                "data: {\"type\":\"response.completed\",\"id\":\"resp_abc\",\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}",
                "data: [DONE]",
            ],
        );
        let out = s.finish(Some("resp.stream.sse".to_owned()));
        assert_eq!(out["sse"], true);
        assert_eq!(out["models_seen"][0], "gpt-6-astra");
        assert_eq!(out["service_tiers_seen"][0], "auto");
        assert_eq!(out["answer"], "hello");
        assert_eq!(out["response_ids"][0], "resp_abc");
        assert_eq!(out["usage"]["output_tokens"], 4);
        assert_eq!(out["recorded"], "resp.stream.sse");
        assert!(out["bytes"].as_u64().unwrap() > 40);
    }

    #[test]
    fn catalog_summary_keeps_etag_and_hash_not_the_payload() {
        let body = serde_json::to_vec(&json!({"models": [{"slug": "gpt-6-astra"}, {"slug": "gpt-5.6-luna"}]}))
            .unwrap();
        let mut s = Summarizer::new(Some("application/json"), 4 * 1024 * 1024, "resp.stream.json");
        s.set_etag(Some("W/\"abc\"".to_owned()));
        s.push(&body);
        let out = s.finish(None);
        assert_eq!(out["catalog"]["parsed"], true);
        assert_eq!(out["catalog"]["slugs"][1], "gpt-5.6-luna");
        assert_eq!(out["etag"], "W/\"abc\"");
        assert!(out["catalog"]["sha256"].as_str().unwrap().len() == 64);
        assert_eq!(out["recorded"], Value::Null);
    }

    #[test]
    fn zero_cap_keeps_nothing_and_marks_truncation() {
        let mut s = Summarizer::new(Some("application/json"), 0, "resp.stream.json");
        s.push(b"{\"models\": []}");
        let out = s.finish(None);
        assert_eq!(out["catalog"]["parsed"], false);
        assert_eq!(out["catalog"]["truncated"], true);
        assert_eq!(out["bytes"], 14);
    }

    #[test]
    fn a_chunk_exactly_at_the_cap_is_not_marked_truncated() {
        let mut s = Summarizer::new(Some("application/json"), 8, "resp.stream.json");
        s.push(b"12345678");
        let out = s.finish(None);
        assert_eq!(out["catalog"]["truncated"], false);
        // one byte more flips it
        let mut s = Summarizer::new(Some("application/json"), 8, "resp.stream.json");
        s.push(b"12345678");
        s.push(b"9");
        let out = s.finish(None);
        assert_eq!(out["catalog"]["truncated"], true);
    }

    #[test]
    fn errors_are_collected() {
        let mut s = Summarizer::new(Some("text/event-stream"), 1024, "resp.stream.sse");
        feed(
            &mut s,
            &[
                "data: {\"type\":\"response.failed\",\"error\":\"boom\"}",
                "data: {\"type\":\"codex.response.metadata\",\"headers\":{\"x-models-etag\":\"W/1\"}}",
            ],
        );
        let out = s.finish(None);
        assert_eq!(out["errors"].as_array().unwrap().len(), 1);
        assert_eq!(out["metadata_frames"].as_array().unwrap().len(), 1);
        assert_eq!(out["events"]["response.failed"], 1);
    }
}
