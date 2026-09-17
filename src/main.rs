//! codex-rec: a recording + forwarding HTTP/1.1 reverse proxy for the Codex backend.
//!
//! v0.4.0 records one directory per codex session (see [`record`]), summarizes responses while they
//! stream (see [`summary`]), bounds memory with `[limits]` (see [`body`]) and lets every artifact be
//! switched off — `[record] enabled = false` turns it into a pure forwarder.
//!
//! TLS: the Linux codex client uses native-tls / a bundled OpenSSL and sends **no ALPN**, so the
//! default backend here reproduces that ClientHello byte-for-byte (measured JA3
//! `0b85eb0d4981e69064e40753e4f0ac5f` with SNI, `23211f2b48104c7030b93680a2efcfd0` without).
//! `tls.extension_order = "randomize"` switches to the rustls backend, whose ClientHello shuffles
//! the extension order per connection (OpenSSL has no such option).

mod body;
mod config;
mod envrewrite;
mod persona;
mod probe;
mod record;
mod session;
mod summary;
mod timeutil;
mod tsgrab;
mod tsscan;
mod tz;

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::Stream;
use serde_json::{json, Value};

use config::{Config, ExtensionOrder};
use record::{thread_from_metadata, Recorder};
use session::Sessions;
use summary::{summarize_request, Summarizer};

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
    recorder: Arc<Recorder>,
    sessions: Option<Arc<Sessions>>,
}

// ---------------------------------------------------------------- helpers

fn now_ms() -> u128 {
    timeutil::now_ms()
}

fn redact(name: &str, value: &str) -> String {
    let n = name.to_ascii_lowercase();
    if n == "authorization" || n == "x-api-key" || n == "cookie" {
        let head: String = value.chars().take(12).collect();
        return format!("{head}...[redacted {} bytes]", value.len());
    }
    value.to_owned()
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
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
pub(crate) fn decode_oailb(headers: &[String]) -> Option<String> {
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

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
}

/// Writes a file only when there is something to write (empty artifacts are skipped).
fn write_if_any(path: &Path, bytes: &[u8], buffered: usize) {
    if bytes.is_empty() {
        return;
    }
    if buffered == 0 {
        let _ = fs::write(path, bytes);
        return;
    }
    if let Ok(file) = File::create(path) {
        let mut writer = BufWriter::with_capacity(buffered, file);
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }
}

fn header_dump(lines: &[String]) -> String {
    format!("{}\n", lines.join("\n"))
}

fn text_response(code: u16, msg: &str) -> Response {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_owned()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

// ---------------------------------------------------------------- tee

struct Sink {
    cfg: Config,
    plan: Option<record::Plan>,
    file: Option<BufWriter<File>>,
    recorded: Option<String>,
    summarizer: Option<Summarizer>,
    status: u16,
    origin: Option<String>,
    index_row: Option<Value>,
    session_id: Option<String>,
    sessions: Option<Arc<Sessions>>,
    spilled: Option<std::path::PathBuf>,
    finished: bool,
}

impl Sink {
    fn write_chunk(&mut self, chunk: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(chunk);
        }
        if let Some(s) = self.summarizer.as_mut() {
            s.push(chunk);
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
        let summary = match self.summarizer.as_mut() {
            Some(s) => s.finish(self.recorded.clone()),
            None => Value::Null,
        };

        let mut index = self.index_row.take().unwrap_or_else(|| json!({}));
        if let Some(obj) = index.as_object_mut() {
            obj.insert("status".to_owned(), json!(self.status));
            obj.insert("origin".to_owned(), json!(self.origin));
            obj.insert("response_file".to_owned(), json!(self.recorded));
            obj.insert("models_seen".to_owned(), summary.get("models_seen").cloned().unwrap_or(Value::Null));
            obj.insert(
                "service_tiers_seen".to_owned(),
                summary.get("service_tiers_seen").cloned().unwrap_or(Value::Null),
            );
            obj.insert("response_ids".to_owned(), summary.get("response_ids").cloned().unwrap_or(Value::Null));
            obj.insert("events".to_owned(), summary.get("events").cloned().unwrap_or(Value::Null));
            obj.insert("errors".to_owned(), summary.get("errors").cloned().unwrap_or(Value::Null));
            obj.insert("usage".to_owned(), summary.get("usage").cloned().unwrap_or(Value::Null));
            obj.insert(
                "answer_chars".to_owned(),
                summary.get("answer_chars").cloned().unwrap_or(Value::Null),
            );
        }

        if self.cfg.record.index {
            let _ = append_line(&self.cfg.log_dir.join("index.jsonl"), &index.to_string());
        }
        if let Some(plan) = self.plan.as_ref() {
            if self.cfg.record.response_summary {
                let _ = write_if_any(
                    &plan.resp("summary.json"),
                    serde_json::to_vec_pretty(&summary).unwrap_or_default().as_slice(),
                    self.cfg.limits.write_buffer_bytes,
                );
            }
        }
        if let (Some(sessions), Some(session)) = (self.sessions.as_ref(), self.session_id.as_deref()) {
            sessions.append(
                session,
                &json!({
                    "type": "response",
                    "ts": now_ms(),
                    "status": self.status,
                    "origin": self.origin,
                    "dir": self.plan.as_ref().map(|p| p.dir().to_string_lossy().to_string()),
                    "response_file": self.recorded,
                    "summary": summary,
                }),
            );
        }
        if let Some(path) = self.spilled.take() {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // A client that disconnects mid-stream still gets a summary + index row.
        self.finish();
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

async fn handle(State(app): State<Arc<App>>, req: axum::extract::Request) -> Response {
    let cfg = &app.cfg;
    let record = &app.recorder.record;
    let (parts, incoming_body) = req.into_parts();
    let method = parts.method.clone();

    // `uri_in` is what the client asked for (logged as-is); `uri` is what we send upstream after the
    // [routes] path mapping. Both are recorded, so the mapping stays auditable.
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

    let session_id = parts
        .headers
        .get("session-id")
        .or_else(|| parts.headers.get("thread-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let thread = thread_from_metadata(
        parts
            .headers
            .get("x-codex-turn-metadata")
            .and_then(|v| v.to_str().ok()),
    );
    let plan = app
        .recorder
        .enabled()
        .then(|| app.recorder.plan(session_id.as_deref(), &thread));

    // ---- incoming header capture + outgoing header assembly
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
    if let Some(plan) = plan.as_ref() {
        if record.request_headers {
            let _ = write_if_any(
                &plan.req("hdr"),
                format!("{method} {uri_in}\n{}", header_dump(&incoming_lines)).as_bytes(),
                cfg.limits.write_buffer_bytes,
            );
        }
    }

    // ---- device persona, then configured header rewrites
    let persona_notes = persona::apply(cfg, &mut out_headers, &mut uri);
    for (name, value) in cfg.request_sets.iter() {
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            out_headers.insert(hn, hv);
        }
    }
    // `set_from_file` is re-read per request: that is what lets the turn-state be rotated while the
    // proxy keeps running (see the tsauto loop).
    let mut file_set_notes: Vec<(String, String)> = Vec::new();
    let mut file_set_errors: Vec<String> = Vec::new();
    for (name, path) in cfg.request_sets_files.iter() {
        match config::read_header_value_file(path) {
            Ok(Some(value)) => {
                if let (Ok(hn), Ok(hv)) = (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                    reqwest::header::HeaderValue::from_str(&value),
                ) {
                    out_headers.insert(hn, hv);
                    file_set_notes.push((name.clone(), redact(name, &value)));
                }
            }
            // An empty file is the documented "send nothing" state: not an error, not a note.
            Ok(None) => {}
            // Anything else must be visible. A malformed value file used to look identical to
            // "the file was not there", which hid a broken rotation for hours.
            Err(e) => file_set_errors.push(format!("{name} from {}: {e}", path.display())),
        }
    }
    for e in file_set_errors.iter() {
        eprintln!("[headers.request] set_from_file: {e}");
    }
    if let Some(plan) = plan.as_ref() {
        if record.request_headers_out {
            let mut out_lines: Vec<String> = vec![format!("{method} {uri}"), format!("x-incoming-path: {in_path}")];
            // Configured headers are reported as comments, and then the header itself is listed
            // once from the real map below. They used to be printed as if they were headers *and*
            // repeated as headers, so a grep for the name found two identical-looking lines and it
            // was impossible to tell which value actually went out -- which is precisely the
            // question this file exists to answer.
            for (name, value) in cfg.request_sets.iter() {
                out_lines.push(format!(
                    "# set by config: {name}: {}",
                    redact(name, value)
                ));
            }
            for (name, value) in file_set_notes.iter() {
                out_lines.push(format!("# set from file: {name}: {value}"));
            }
            for e in file_set_errors.iter() {
                out_lines.push(format!("# set_from_file error: {e}"));
            }
            let configured = |name: &str| -> bool {
                cfg.request_sets.iter().any(|(s, _)| s.eq_ignore_ascii_case(name))
                    || cfg.request_sets_files.iter().any(|(s, _)| s.eq_ignore_ascii_case(name))
            };
            for (k, v) in out_headers.iter() {
                let name = k.as_str().to_ascii_lowercase();
                if configured(&name) {
                    continue; // already reported above, as a comment with its provenance
                }
                out_lines.push(format!("{name}: {}", redact(&name, v.to_str().unwrap_or("<non-utf8>"))));
            }
            for note in persona_notes.iter() {
                out_lines.push(format!("# persona: {note}"));
            }
            let _ = write_if_any(
                &plan.req("out.hdr"),
                header_dump(&out_lines).as_bytes(),
                cfg.limits.write_buffer_bytes,
            );
        }
    }

    // ---- request body: stream it through, or buffer/spill it
    let rewrites_env = cfg.rewrite.environment.is_active();
    let rewrites_body = !cfg.body_drop.is_empty() || !cfg.body_set.is_empty() || rewrites_env;
    let want_body = rewrites_body || (record.enabled && app.recorder.needs_request_decode());
    let mut spilled: Option<std::path::PathBuf> = None;
    let mut body_bytes: Vec<u8> = Vec::new();
    let mut streamed_body = None;
    let mut body_over_limit = false;
    let mut body_total: u64 = 0;

    if want_body {
        let policy = body::Policy::parse(&cfg.limits.request_body_over_limit).unwrap_or(body::Policy::Spill);
        let spill_target = std::env::temp_dir().join(format!(
            "codex-rec-{}.body",
            plan.as_ref().map(|p| p.stem().to_owned()).unwrap_or_else(|| timeutil::stamp())
        ));
        match body::read(incoming_body, cfg.limits.request_body_bytes, policy, &spill_target).await {
            Ok(buffered) => {
                body_over_limit = buffered.over_limit;
                body_total = buffered.total;
                if buffered.over_limit {
                    eprintln!(
                        "[request body over limits.request_body_bytes] {} bytes spilled to a file",
                        buffered.total
                    );
                }
                body_bytes = buffered.head.clone();
                if let Some(path) = buffered.spill {
                    let mut forward_path = path.clone();
                    let mut delete_after = true;
                    if record.enabled && record.request_body_raw && cenc.is_some() {
                        // the spilled raw body *is* the recorded artifact: move it into place
                        if let Some(plan) = plan.as_ref() {
                            let dest = plan.req("body");
                            let moved = fs::rename(&path, &dest).is_ok() || {
                                let copied = fs::copy(&path, &dest).is_ok();
                                if copied {
                                    let _ = fs::remove_file(&path);
                                }
                                copied
                            };
                            if moved {
                                forward_path = dest;
                                delete_after = false;
                            }
                        }
                    }
                    streamed_body = Some(reqwest::Body::wrap_stream(body::file_body(forward_path)));
                    spilled = delete_after.then_some(path);
                }
            }
            Err(e) => {
                let code = match e {
                    body::BodyError::TooLarge(_) => 413,
                    _ => 400,
                };
                return text_response(code, &e.message());
            }
        }
    } else {
        // nothing has to look at the body: pass it through without buffering it (pure-forwarder path)
        streamed_body = Some(reqwest::Body::wrap_stream(incoming_body.into_data_stream()));
    }

    // ---- body decode / rewrite
    let mut body_out: Vec<u8> = body_bytes.clone();
    let mut rewritten = false;
    let mut env_notes_detail: Vec<String> = Vec::new();
    if rewrites_body {
        match summary::decode_body(&body_bytes, cenc.as_deref()) {
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
                    if rewrites_env {
                        let env_notes = envrewrite::apply_env(&cfg.rewrite.environment, &mut v, None);
                        if env_notes.changed {
                            env_notes_detail = env_notes.detail.clone();
                            for line in env_notes.detail.iter() {
                                println!("[rewrite] {line}");
                            }
                        }
                    }
                    let new = serde_json::to_vec(&v).unwrap_or_default();
                    if cenc.is_some() {
                        // The client compressed this body, so the upstream expects a zstd frame.
                        // If we cannot re-compress (no zstd CLI) we must NOT hand over plain bytes
                        // together with `content-encoding: zstd` — that would corrupt the request.
                        match summary::zstd_encode(&new) {
                            Some(recompressed) => {
                                body_out = recompressed;
                                rewritten = true;
                            }
                            None => {
                                eprintln!(
                                    "[body rewrite skipped] the body was zstd-compressed and cannot be                                      re-compressed without the zstd CLI: set $ZSTD or put zstd(.exe) on                                      PATH (the request was forwarded unchanged)"
                                );
                            }
                        }
                    } else {
                        body_out = new;
                        rewritten = true;
                    }
                }
                _ => eprintln!("[body rewrite skipped] body is not a JSON object"),
            },
            None => eprintln!(
                "[body rewrite skipped] could not decode the body: set $ZSTD or put zstd(.exe) on PATH                  (the request was forwarded unchanged)"
            ),
        }
    }

    // ---- request artifacts
    let mut request_summary = Value::Null;
    let mut request_json_file: Option<String> = None;
    if let Some(plan) = plan.as_ref() {
        let compressed = cenc.is_some() && body_bytes.len() < 4 * 1024 * 1024;
        if record.request_body_raw && compressed && spilled.is_none() {
            write_if_any(&plan.req("body"), &body_bytes, cfg.limits.write_buffer_bytes);
        }
        if record.request_body_out && rewritten && !body_out.is_empty() {
            write_if_any(&plan.req("out.json"), &body_out, cfg.limits.write_buffer_bytes);
        }
        if (record.request_body_json || record.request_summary)
            && !body_bytes.is_empty()
            && !body_over_limit
        {
            if let Some(decoded) = summary::decode_body(&body_bytes, cenc.as_deref()) {
                if record.request_body_json && !decoded.is_empty() {
                    write_if_any(&plan.req("json"), &decoded, cfg.limits.write_buffer_bytes);
                    request_json_file = Some(plan.req_name("json"));
                }
                if let Ok(v) = serde_json::from_slice::<Value>(&decoded) {
                    request_summary = summarize_request(&v);
                    if record.request_summary {
                        let _ = write_if_any(
                            &plan.req("summary.json"),
                            serde_json::to_vec_pretty(&request_summary)
                                .unwrap_or_default()
                                .as_slice(),
                            cfg.limits.write_buffer_bytes,
                        );
                    }
                }
            }
        }
    }

    let index_row = if record.index {
        json!({
            "req": plan.as_ref().map(|p| p.stem().to_owned()),
            "session": session_id,
            "thread": thread,
            "dir": plan.as_ref().map(|p| p.dir().to_string_lossy().to_string()),
            "method": method.to_string(),
            "uri_in": uri_in,
            "uri": uri,
            "persona": persona_notes,
            "request": request_summary,
            "request_file": request_json_file,
            "env_rewrite": env_notes_detail,
            "request_body_bytes": body_total,
            "request_body_spilled": body_over_limit,
        })
    } else {
        Value::Null
    };

    if let (Some(sessions), Some(session)) = (app.sessions.as_ref(), session_id.as_deref()) {
        if let Some(text) = request_summary.get("last_user").and_then(|v| v.as_str()) {
            sessions.note_title(session, text);
        }
        sessions.append(
            session,
            &json!({
                "type": "request",
                "ts": now_ms(),
                "uri_in": index_row.get("uri_in"),
                "uri": index_row.get("uri"),
                "persona": index_row.get("persona"),
                "dir": plan.as_ref().map(|p| p.dir().to_string_lossy().to_string()),
                "body_file": request_json_file,
                "summary_file": if record.request_summary && request_summary != Value::Null {
                    plan.as_ref().map(|p| p.req_name("summary.json"))
                } else {
                    None
                },
                "request": request_summary,
            }),
        );
    }

    // ---- forward
    let url = format!("{}{}", cfg.upstream, uri);
    let mut builder = app.client.request(method.clone(), &url).headers(out_headers);
    if let Some(stream) = streamed_body {
        builder = builder.body(stream);
    } else if !body_out.is_empty() {
        builder = builder.body(body_out);
    }
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            if let Some(path) = spilled {
                let _ = fs::remove_file(path);
            }
            return text_response(502, &format!("upstream error: {e}"));
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let etag = resp
        .headers()
        .get("x-models-etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let resp_encoding = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let mut resp_lines: Vec<String> = vec![format!("HTTP {}", status.as_u16())];
    let mut rb = Response::builder().status(status);
    if let Some(hs) = rb.headers_mut() {
        for (k, v) in resp.headers().iter() {
            let name = k.as_str().to_ascii_lowercase();
            let value = v.to_str().unwrap_or("<non-utf8>").to_owned();
            resp_lines.push(format!("{name}: {}", redact(&name, &value)));
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
        for (name, value) in cfg.response_sets.iter() {
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                hs.insert(hn, hv);
                resp_lines.push(format!("{name}: {value}   <- set by config"));
            }
        }
        for (name, path) in cfg.response_sets_files.iter() {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            let value = text.trim();
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                hs.insert(hn, hv);
                resp_lines.push(format!("{name}: {value}   <- set from file"));
            }
        }
    }
    let origin = decode_oailb(&resp_lines);
    if let Some(o) = origin.as_ref() {
        resp_lines.push(format!("[decoded] __oailb origin = {o}"));
    }
    let is_catalog = uri.contains("/models");
    if let Some(plan) = plan.as_ref() {
        if record.response_headers {
            let _ = write_if_any(
                &plan.resp("hdr"),
                header_dump(&resp_lines).as_bytes(),
                cfg.limits.write_buffer_bytes,
            );
        }
    }

    // ---- optional catalog rewrite (buffered, non-streaming)
    if cfg.rewrite_catalog && is_catalog {
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return text_response(502, &format!("upstream read error: {e}")),
        };
        let decoded = summary::decode_body(&bytes, resp_encoding.as_deref()).unwrap_or_else(|| bytes.to_vec());
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
        if let Some(plan) = plan.as_ref() {
            let _ = write_if_any(
                &plan.resp("catalog.json"),
                &rewritten,
                cfg.limits.write_buffer_bytes,
            );
        }
        if let (Some(sessions), Some(session)) = (app.sessions.as_ref(), session_id.as_deref()) {
            sessions.append(
                session,
                &json!({"type": "catalog_rewrite", "ts": now_ms(), "bytes": rewritten.len()}),
            );
        }
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(rewritten))
            .unwrap_or_else(|_| text_response(500, "response build error"));
    }

    // ---- response body: record + summarize, or pass straight through
    let keep_stream = app.recorder.tracks_response() && (!is_catalog || record.response_stream_catalog);
    let want_summary = app.recorder.enabled() && record.response_summary;
    if !keep_stream && !want_summary {
        if let Some(path) = spilled {
            let _ = fs::remove_file(path);
        }
        return rb
            .body(Body::from_stream(resp.bytes_stream()))
            .unwrap_or_else(|_| text_response(500, "response build error"));
    }

    let mut summarizer = if want_summary || !keep_stream {
        let mut s = Summarizer::new(
            content_type.as_deref(),
            cfg.limits.summary_buffer_bytes,
            plan.as_ref().map(|p| p.resp_name("stream.sse")).unwrap_or_default(),
        );
        s.set_etag(etag.clone());
        s.set_response_encoding(resp_encoding.clone());
        Some(s)
    } else {
        None
    };
    if let Some(s) = summarizer.as_mut() {
        if !s.is_sse() && is_catalog && !record.response_stream_catalog {
            s.mark_stream_skipped();
        }
    }

    let recorded = if keep_stream {
        plan.as_ref().map(|p| {
            if content_type
                .as_deref()
                .map(|c| c.to_ascii_lowercase().contains("event-stream"))
                .unwrap_or(false)
            {
                p.resp_name("stream.sse")
            } else if is_catalog {
                p.resp_name("stream.json")
            } else {
                p.resp_name("stream.bin")
            }
        })
    } else {
        None
    };
    let file = match (keep_stream, plan.as_ref(), recorded.as_ref()) {
        (true, Some(plan), Some(name)) => File::create(plan.dir().join(name))
            .ok()
            .map(|f| {
                if cfg.limits.write_buffer_bytes == 0 {
                    BufWriter::with_capacity(8 * 1024, f)
                } else {
                    BufWriter::with_capacity(cfg.limits.write_buffer_bytes, f)
                }
            }),
        _ => None,
    };

    let sink = Arc::new(Mutex::new(Sink {
        cfg: cfg.clone(),
        plan: plan.clone(),
        file,
        recorded: recorded.clone(),
        summarizer: summarizer.take(),
        status: status.as_u16(),
        origin,
        index_row: Some(index_row),
        session_id: session_id.clone(),
        sessions: app.sessions.clone(),
        spilled,
        finished: false,
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

/// The HTTP client builder `tsgrab` uses: the *same* TLS stack and extension order as forwarding, so
/// a grabbed request leaves the box with codex's own ClientHello.
///
/// Two things are specific to probing:
///
/// * `source` — binds the local socket, so an IPv6 prefix (or one v4 address of many) can be
///   exercised without touching the host's routing table;
/// * `proxy` — same surface as codex's own `HTTPS_PROXY`, including SOCKS5.
pub(crate) fn tsgrab_http_builder(
    timeout_secs: f64,
    insecure: bool,
    source: Option<std::net::IpAddr>,
    proxy: Option<&str>,
) -> reqwest::ClientBuilder {
    let mut tls = native_tls::TlsConnector::builder();
    if insecure {
        tls.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
    }
    let tls = tls.build().expect("failed to build native-tls connector");
    let mut b = base_builder()
        .use_preconfigured_tls(tls)
        .timeout(Duration::from_secs_f64(timeout_secs.max(1.0)));
    if let Some(ip) = source {
        b = b.local_address(ip);
    }
    if let Some(p) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
        match reqwest::Proxy::all(p) {
            Ok(px) => b = b.proxy(px),
            Err(e) => eprintln!("warning: --proxy {p:?} rejected: {e}"),
        }
    }
    b
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
    // `probe` is a subcommand: it prints this machine's codex environment and a ready-to-paste
    // config without starting the proxy (see src/probe.rs).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("probe") {
        std::process::exit(probe::run(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("tsgrab") {
        // already inside the tokio runtime that #[tokio::main] set up -- do NOT nest one
        let code = match tsgrab::run(&argv[1..]).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("tsgrab: {e}");
                1
            }
        };
        std::process::exit(code);
    }
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
    let record = cfg.record.clone();
    if record.enabled {
        println!(
            "recording into {} (session dirs; retention={:?}d gzip={:?}d max={:?}B)",
            cfg.log_dir.display(),
            record.retention_days,
            record.gzip_after_days,
            record.max_total_bytes
        );
        println!(
            "record switches: index={} req.hdr={} req.out.hdr={} req.body={} req.json={} req.summary={} resp.hdr={} resp.stream={} resp.catalog_stream={} resp.summary={}",
            record.index,
            record.request_headers,
            record.request_headers_out,
            record.request_body_raw,
            record.request_body_json,
            record.request_summary,
            record.response_headers,
            record.response_stream,
            record.response_stream_catalog,
            record.response_summary
        );
    } else {
        println!("recording disabled ([record] enabled = false): pure forwarder");
    }
    println!(
        "limits: request_body={} bytes (over-limit={}), summary_buffer={} bytes, write_buffer={} bytes",
        cfg.limits.request_body_bytes,
        cfg.limits.request_body_over_limit,
        cfg.limits.summary_buffer_bytes,
        cfg.limits.write_buffer_bytes
    );
    println!(
        "routes: strip {:?} -> prefix {:?}",
        cfg.routes.strip_prefixes, cfg.routes.upstream_prefix
    );
    let env = &cfg.rewrite.environment;
    if env.is_active() {
        println!(
            "rewrite: environment timezone={:?} current_date={:?} cwd={:?} shell={:?} drop={:?} fill_missing={}",
            env.timezone, env.current_date, env.cwd, env.shell, env.drop, env.fill_missing
        );
    } else {
        println!("rewrite: no environment overrides (the client's values pass through)");
    }
    if !cfg.request_sets_files.is_empty() {
        println!(
            "headers from file (re-read per request): {:?}",
            cfg.request_sets_files
        );
    }
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
    let recorder = Arc::new(Recorder::new(
        cfg.log_dir.clone(),
        cfg.record.clone(),
        cfg.limits.clone(),
    ));
    if recorder.enabled() {
        let pruner = Arc::clone(&recorder);
        tokio::spawn(async move {
            loop {
                println!("{}", pruner.prune());
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }
    let client = build_client(&cfg);
    let app = Arc::new(App {
        cfg,
        client,
        recorder,
        sessions,
    });
    let listen = app.cfg.listen.clone();
    let router = Router::new().fallback(any(handle)).with_state(Arc::clone(&app));
    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind failed");
    axum::serve(listener, router).await.expect("server error");
}
