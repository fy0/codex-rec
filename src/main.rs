//! codex-rec: a tiny recording + forwarding HTTP/1.1 reverse proxy for the Codex backend.
//!
//! Purpose: sit between the real codex client and `https://chatgpt.com` so we can see every
//! request/response byte, while presenting the *same* TLS fingerprint the real client does
//! (native-tls / OpenSSL, **no ALPN**, HTTP/1.1 only) — the stack the Linux codex client uses by
//! default (it only switches to rustls when CODEX_CA_CERTIFICATE/SSL_CERT_FILE is set).
//!
//! Recording layout (under --log-dir):
//!   index.jsonl            one compact line per request/response
//!   req-<stamp>.hdr        request line + headers (authorization redacted)
//!   req-<stamp>.body       raw request body as received
//!   req-<stamp>.json       decompressed body (zstd handled through the `zstd` CLI) + extracted fields
//!   req-<stamp>.summary.json
//!   resp-<stamp>.hdr       status line + headers (with the __oailb origin host decoded)
//!   resp-<stamp>.sse       raw response body as it streamed
//!   resp-<stamp>.summary.json  model / service_tier / response ids / error events / answer text
//!
//! Optional rewriting (all off by default):
//!   --drop-req-header name[,name]     --set-req-header name=value[;name=value]
//!   --drop-res-header name[,name]     --set-res-header name=value[;name=value]
//!   --drop-body-key key[,key]         --set-body-key key=json[;key=json]
//!   --rewrite-catalog                 force use_responses_lite=false and service_tiers=[] in /models
//!
//! Build: cargo build --release   (needs a C toolchain for aws-lc-rs)

use std::collections::HashMap;
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

#[derive(Clone)]
struct Cfg {
    upstream: String,
    log_dir: PathBuf,
    drop_req: Vec<String>,
    set_req: Vec<(String, String)>,
    drop_res: Vec<String>,
    set_res: Vec<(String, String)>,
    body_drop: Vec<String>,
    body_set: Vec<(String, String)>,
    rewrite_catalog: bool,
}

impl Cfg {
    fn from_args() -> Cfg {
        let args: Vec<String> = env::args().skip(1).collect();
        let mut map: HashMap<String, String> = HashMap::new();
        let mut flags: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if let Some(rest) = args[i].strip_prefix("--") {
                if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    map.insert(rest.to_owned(), args[i + 1].clone());
                    i += 2;
                } else {
                    flags.push(rest.to_owned());
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        let list = |k: &str| -> Vec<String> {
            map.get(k)
                .map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default()
        };
        let pairs = |k: &str| -> Vec<(String, String)> {
            map.get(k)
                .map(|v| {
                    v.split(';')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(a, b)| (a.trim().to_ascii_lowercase(), b.trim().to_owned()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let listen = map.get("listen").cloned().unwrap_or_else(|| "127.0.0.1:18080".to_owned());
        env::set_var("CODEX_REC_LISTEN", listen);
        Cfg {
            upstream: map.get("upstream").cloned().unwrap_or_else(|| "https://chatgpt.com".to_owned()),
            log_dir: PathBuf::from(map.get("log-dir").cloned().unwrap_or_else(|| "/root/rec".to_owned())),
            drop_req: list("drop-req-header"),
            set_req: pairs("set-req-header"),
            drop_res: list("drop-res-header"),
            set_res: pairs("set-res-header"),
            body_drop: map
                .get("drop-body-key")
                .map(|v| v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default(),
            body_set: map
                .get("set-body-key")
                .map(|v| {
                    v.split(';')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(a, b)| (a.trim().to_owned(), b.trim().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            rewrite_catalog: flags.iter().any(|f| f == "rewrite-catalog"),
        }
    }
}

// ---------------------------------------------------------------- helpers

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn stamp() -> String {
    let n = now_ms();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{n}-{seq}")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn redact(name: &str, value: &str) -> String {
    let n = name.to_ascii_lowercase();
    if n == "authorization" || n == "x-api-key" || n == "cookie" {
        let head: String = value.chars().take(12).collect();
        return format!("{head}...[redacted {} bytes]", value.len());
    }
    value.to_owned()
}

/// Run the system `zstd` CLI: `mode` is either "-d" (decompress) or "-3" (compress).
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
    // Feed stdin from a thread: writing a big body while zstd fills its stdout pipe would
    // otherwise dead-lock both sides.
    let mut si = child.stdin.take()?;
    let data = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = si.write_all(&data);
        drop(si);
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
    let d = h.finalize();
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn decode_oailb(headers: &[String]) -> Option<String> {
    for h in headers {
        let lower = h.to_ascii_lowercase();
        if !lower.contains("__oailb=") {
            continue;
        }
        let start = lower.find("__oailb=")? + "__oailb=".len();
        let rest = &h[start..];
        let end = rest.find(|c: char| c == ';' || c == ' ' || c == '\r').unwrap_or(rest.len());
        let token = &rest[..end];
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() < 3 {
            continue;
        }
        let mut p = parts[1].replace('-', "+").replace('_', "/");
        while p.len() % 4 != 0 {
            p.push('=');
        }
        let decoded = base64_decode(&p)?;
        if let Ok(v) = serde_json::from_slice::<Value>(&decoded) {
            if let Some(host) = v.get("host").and_then(|h| h.as_str()) {
                return Some(host.to_owned());
            }
        }
    }
    None
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

// ---------------------------------------------------------------- extraction

fn summarize_request(body: &Value) -> Value {
    let mut kinds: HashMap<String, usize> = HashMap::new();
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
            if it.get("role").and_then(|v| v.as_str()) == Some("user") {
                let mut txt = String::new();
                if let Some(cs) = it.get("content").and_then(|v| v.as_array()) {
                    for c in cs {
                        if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                            txt.push_str(t);
                        }
                    }
                }
                msg_chars += txt.len();
                if !txt.is_empty() {
                    last_user = txt.chars().take(400).collect();
                }
            } else if let Some(t) = it.get("content").and_then(|v| v.as_array()) {
                for c in t {
                    if let Some(txt) = c.get("text").and_then(|v| v.as_str()) {
                        msg_chars += txt.len();
                    }
                }
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
        "client_metadata_keys": body.get("client_metadata").and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()),
        "last_user": last_user,
    })
}

fn summarize_response(text: &str) -> Value {
    let mut models: Vec<String> = Vec::new();
    let mut tiers: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut events: HashMap<String, usize> = HashMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut answer = String::new();
    let mut metadata_frames: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('{') && !t.starts_with("event:") {
            continue;
        }
        if t.starts_with("event:") {
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
        if v.get("type").and_then(|x| x.as_str()) == Some("response.output_text.delta") {
            if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                answer.push_str(d);
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

// ---------------------------------------------------------------- recording sink

struct Sink {
    file: Option<File>,
    acc: Vec<u8>,
    prefix: String,
    cfg: Arc<Cfg>,
    req_stamp: String,
    status: u16,
    #[allow(dead_code)]
    resp_headers: Vec<String>,
    origin: Option<String>,
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
        let path = self.cfg.log_dir.join(format!("resp-{}.summary.json", self.prefix));
        let _ = fs::write(&path, serde_json::to_vec_pretty(&summary).unwrap_or_default());
        let line = json!({
            "req": self.req_stamp,
            "status": self.status,
            "origin": self.origin,
            "models_seen": summary.get("models_seen"),
            "service_tiers_seen": summary.get("service_tiers_seen"),
            "response_ids": summary.get("response_ids"),
            "events": summary.get("events"),
            "errors": summary.get("errors"),
            "answer_chars": summary.get("answer_chars"),
        });
        let _ = append_line(&self.cfg.log_dir.join("index.jsonl"), &line.to_string());
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
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

async fn handle(State(cfg): State<Arc<Cfg>>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let req_stamp = stamp();
    let prefix = req_stamp.clone();

    let body_bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return text_response(400, &format!("body read error: {e}")),
    };

    // ---- record request
    let mut hdr_lines: Vec<String> = Vec::new();
    let mut out_headers = reqwest::header::HeaderMap::new();
    let mut cenc = None;
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        let val = v.to_str().unwrap_or("<non-utf8>").to_owned();
        hdr_lines.push(format!("{name}: {}", redact(&name, &val)));
        if name == "content-encoding" {
            cenc = Some(val.clone());
        }
        if HOP_BY_HOP.contains(&name.as_str()) || cfg.drop_req.contains(&name) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&val),
        ) {
            out_headers.insert(hn, hv);
        }
    }
    for (k, v) in cfg.set_req.iter() {
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            out_headers.insert(hn, hv);
            hdr_lines.push(format!("{k}: {v}   <- injected"));
        }
    }
    let _ = fs::write(
        cfg.log_dir.join(format!("req-{req_stamp}.hdr")),
        format!("{method} {uri}\n{}\n", hdr_lines.join("\n")),
    );
    let _ = fs::write(cfg.log_dir.join(format!("req-{req_stamp}.body")), &body_bytes);

    // ---- body rewriting (JSON only; zstd handled via the CLI)
    let mut body_out = body_bytes.to_vec();
    if !cfg.body_drop.is_empty() || !cfg.body_set.is_empty() {
        match decode_body(&body_bytes, cenc.as_deref()) {
            Some(decoded) => {
                match serde_json::from_slice::<Value>(&decoded) {
                    Ok(mut v) if v.is_object() => {
                        let obj = v.as_object_mut().unwrap();
                        for k in cfg.body_drop.iter() {
                            obj.remove(k);
                        }
                        for (k, raw) in cfg.body_set.iter() {
                            let parsed = serde_json::from_str::<Value>(raw).unwrap_or(Value::String(raw.clone()));
                            obj.insert(k.clone(), parsed);
                        }
                        let new = serde_json::to_vec(&v).unwrap_or_default();
                        body_out = match cenc {
                            Some(_) => zstd_cli("-3", &new).unwrap_or(new),
                            None => new,
                        };
                    }
                    _ => eprintln!("[body rewrite skipped] body is not a JSON object"),
                }
            }
            None => eprintln!("[body rewrite skipped] could not decode body"),
        }
    }

    // ---- request body extras
    if let Some(decoded) = decode_body(&body_bytes, cenc.as_deref()) {
        let _ = fs::write(cfg.log_dir.join(format!("req-{req_stamp}.json")), &decoded);
        if let Ok(v) = serde_json::from_slice::<Value>(&decoded) {
            let summary = summarize_request(&v);
            let _ = fs::write(
                cfg.log_dir.join(format!("req-{req_stamp}.summary.json")),
                serde_json::to_vec_pretty(&summary).unwrap_or_default(),
            );
            let line = json!({"req": req_stamp, "method": method.to_string(), "uri": uri.to_string(), "request": summary});
            let _ = append_line(&cfg.log_dir.join("index.jsonl"), &line.to_string());
        }
    }

    // ---- forward
    let url = format!("{}{}", cfg.upstream, uri);
    let client = CLIENT.get_or_init(build_client);
    let mut builder = client.request(method.clone(), &url).headers(out_headers);
    if !body_out.is_empty() {
        builder = builder.body(body_out);
    }
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return text_response(502, &format!("upstream error: {e}")),
    };

    let status = resp.status();
    let mut resp_hdr_lines: Vec<String> = vec![format!("HTTP {}", status.as_u16())];
    let mut rb = Response::builder().status(status);
    if let Some(hs) = rb.headers_mut() {
        for (k, v) in resp.headers().iter() {
            let name = k.as_str().to_ascii_lowercase();
            let val = v.to_str().unwrap_or("<non-utf8>").to_owned();
            resp_hdr_lines.push(format!("{name}: {val}"));
            if HOP_BY_HOP.contains(&name.as_str())
                || cfg.drop_res.contains(&name)
                || name == "content-length"
                || name == "transfer-encoding"
            {
                continue;
            }
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(&val),
            ) {
                hs.insert(hn, hv);
            }
        }
        for (k, v) in cfg.set_res.iter() {
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                hs.insert(hn, hv);
                resp_hdr_lines.push(format!("{k}: {v}   <- injected"));
            }
        }
    }
    let origin = decode_oailb(&resp_hdr_lines);
    if let Some(o) = origin.as_ref() {
        resp_hdr_lines.push(format!("[decoded] __oailb origin = {o}"));
    }
    let _ = fs::write(
        cfg.log_dir.join(format!("resp-{prefix}.hdr")),
        resp_hdr_lines.join("\n"),
    );

    // ---- catalog rewrite (buffered, non-streaming GET /models)
    if cfg.rewrite_catalog && uri.path().contains("/models") {
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
        prefix: prefix.clone(),
        cfg: Arc::clone(&cfg),
        req_stamp,
        status: status.as_u16(),
        resp_headers: resp_hdr_lines,
        origin,
    }));
    let stream = TeeStream {
        inner: Box::pin(resp.bytes_stream()),
        sink,
        done: false,
    };
    rb.body(Body::from_stream(stream))
        .unwrap_or_else(|_| text_response(500, "response build error"))
}

fn text_response(code: u16, msg: &str) -> Response {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn build_client() -> reqwest::Client {
    // codex on Linux = reqwest + native-tls (bundled OpenSSL) with no ALPN and HTTP/1.1.
    // Mirror exactly that, so the upstream ClientHello matches codex's.
    // NOTE: reqwest only configures ALPN on its rustls path; with native-tls it leaves ALPN
    // unset, which is exactly why the real codex client sends no ALPN extension either.
    let tls = native_tls::TlsConnector::builder()
        .build()
        .expect("failed to build native-tls connector");
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .pool_max_idle_per_host(2)
        .build()
        .expect("failed to build reqwest client")
}

#[tokio::main]
async fn main() {
    let cfg = Arc::new(Cfg::from_args());
    fs::create_dir_all(&cfg.log_dir).expect("cannot create log dir");
    let listen = env::var("CODEX_REC_LISTEN").unwrap_or_else(|_| "127.0.0.1:18080".to_owned());
    let app = Router::new().fallback(any(handle)).with_state(Arc::clone(&cfg));
    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind failed");
    println!("codex-rec listening on http://{listen} -> {}", cfg.upstream);
    println!("recording into {}", cfg.log_dir.display());
    println!(
        "config: drop_req={:?} set_req={:?} drop_res={:?} set_res={:?} body_drop={:?} catalog_rewrite={}",
        cfg.drop_req, cfg.set_req, cfg.drop_res, cfg.set_res, cfg.body_drop, cfg.rewrite_catalog
    );
    axum::serve(listener, app).await.expect("server error");
}
