//! codex-rec: a recording + forwarding HTTP/1.1 reverse proxy for the Codex backend.
//!
//! v0.2.0 adds a configurable **device persona** (override-or-inherit per field), TOML
//! configuration, header rewriting from the config file and a TLS extension-order switch.
//!
//! TLS: the Linux codex client uses native-tls / a bundled OpenSSL and sends **no ALPN**, so
//! the default backend here reproduces that ClientHello byte-for-byte (measured JA3
//! `0b85eb0d4981e69064e40753e4f0ac5f` with SNI, `23211f2b48104c7030b93680a2efcfd0` without).
//! `tls.extension_order = "randomize"` switches to the rustls backend, whose ClientHello shuffles
//! the extension order per connection (OpenSSL has no such option).
//!
//! Recording layout (under `log_dir`):
//!   index.jsonl               one compact line per request/response
//!   req-<stamp>.hdr           incoming request line + headers (secrets redacted)
//!   req-<stamp>.out.hdr       what we actually sent upstream (after persona + rewrites)
//!   req-<stamp>.body/.json    raw and decompressed request body
//!   req-<stamp>.summary.json  model / service_tier / effort / instructions / client_metadata
//!   resp-<stamp>.hdr          status + headers, with the __oailb origin host decoded
//!   resp-<stamp>.sse          raw streamed response
//!   resp-<stamp>.summary.json model(s) seen / service_tier(s) / response ids / errors / answer

mod config;
mod persona;
mod session;

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::Stream;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use config::{Config, ExtensionOrder};
use session::Sessions;

const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_ACC: usize = 32 * 1024 * 1024;
const HOP_BY_HOP: &[&str] = &[
    "host",
    "connection",
    "proxy-connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
];

struct App {
    cfg: Config,
    client: reqwest::Client,
    sessions: Option<Arc<Sessions>>,
}

// ---------------------------------------------------------------- helpers

static SEQ: AtomicU64 = AtomicU64::new(0);

fn stamp() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{n}-{seq}")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn redact(name: &str, value: &str) -> String {
    let n = name.to_ascii_lowercase();
    if n == "authorization" || n == "x-api-key" || n == "cookie" {
        let head: String = value.chars().take(12).collect();
        return format!("{head}...[redacted {} bytes]", value.len());
    }
    value.to_owned()
}

/// Runs the system `zstd` CLI (`mode` is `-d` or `-3`).
fn zstd_cli(mode: &str, input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("zstd")
        .arg(mode)
        .arg("-q")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

fn decode_body(raw: &[u8], cenc: Option<&str>) -> Option<Vec<u8>> {
    match cenc {
        Some(e) if e.to_ascii_lowercase().contains("zstd") => zstd_cli("-d", raw),
        _ => Some(raw.to_vec()),
    }
}

fn short_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Decodes the `__oailb` cookie that Cloudflare sets: it names the origin pool in use.
fn decode_oailb(headers: &[String]) -> Option<String> {
    for h in headers {
        let lower = h.to_ascii_lowercase();
        let Some(start) = lower.find("__oailb=") else { continue };
        let rest = &h[start + "__oailb=".len()..];
        let end = rest.find(|c: char| c == ';' || c == ' ' || c == '\r').unwrap_or(rest.len());
        let parts: Vec<&str> = rest[..end].split('.').collect();
        if parts.len() < 3 {
            continue;
        }
        let mut payload = parts[1].replace('-', "+").replace('_', "/");
        while payload.len() % 4 != 0 {
            payload.push('=');
        }
        if let Some(decoded) = base64_decode(&payload) {
            if let Ok(v) = serde_json::from_slice::<Value>(&decoded) {
                if let Some(host) = v.get("host").and_then(|h| h.as_str()) {
                    return Some(host.to_owned());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- summaries

fn summarize_request(body: &Value) -> Value {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut msg_chars = 0usize;
    let mut last_user = String::new();
    if let Some(items) = body.get("input").and_then(|v| v.as_array()) {
        for it in items {
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
            if it.get("role").and_then(|v| v.as_str()) == Some("user") && !text.is_empty() {
                last_user = text.chars().take(400).collect();
            }
        }
    }
    let instructions = body.get("instructions").and_then(|v| v.as_str()).unwrap_or("");
    json!({
        "model": body.get("model"),
        "service_tier": body.get("service_tier"),
        "reasoning": body.get("reasoning"),
        "stream": body.get("stream"),
        "instructions_len": instructions.len(),
        "instructions_sha": short_hash(instructions.as_bytes()),
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

fn summarize_response(text: &str) -> Value {
    let mut models: Vec<String> = Vec::new();
    let mut tiers: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut events: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut metadata_frames: Vec<String> = Vec::new();
    let mut answer = String::new();
    for line in text.lines() {
        let t = line.trim();
        // HTTP/SSE responses prefix every JSON frame with `data: `; WebSocket frames do not.
        let t = t.strip_prefix("data:").map(str::trim).unwrap_or(t);
        if !t.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else { continue };
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            *events.entry(ty.to_owned()).or_insert(0) += 1;
            if ty == "codex.response.metadata" {
                if let Some(h) = v.get("headers") {
                    metadata_frames.push(h.to_string());
                }
            }
            if ty.contains("error") || ty.contains("failed") {
                errors.push(t.chars().take(300).collect());
            }
            if ty == "response.output_text.delta" {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    answer.push_str(d);
                }
            }
        }
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            if !models.iter().any(|e| e == m) {
                models.push(m.to_owned());
            }
        }
        if let Some(s) = v.get("service_tier").and_then(|x| x.as_str()) {
            if !tiers.iter().any(|e| e == s) {
                tiers.push(s.to_owned());
            }
        }
        for key in ["id", "response_id"] {
            if let Some(id) = v.get(key).and_then(|x| x.as_str()) {
                if id.starts_with("resp_") && !ids.iter().any(|e| e == id) {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    json!({
        "models_seen": models,
        "service_tiers_seen": tiers,
        "response_ids": ids,
        "events": events,
        "errors": errors,
        "metadata_frames": metadata_frames,
        "answer_chars": answer.len(),
        "answer": answer.chars().take(4000).collect::<String>(),
    })
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
}

fn header_dump(lines: &[String]) -> String {
    format!("{}\n", lines.join("\n"))
}

// ---------------------------------------------------------------- tee

struct Sink {
    file: Option<File>,
    acc: Vec<u8>,
    prefix: String,
    log_dir: PathBuf,
    req_stamp: String,
    status: u16,
    origin: Option<String>,
    persona: Vec<String>,
    sessions: Option<Arc<Sessions>>,
    session_id: Option<String>,
}

impl Sink {
    fn write_chunk(&mut self, chunk: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(chunk);
        }
        if self.acc.len() < MAX_ACC {
            self.acc.extend_from_slice(chunk);
        }
    }

    fn finish(&mut self) {
        let text = String::from_utf8_lossy(&self.acc).to_string();
        let summary = summarize_response(&text);
        let _ = fs::write(
            self.log_dir.join(format!("resp-{}.summary.json", self.prefix)),
            serde_json::to_vec_pretty(&summary).unwrap_or_default(),
        );
        let line = json!({
            "req": self.req_stamp,
            "status": self.status,
            "origin": self.origin,
            "persona": self.persona,
            "models_seen": summary.get("models_seen"),
            "service_tiers_seen": summary.get("service_tiers_seen"),
            "response_ids": summary.get("response_ids"),
            "events": summary.get("events"),
            "errors": summary.get("errors"),
            "answer_chars": summary.get("answer_chars"),
        });
        let _ = append_line(&self.log_dir.join("index.jsonl"), &line.to_string());
        if let (Some(sessions), Some(session)) = (self.sessions.as_ref(), self.session_id.as_deref()) {
            sessions.append(
                session,
                &json!({
                    "type": "response",
                    "ts": now_ms(),
                    "status": self.status,
                    "origin": self.origin,
                    "summary": summary,
                }),
            );
        }
    }
}

struct TeeStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    sink: Arc<Mutex<Sink>>,
    done: bool,
}

impl Stream for TeeStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Ok(mut s) = this.sink.lock() {
                    s.write_chunk(&chunk);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                eprintln!("[upstream stream error] {e}");
                if let Ok(mut s) = this.sink.lock() {
                    s.finish();
                }
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                if let Ok(mut s) = this.sink.lock() {
                    s.finish();
                }
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------- handler

fn text_response(code: u16, msg: &str) -> Response {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn handle(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let cfg = &app.cfg;
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    // `uri_in` is what the client asked for (logged as-is); `uri` is what we actually send
    // upstream after the [routes] path mapping. Both are recorded, so the mapping stays auditable.
    let uri_in = parts.uri.to_string();
    let (in_path, in_query) = match uri_in.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (uri_in.clone(), None),
    };
    let mut uri = cfg.routes.upstream_path(&in_path);
    if let Some(query) = in_query {
        uri.push('?');
        uri.push_str(&query);
    }
    let req_stamp = stamp();
    let prefix = req_stamp.clone();

    let body_bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return text_response(400, &format!("body read error: {e}")),
    };

    let session_id = parts
        .headers
        .get("session-id")
        .or_else(|| parts.headers.get("thread-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // ---- incoming request capture + outgoing header assembly
    let mut incoming_lines: Vec<String> = Vec::new();
    let mut out_headers = reqwest::header::HeaderMap::new();
    let mut cenc = None;
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        let value = v.to_str().unwrap_or("<non-utf8>").to_owned();
        incoming_lines.push(format!("{name}: {}", redact(&name, &value)));
        if name == "content-encoding" {
            cenc = Some(value.clone());
        }
        let dropped = cfg
            .request_drop_globs
            .as_ref()
            .map(|globs| globs.is_match(&name))
            .unwrap_or(false);
        if HOP_BY_HOP.contains(&name.as_str()) || dropped {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            out_headers.insert(hn, hv);
        }
    }
    let _ = fs::write(
        cfg.log_dir.join(format!("req-{req_stamp}.hdr")),
        format!("{method} {uri_in}\n{}", header_dump(&incoming_lines)),
    );
    let _ = fs::write(cfg.log_dir.join(format!("req-{req_stamp}.body")), &body_bytes);

    // ---- device persona (override-or-inherit) then configured header rewrites
    let persona_notes = persona::apply(cfg, &mut out_headers, &mut uri);
    let mut out_lines: Vec<String> = vec![format!("{method} {uri}")];
    for (name, value) in cfg.request_rules.set.iter() {
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            out_headers.insert(hn, hv);
            out_lines.push(format!("{name}: {value}   <- set by config"));
        }
    }
    for (k, v) in out_headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if cfg.request_rules.set.keys().any(|s| s.eq_ignore_ascii_case(&name)) {
            continue;
        }
        out_lines.push(format!("{name}: {}", redact(&name, v.to_str().unwrap_or("<non-utf8>"))));
    }
    for note in persona_notes.iter() {
        out_lines.push(format!("# persona: {note}"));
    }
    let _ = fs::write(cfg.log_dir.join(format!("req-{req_stamp}.out.hdr")), header_dump(&out_lines));

    // ---- optional body rewriting (JSON only; zstd via the CLI)
    let mut body_out = body_bytes.to_vec();
    if !cfg.body_drop.is_empty() || !cfg.body_set.is_empty() {
        match decode_body(&body_bytes, cenc.as_deref()) {
            Some(decoded) => match serde_json::from_slice::<Value>(&decoded) {
                Ok(mut v) if v.is_object() => {
                    let obj = v.as_object_mut().unwrap();
                    for k in cfg.body_drop.iter() {
                        obj.remove(k);
                    }
                    for (k, raw) in cfg.body_set.iter() {
                        let parsed =
                            serde_json::from_str::<Value>(raw).unwrap_or(Value::String(raw.clone()));
                        obj.insert(k.clone(), parsed);
                    }
                    let new = serde_json::to_vec(&v).unwrap_or_default();
                    body_out = if cenc.is_some() {
                        zstd_cli("-3", &new).unwrap_or(new)
                    } else {
                        new
                    };
                }
                _ => eprintln!("[body rewrite skipped] body is not a JSON object"),
            },
            None => eprintln!("[body rewrite skipped] could not decode body"),
        }
    }

    // ---- request body recording
    if let Some(decoded) = decode_body(&body_bytes, cenc.as_deref()) {
        let _ = fs::write(cfg.log_dir.join(format!("req-{req_stamp}.json")), &decoded);
        if let Ok(v) = serde_json::from_slice::<Value>(&decoded) {
            let summary = summarize_request(&v);
            let _ = fs::write(
                cfg.log_dir.join(format!("req-{req_stamp}.summary.json")),
                serde_json::to_vec_pretty(&summary).unwrap_or_default(),
            );
            let line = json!({
                "req": req_stamp,
                "method": method.to_string(),
                "uri": uri.clone(),
                "persona": persona_notes.clone(),
                "request": summary,
            });
            if let (Some(sessions), Some(session)) = (app.sessions.as_ref(), session_id.as_deref()) {
                if let Some(text) = line
                    .get("request")
                    .and_then(|r| r.get("last_user"))
                    .and_then(|v| v.as_str())
                {
                    sessions.note_title(session, text);
                }
                sessions.append(
                    session,
                    &json!({
                        "type": "request",
                        "ts": now_ms(),
                        "uri": uri.clone(),
                        "persona": persona_notes.clone(),
                        "request": line.get("request"),
                    }),
                );
            }
            let _ = append_line(&cfg.log_dir.join("index.jsonl"), &line.to_string());
        }
    }

    // ---- forward
    let url = format!("{}{}", cfg.upstream, uri);
    let mut builder = app
        .client
        .request(method.clone(), &url)
        .headers(out_headers);
    if !body_out.is_empty() {
        builder = builder.body(body_out);
    }
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return text_response(502, &format!("upstream error: {e}")),
    };

    let status = resp.status();
    let mut resp_lines: Vec<String> = vec![format!("HTTP {}", status.as_u16())];
    let mut rb = Response::builder().status(status);
    if let Some(hs) = rb.headers_mut() {
        for (k, v) in resp.headers().iter() {
            let name = k.as_str().to_ascii_lowercase();
            let value = v.to_str().unwrap_or("<non-utf8>").to_owned();
            resp_lines.push(format!("{name}: {value}"));
            let dropped = cfg
                .response_drop_globs
                .as_ref()
                .map(|globs| globs.is_match(&name))
                .unwrap_or(false);
            if HOP_BY_HOP.contains(&name.as_str())
                || dropped
                || name == "content-length"
                || name == "transfer-encoding"
            {
                continue;
            }
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(&value),
            ) {
                hs.insert(hn, hv);
            }
        }
        for (name, value) in cfg.response_rules.set.iter() {
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                hs.insert(hn, hv);
                resp_lines.push(format!("{name}: {value}   <- set by config"));
            }
        }
    }
    let origin = decode_oailb(&resp_lines);
    if let Some(o) = origin.as_ref() {
        resp_lines.push(format!("[decoded] __oailb origin = {o}"));
    }
    let _ = fs::write(cfg.log_dir.join(format!("resp-{prefix}.hdr")), header_dump(&resp_lines));

    // ---- optional catalog rewrite (buffered, non-streaming)
    if cfg.rewrite_catalog && uri.contains("/models") {
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return text_response(502, &format!("upstream read error: {e}")),
        };
        let decoded = decode_body(&bytes, None).unwrap_or_else(|| bytes.to_vec());
        let rewritten = match serde_json::from_slice::<Value>(&decoded) {
            Ok(mut v) => {
                if let Some(models) = v.get_mut("models").and_then(|m| m.as_array_mut()) {
                    for m in models.iter_mut() {
                        if let Some(o) = m.as_object_mut() {
                            o.insert("use_responses_lite".to_owned(), Value::Bool(false));
                            o.insert("service_tiers".to_owned(), Value::Array(vec![]));
                            o.insert("additional_speed_tiers".to_owned(), Value::Array(vec![]));
                        }
                    }
                }
                serde_json::to_vec(&v).unwrap_or(decoded)
            }
            Err(_) => decoded,
        };
        let _ = fs::write(cfg.log_dir.join(format!("resp-{prefix}.catalog.json")), &rewritten);
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(rewritten))
            .unwrap_or_else(|_| text_response(500, "response build error"));
    }

    let file = File::create(cfg.log_dir.join(format!("resp-{prefix}.sse"))).ok();
    let sink = Arc::new(Mutex::new(Sink {
        file,
        acc: Vec::new(),
        prefix,
        log_dir: cfg.log_dir.clone(),
        req_stamp,
        status: status.as_u16(),
        origin,
        persona: persona_notes,
        sessions: app.sessions.clone(),
        session_id: session_id.clone(),
    }));
    let stream = TeeStream {
        inner: Box::pin(resp.bytes_stream()),
        sink,
        done: false,
    };
    rb.body(Body::from_stream(stream))
        .unwrap_or_else(|_| text_response(500, "response build error"))
}

// ---------------------------------------------------------------- upstream TLS

fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .pool_max_idle_per_host(2)
}

/// native-tls / OpenSSL, no ALPN, HTTP/1.1: matches the measured codex ClientHello.
#[cfg(feature = "native-tls-backend")]
fn build_native_tls_client() -> reqwest::Client {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .expect("failed to build native-tls connector");
    base_builder()
        .use_preconfigured_tls(tls)
        .build()
        .expect("failed to build reqwest client")
}

#[cfg(not(feature = "native-tls-backend"))]
fn build_native_tls_client() -> reqwest::Client {
    base_builder().build().expect("failed to build reqwest client")
}

/// rustls + aws-lc-rs (prefer-post-quantum), no ALPN: rustls shuffles the extension order per
/// connection, which is what `tls.extension_order = "randomize"` is for.
#[cfg(feature = "rustls-backend")]
fn build_rustls_client() -> reqwest::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols.clear();
    base_builder()
        .use_preconfigured_tls(tls)
        .build()
        .expect("failed to build reqwest client")
}

fn build_client(cfg: &Config) -> reqwest::Client {
    match cfg.extension_order {
        ExtensionOrder::Fixed => {
            println!("tls: native-tls/OpenSSL, fixed extension order (matches codex)");
            build_native_tls_client()
        }
        ExtensionOrder::Randomize => {
            #[cfg(feature = "rustls-backend")]
            {
                println!("tls: rustls backend, extension order randomized per connection");
                build_rustls_client()
            }
            #[cfg(not(feature = "rustls-backend"))]
            {
                eprintln!("tls: rustls-backend not compiled in; using native-tls (fixed order)");
                build_native_tls_client()
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let (cfg, warnings) = match config::load() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };
    fs::create_dir_all(&cfg.log_dir).expect("cannot create log dir");

    println!(
        "codex-rec {} listening on http://{} -> {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen,
        cfg.upstream
    );
    println!("recording into {}", cfg.log_dir.display());
    println!(
        "routes: strip {:?} -> prefix {:?}",
        cfg.routes.strip_prefixes, cfg.routes.upstream_prefix
    );
    if let Some(p) = cfg.config_path.as_ref() {
        println!("config file: {}", p.display());
    }
    match cfg.persona.as_ref() {
        Some(p) => println!(
            "persona: originator={:?} codex_version={:?} os={:?} arch={:?} terminal={:?} user_agent={:?} rewrite_client_version={}",
            p.originator, p.codex_version, p.os, p.arch, p.terminal, p.user_agent, p.rewrite_client_version
        ),
        None => println!("persona: none (the client's identity passes through)"),
    }
    for w in &warnings {
        eprintln!("warning: {w}");
    }

    let sessions = cfg.session_capture.then(|| {
        println!("session capture -> {}", cfg.session_dir.display());
        Arc::new(Sessions::new(cfg.session_dir.clone()))
    });
    let client = build_client(&cfg);
    let app = Arc::new(App {
        cfg,
        client,
        sessions,
    });
    let listen = app.cfg.listen.clone();
    let router = Router::new().fallback(any(handle)).with_state(Arc::clone(&app));
    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind failed");
    axum::serve(listener, router).await.expect("server error");
}
