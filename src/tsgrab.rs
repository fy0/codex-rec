//! Probe the Codex backend for a `x-codex-turn-state` without paying for the generation.
//!
//! The token is issued as a **response header**, i.e. before any output token exists. The recipe is
//! therefore: send a real request, read the header block, abort the connection — the generation is
//! never completed, so nothing is billed.
//!
//! Every attempt:
//!
//! 1. build the request (headers copied from the recorded template, only the ids regenerated);
//! 2. `send()` — returns as soon as the **response head** is in;
//! 3. read `x-codex-turn-state`, compare its base64 length against `--want-lengths`;
//! 4. drop the response body **without polling it**: reqwest discards the connection instead of
//!    returning it to the pool, which stops the upstream transfer.
//!
//! On a hit the raw token goes to stdout and the metadata to `--out`. `--quiet` prints nothing but
//! the token, so the probe composes with other scripts:
//!
//! ```text
//! TOK=$(codex-rec tsgrab --body-file x.req.body --want-lengths 292 --quiet)
//! ```
//!
//! Egress can be pinned:
//!
//! * `--interface eth0` / `--source-ip 2001:db8::1` — bind the local address, so an IPv6 prefix
//!   (or a specific v4 address) can be exercised without touching the host's default route;
//! * `--proxy http://…` / `--proxy socks5://…` — same surface as codex's own `HTTPS_PROXY`.
//!
//! Credentials are read from an `auth.json` in codex's own shape
//! (`{"tokens":{"access_token":…,"account_id":…}}`) so this composes with the rest of the tooling.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub(crate) use crate::base64_decode;

// --------------------------------------------------------------------------- request spec

/// A request that, when varied, almost certainly matches `--vary`.
#[derive(Debug, Default)]
pub struct VarySpec {
    /// `x-codex-turn-state` to send instead of the template's own.
    pub turn_state: Option<String>,
    /// Field name to blank out. `*` patch-fills every match.
    pub field: Option<String>,
    /// `old=new` replacements inside the decoded body. `pat` replaces the first `"model":"…"`
    /// occurrence; `sid` swaps every session/thread/turn id for fresh ones.
    pub patch: Vec<(String, String)>,
}

impl VarySpec {
    fn is_active(&self) -> bool {
        self.turn_state.is_some() || self.field.is_some() || !self.patch.is_empty()
    }
}

#[derive(Debug)]
pub struct Args {
    pub url: String,
    pub body: PathBuf,
    /// Accepted base64 lengths, e.g. `292` or `288,292`; empty / `any` accepts any token.
    pub want: Vec<String>,
    pub attempts: u32,
    pub gap_ms: u64,
    pub timeout: f64,
    pub headers: Vec<(String, String)>,
    pub out: Option<PathBuf>,
    /// Print just the token (for `$(...)` composition).
    pub quiet: bool,
    pub insecure: bool,
    /// Bind the outgoing socket to this local address (an IPv6 prefix member, or a v4 address).
    pub source_ip: Option<String>,
    /// Resolve this interface's addresses and pick one (address family follows `--prefer-family`).
    pub interface: Option<String>,
    pub prefer_family: Option<String>,
    /// `http://…`, `https://…` or `socks5[h]://…`
    pub proxy: Option<String>,
    pub auth: Option<PathBuf>,
    pub vary: VarySpec,
    /// `codex-rec.toml` (or any file with `[rewrite.environment]`) whose environment rewrite is
    /// applied to the outgoing body -- so a probe looks like a forwarded request, not like a
    /// verbatim replay of a stale template.
    pub config: Option<PathBuf>,
    /// Print the template's decoded body before sending (debugging `--vary`).
    pub dump_template: bool,
    /// Where to look for recent turn-states to reuse before probing. `Some(vec![])` = default tree.
    pub scan_dirs: Vec<PathBuf>,
    /// Reuse a scanned token instead of probing when one is younger than this many seconds.
    pub scan_fresh_within: u64,
    /// Only scan: print the best token and exit (no request at all).
    pub scan_only: bool,
    /// Stop reading after this many usable candidates (default 1: we only need the freshest).
    pub scan_max: usize,
    /// Include tokens that are already installed in a config (default: ignore them).
    pub scan_anyway: bool,
}

const FAKE_ACCOUNT: &str = "00000000-0000-4000-8000-000000000000";
const FAKE_TOKEN: &str = "FAKE0000000000000000000000000000000000000000000000000000000000";

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| {
        std::env::var(n)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        url: env_first(&["TS_URL", "CODEX_TS_URL"])
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex/responses".to_owned()),
        body: PathBuf::new(),
        want: vec![],
        attempts: 1,
        gap_ms: 1500,
        timeout: 60.0,
        headers: Vec::new(),
        out: None,
        quiet: false,
        insecure: false,
        source_ip: None,
        interface: None,
        prefer_family: None,
        proxy: None,
        auth: None,
        vary: VarySpec::default(),
        config: env_first(&["CODEX_REC_CONFIG"]).map(PathBuf::from),
        dump_template: false,
        scan_dirs: Vec::new(),
        scan_fresh_within: 1800,
        scan_only: false,
        scan_max: 1,
        scan_anyway: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let value = |i: usize| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        let take = |i: &mut usize| -> Result<String, String> {
            let v = value(*i)?;
            *i += 2;
            Ok(v)
        };
        match argv[i].as_str() {
            "--url" => args.url = take(&mut i)?,
            "--body-file" => args.body = PathBuf::from(take(&mut i)?),
            "--want-lengths" | "--want" => {
                let raw = take(&mut i)?;
                args.want = raw
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty() && s != "any")
                    .collect();
            }
            "--attempts" => args.attempts = take(&mut i)?.parse().map_err(|e| format!("--attempts: {e}"))?,
            "--gap-ms" => args.gap_ms = take(&mut i)?.parse().map_err(|e| format!("--gap-ms: {e}"))?,
            "--timeout" => args.timeout = take(&mut i)?.parse().map_err(|e| format!("--timeout: {e}"))?,
            "--header" => {
                let raw = take(&mut i)?;
                let (k, v) = raw
                    .split_once(':')
                    .ok_or_else(|| format!("--header expects \"name: value\", got {raw:?}"))?;
                args.headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
            }
            "--inject" => args.vary.turn_state = Some(take(&mut i)?),
            "--inject-from" => {
                let path = PathBuf::from(take(&mut i)?);
                args.vary.turn_state = Some(
                    token_from_hdr_file(&path)
                        .ok_or_else(|| format!("no x-codex-turn-state in {}", path.display()))?,
                );
            }
            "--field" => args.vary.field = Some(take(&mut i)?),
            "--patch" => {
                let raw = take(&mut i)?;
                let (a, b) = raw
                    .split_once('=')
                    .ok_or_else(|| format!("--patch expects old=new, got {raw:?}"))?;
                args.vary.patch.push((a.to_owned(), b.to_owned()));
            }
            "--vary" => {
                args.vary.field = Some("*".to_owned());
                args.vary.patch.push(("sid".to_owned(), String::new()));
            }
            "--source-ip" => args.source_ip = Some(take(&mut i)?),
            "--interface" => args.interface = Some(take(&mut i)?),
            "--prefer-family" => args.prefer_family = Some(take(&mut i)?),
            "--proxy" => args.proxy = Some(take(&mut i)?),
            "--auth" => args.auth = Some(PathBuf::from(take(&mut i)?)),
            "--config" => args.config = Some(PathBuf::from(take(&mut i)?)),
            "--scan-dir" => args.scan_dirs.push(PathBuf::from(take(&mut i)?)),
            "--scan-fresh-within" => {
                args.scan_fresh_within = take(&mut i)?
                    .parse()
                    .map_err(|e| format!("--scan-fresh-within: {e}"))?
            }
            "--scan-only" => {
                args.scan_only = true;
                i += 1;
            }
            "--scan-anyway" => {
                args.scan_anyway = true;
                i += 1;
            }
            "--scan-max" => {
                args.scan_max = take(&mut i)?
                    .parse()
                    .map_err(|e| format!("--scan-max: {e}"))?
            }
            "--scan-all" => {
                args.scan_max = 0; // 0 = no cap
                i += 1;
            }
            "--out" => args.out = Some(PathBuf::from(take(&mut i)?)),
            "--record" => {
                let dir = PathBuf::from(take(&mut i)?);
                args.out = Some(dir.join("ts-grab.json"));
            }
            "--quiet" | "-q" => {
                args.quiet = true;
                i += 1;
            }
            "--insecure" => {
                args.insecure = true;
                i += 1;
            }
            "--dump-template" => {
                args.dump_template = true;
                i += 1;
            }
            other => return Err(format!("unknown tsgrab argument {other:?}")),
        }
    }
    if args.body.as_os_str().is_empty() {
        return Err("--body-file <captured .req.body> is required".to_owned());
    }
    Ok(args)
}

/// One attempt is a fresh identity, so a length probe is not fooled by a stale session.
#[derive(Debug, Clone)]
pub struct Identity {
    pub session: String,
    pub turn: String,
}

/// RFC-4122-shaped, time-ordered id, matching what the codex client generates (`01a0…`).
pub fn new_uuid() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        & ((1 << 48) - 1);
    let mut b = [0u8; 16];
    b[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    let noise = mix(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0));
    b[6..8].copy_from_slice(&(0x7000u16 | ((noise >> 48) as u16 & 0x0fff)).to_be_bytes());
    b[8..16].copy_from_slice(&noise.to_be_bytes());
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Cheap deterministic mixer (xorshift); we only need uniqueness, not cryptography.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

pub fn identity() -> Identity {
    Identity { session: new_uuid(), turn: new_uuid() }
}

// --------------------------------------------------------------------------- token helpers

pub fn token_len(token: &str) -> usize {
    token.len()
}

/// Version byte, then a u64 big-endian issue time (see docs/fingerprint.md).
pub fn token_issued(token: &str) -> Option<u64> {
    let raw = base64_decode(&token.replace('-', "+").replace('_', "/"))?;
    if raw.len() < 9 || raw[0] != 0x80 {
        return None;
    }
    Some(u64::from_be_bytes([
        raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7], raw[8],
    ]))
}

/// Short, non-reversible id, so logs never carry the token itself.
pub fn token_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// Reads the token out of a recorder header dump, so a previous grab can be replayed.
pub(crate) fn token_from_hdr_file(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let at = lower.find("x-codex-turn-state:")?;
        let rest = line[at + "x-codex-turn-state:".len()..].trim();
        let value = rest.split_whitespace().next()?;
        if value.len() > 100 {
            return Some(value.to_owned());
        }
    }
    None
}

/// Sibling header file of a `.req.body`, as the recorder writes it.
pub fn sibling_header(body: &Path) -> Option<PathBuf> {
    let name = body.file_name()?.to_string_lossy().to_string();
    let base = name.strip_suffix(".req.body")?;
    for suffix in [".req.out.hdr", ".req.hdr"] {
        let cand = body.with_file_name(format!("{base}{suffix}"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Everything a template header file carried, minus what reqwest owns and minus any turn-state
/// that `--inject` is going to replace.
fn headers_from_template(body: &Path, inject: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(hdr) = sibling_header(body) else { return out };
    let Ok(text) = fs::read_to_string(&hdr) else { return out };
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("post ") || lower.starts_with("get ") || lower.starts_with("x-incoming-path") {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if value.is_empty() {
            continue;
        }
        match name.as_str() {
            "host" | "connection" | "content-length" | "transfer-encoding" => continue,
            "x-codex-turn-state" if inject.is_some() => continue,
            _ => {}
        }
        out.push((name, value));
    }
    out
}

// --------------------------------------------------------------------------- auth.json

/// What we take from `auth.json`: codex's own layout, so `~/.codex/auth.json` works as-is.
struct Auth {
    access: Option<String>,
    account: Option<String>,
    /// The file may only hold an API key (no `tokens` object).
    api_key: Option<String>,
}

fn read_auth(path: &Path) -> Result<Auth, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read --auth {}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("--auth {} is not JSON: {e}", path.display()))?;
    let tokens = v.get("tokens");
    Ok(Auth {
        access: tokens
            .and_then(|t| t.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| v.get("access_token").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| v.get("OPENAI_API_KEY").and_then(Value::as_str).map(str::to_owned)),
        account: tokens
            .and_then(|t| t.get("account_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| v.get("account_id").and_then(Value::as_str).map(str::to_owned)),
        api_key: v
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| v.get("api_key").and_then(Value::as_str).map(str::to_owned)),
    })
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// --------------------------------------------------------------------------- body patching

fn zstd_decode(raw: &[u8]) -> Option<Vec<u8>> {
    crate::summary::zstd_decode(raw)
}

fn is_zstd(raw: &[u8]) -> bool {
    raw.len() > 4 && raw[0] == 0x28 && raw[1] == 0xb5 && raw[2] == 0x2f && raw[3] == 0xfd
}

/// Applies the configured environment rewrite, then `--inject` / `--field` / `--patch`.
///
/// The rewrite matters even for a pure probe: the recorded template carries the
/// `<environment_context>` block **as the client sent it**, so without this step the probe would go
/// out with the client's raw timezone/date instead of the zone this proxy is configured to present.
/// A forwarded request gets that rewrite; a probe must too, or the two are not comparable.
///
/// A body we cannot decode (no zstd, unexpected frame) is returned untouched with a warning: a
/// failed *experiment* must never silently become a *different* experiment.
fn build_body(
    template: &[u8],
    vary: &VarySpec,
    inject: &Identity,
    env: Option<&crate::envrewrite::EnvironmentSection>,
) -> (Vec<u8>, Vec<String>) {
    let mut notes = Vec::new();
    let encoded = is_zstd(template);
    let env_active = env.map(|e| e.is_active()).unwrap_or(false);
    if !vary.is_active() && !env_active {
        return (template.to_vec(), notes);
    }
    let Some(plain) = zstd_decode(template) else {
        notes.push("body is not decodable zstd; rewrites ignored (sent unchanged)".to_owned());
        return (template.to_vec(), notes);
    };
    let Ok(mut v) = serde_json::from_slice::<Value>(&plain) else {
        notes.push("decoded body is not JSON; rewrites ignored (sent unchanged)".to_owned());
        return (template.to_vec(), notes);
    };

    // (A) the same environment rewrite the forwarded path performs. `now = None` means "use the
    // clock", which is what makes `current_date = "auto"` land on today's date in that zone
    // instead of the stale date baked into the template message.
    if let Some(section) = env.filter(|e| e.is_active()) {
        let env_notes = crate::envrewrite::apply_env(section, &mut v, None);
        for line in env_notes.detail.iter() {
            notes.push(line.clone());
        }
        if !env_notes.changed {
            notes.push("environment_context: no block matched".to_owned());
        }
    }

    if !vary.is_active() {
        return encode_back(&v, encoded, notes);
    }

    // fresh identity, applied to the fields the client derives it from
    let sids = ["session_id", "thread_id", "window_id", "context_window_id", "root_turn_id", "turn_id"];
    if let Some(cm) = v.get_mut("client_metadata").and_then(Value::as_object_mut) {
        for (k, val) in cm.iter_mut() {
            let kl = k.to_ascii_lowercase();
            if kl == "x-codex-turn-metadata" {
                if let Some(inner) = val.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                    let mut inner = inner;
                    if let Some(o) = inner.as_object_mut() {
                        for f in ["session_id", "thread_id"] {
                            o.insert(f.to_owned(), json!(inject.session));
                        }
                        for f in ["turn_id", "root_turn_id"] {
                            o.insert(f.to_owned(), json!(inject.turn));
                        }
                        o.insert("window_id".to_owned(), json!(format!("{}:0", inject.session)));
                    }
                    *val = json!(serde_json::to_string(&inner).unwrap_or_default());
                    notes.push("client_metadata.x-codex-turn-metadata ids refreshed".to_owned());
                }
                continue;
            }
            if kl == "session_id" || kl == "thread_id" {
                *val = json!(inject.session);
            } else if kl == "turn_id" || kl == "root_turn_id" {
                *val = json!(inject.turn);
            } else if kl == "window_id" {
                *val = json!(format!("{}:0", inject.session));
            }
        }
    }
    if v.get("prompt_cache_key").is_some() {
        v["prompt_cache_key"] = json!(inject.session);
    }
    let _ = sids;

    let blank = |v: &mut Value, field: &str| -> usize {
        if let Some(obj) = v.get_mut(field).and_then(Value::as_object_mut) {
            for (_, val) in obj.iter_mut() {
                if !val.is_null() {
                    *val = Value::Null;
                }
            }
            return obj.len();
        }
        let mut n = 0;
        if let Some(root) = v.as_object_mut() {
            // one level down as well (e.g. {"reasoning": {"effort": …}})
            let keys: Vec<String> = root.keys().cloned().collect();
            for k in keys {
                if let Some(sub) = root.get_mut(&k).and_then(Value::as_object_mut) {
                    if let Some(slot) = sub.get_mut(field) {
                        *slot = Value::Null;
                        n += 1;
                    }
                }
            }
        }
        n
    };

    if let Some(field) = vary.field.as_deref() {
        let hit = if field == "*" {
            // every scalar at the top level
            let mut n = 0;
            let keys: Vec<String> = v
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            for k in keys {
                if k == "input" {
                    continue;
                }
                if v.get(&k).map(|x| !x.is_object() && !x.is_array()).unwrap_or(false) {
                    v[&k] = Value::Null;
                    n += 1;
                }
            }
            n
        } else {
            blank(&mut v, field)
        };
        if hit == 0 {
            notes.push(format!("--field {field:?} matched nothing (sent unchanged)"));
        } else {
            notes.push(format!("--field {field:?} blanked {hit} value(s)"));
        }
    }

    for (old, new) in &vary.patch {
        match old.as_str() {
            "sid" => {
                notes.push("patch sid: body carries no literal session id to swap".to_owned());
                let _ = new;
            }
            "pat" => {
                // replace the first `"model":"…"` in the serialized text
                let mut text = serde_json::to_string(&v).unwrap_or_default();
                if let Some(at) = text.find("\"model\":\"") {
                    let start = at + "\"model\":\"".len();
                    if let Some(end) = text[start..].find('"') {
                        let cur = text[start..start + end].to_owned();
                        text.replace_range(start..start + end, new);
                        if let Ok(back) = serde_json::from_str::<Value>(&text) {
                            v = back;
                        }
                        notes.push(format!("patch pat: model {cur:?} -> {new:?}"));
                    }
                }
            }
            other => {
                let mut text = serde_json::to_string(&v).unwrap_or_default();
                if text.contains(other) {
                    text = text.replacen(other, new, 1);
                    if let Ok(back) = serde_json::from_str::<Value>(&text) {
                        v = back;
                    }
                    notes.push(format!("patch {other:?} -> {new:?}"));
                } else {
                    notes.push(format!("patch {other:?} not found"));
                }
            }
        }
    }

    encode_back(&v, encoded, notes)
}

/// Re-compresses when the template was compressed, so the upstream still receives a zstd frame.
fn encode_back(v: &Value, encoded: bool, mut notes: Vec<String>) -> (Vec<u8>, Vec<String>) {
    let text = serde_json::to_vec(v).unwrap_or_default();
    if encoded {
        match crate::summary::zstd_encode(&text) {
            Some(z) => (z, notes),
            None => {
                notes.push(
                    "cannot re-compress (no zstd CLI) -> sending the decode, uncompressed".to_owned(),
                );
                (text, notes)
            }
        }
    } else {
        (text, notes)
    }
}

// --------------------------------------------------------------------------- egress binding

/// Resolves `--interface` / `--source-ip` into a concrete local address.
fn resolve_source(args: &Args) -> Result<Option<IpAddr>, String> {
    if let Some(ip) = args.source_ip.as_deref() {
        return ip
            .parse::<IpAddr>()
            .map(Some)
            .map_err(|e| format!("--source-ip {ip:?}: {e}"));
    }
    let Some(iface) = args.interface.as_deref() else { return Ok(None) };
    let addrs = iface_addresses(iface)?;
    let want_v6 = match args.prefer_family.as_deref() {
        Some("v6") | Some("ipv6") | Some("6") => Some(true),
        Some("v4") | Some("ipv4") | Some("4") => Some(false),
        _ => None,
    };
    let pick = match want_v6 {
        Some(true) => addrs.iter().find(|a| a.is_ipv6()),
        Some(false) => addrs.iter().find(|a| a.is_ipv4()),
        None => addrs
            .iter()
            .find(|a| a.is_ipv6() && !is_link_local(a))
            .or_else(|| addrs.iter().find(|a| a.is_ipv4())),
    };
    pick.copied()
        .map(Some)
        .ok_or_else(|| format!("interface {iface} has no usable address (found {addrs:?})"))
}

fn is_link_local(a: &IpAddr) -> bool {
    match a {
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        IpAddr::V4(v4) => v4.is_link_local(),
    }
}

/// Addresses of an interface, via `ip -o addr show dev <iface>` (present on the landing box and any
/// iproute2 host). Parsing this instead of `getifaddrs` keeps the dependency list unchanged.
fn iface_addresses(iface: &str) -> Result<Vec<IpAddr>, String> {
    let out = std::process::Command::new("ip")
        .args(["-o", "addr", "show", "dev", iface])
        .output()
        .map_err(|e| format!("cannot run `ip` to resolve --interface {iface}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`ip addr show dev {iface}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut addrs = Vec::new();
    for line in text.lines() {
        for tok in line.split_whitespace() {
            if let Some(rest) = tok.strip_prefix("inet6") {
                let _ = rest;
                continue;
            }
        }
        // "… inet 172.30.104.253/12 …" / "… inet6 2001:db8::1/64 …"
        let parts: Vec<&str> = line.split_whitespace().collect();
        for (i, p) in parts.iter().enumerate() {
            if (*p == "inet" || *p == "inet6") && i + 1 < parts.len() {
                let cidr = parts[i + 1];
                if let Some((ip, _)) = cidr.split_once('/') {
                    if let Ok(a) = ip.parse::<IpAddr>() {
                        addrs.push(a);
                    }
                }
            }
        }
    }
    Ok(addrs)
}

// --------------------------------------------------------------------------- run

pub async fn run(argv: &[String]) -> Result<(), String> {
    let args = parse_args(argv)?;
    let template = fs::read(&args.body)
        .map_err(|e| format!("cannot read --body-file {}: {e}", args.body.display()))?;

    // The environment rewrite the forwarded path applies. Without it a probe would carry the
    // client's raw `<timezone>`/`<current_date>` instead of the zone this proxy presents.
    let env_section = match args.config.as_ref() {
        Some(path) => match crate::config::load_environment_section(path) {
            Ok(section) => Some(section),
            Err(e) => {
                if !args.quiet {
                    eprintln!("warning: --config {}: {e}", path.display());
                }
                None
            }
        },
        None => crate::config::default_environment_section(),
    };

    let mut headers = headers_from_template(&args.body, args.vary.turn_state.as_deref());
    if let Some(tok) = args.vary.turn_state.as_ref() {
        headers.retain(|(k, _)| k != "x-codex-turn-state");
        headers.push(("x-codex-turn-state".to_owned(), tok.clone()));
    }
    let mut auth_source = args.auth.clone();
    if let Some(acc) = auth_source.clone() {
        let a = read_auth(&acc)?;
        if let Some(tok) = a.access.as_ref() {
            headers.retain(|(k, _)| k != "authorization");
            headers.push(("authorization".to_owned(), format!("Bearer {tok}")));
        } else if let Some(key) = a.api_key.as_ref() {
            headers.retain(|(k, _)| k != "authorization");
            headers.push(("authorization".to_owned(), format!("Bearer {key}")));
        }
        if let Some(id) = a.account.as_ref() {
            headers.retain(|(k, _)| k != "chatgpt-account-id");
            headers.push(("chatgpt-account-id".to_owned(), id.clone()));
        }
        auth_source = Some(acc.clone());
    } else {        // No --auth: make absolutely sure we are not accidentally sending the recorder's real
        // credentials from a recording file. An experiment must be explicit about its identity.
        let had_auth = headers.iter().any(|(k, _)| k == "authorization")
            || headers.iter().any(|(k, _)| k == "chatgpt-account-id");
        if had_auth {
            headers.retain(|(k, _)| k != "authorization" && k != "chatgpt-account-id");
            headers.push(("authorization".to_owned(), format!("Bearer {FAKE_TOKEN}")));
            headers.push(("chatgpt-account-id".to_owned(), FAKE_ACCOUNT.to_owned()));
        }
    }
    for (k, v) in &args.headers {
        headers.retain(|(n, _)| n != k);
        headers.push((k.clone(), v.clone()));
    }

    let source = resolve_source(&args)?;
    let mut builder = crate::tsgrab_http_builder(args.timeout, args.insecure, source, args.proxy.as_deref());
    builder = builder.pool_max_idle_per_host(1).http1_only();
    let client = builder.build().map_err(|e| format!("cannot build the HTTP client: {e}"))?;

    if !args.quiet {
        println!("tsgrab: {} attempt(s) -> {}", args.attempts, args.url);
        println!("  template body : {} ({} bytes{})", args.body.display(), template.len(),
                 if is_zstd(&template) { ", zstd" } else { "" });
        match sibling_header(&args.body) {
            Some(p) => println!("  template hdrs : {}", p.display()),
            None => println!("  template hdrs : (none; only --header values are sent)"),
        }
        println!(
            "  want length(s): {}",
            if args.want.is_empty() { "(any)".to_owned() } else { args.want.join(", ") }
        );
        println!(
            "  egress        : source={} proxy={}",
            source.map(|s| s.to_string()).unwrap_or_else(|| "(host default)".to_owned()),
            args.proxy.clone().unwrap_or_else(|| "(none)".to_owned())
        );
        println!(
            "  credentials   : {}",
            match auth_source.as_ref() {
                Some(p) => format!("from {} (account={:?})", p.display(),
                                   headers.iter().find(|(k, _)| k == "chatgpt-account-id").map(|(_, v)| v.clone())),
                None => "NOT used -- sending a placeholder bearer".to_owned(),
            }
        );
        if args.vary.is_active() {
            println!("  vary          : turn-state={} field={:?} patch={:?}",
                     args.vary.turn_state.is_some(), args.vary.field, args.vary.patch);
        }
        match env_section.as_ref().filter(|e| e.is_active()) {
            Some(e) => println!(
                "  env rewrite   : timezone={:?} current_date={:?} (same as the forwarded path)",
                e.timezone, e.current_date
            ),
            None => println!("  env rewrite   : none (the template's own block goes out as-is)"),
        }
        println!();
    }

    if args.dump_template {
        if let Some(plain) = zstd_decode(&template) {
            println!("--- decoded template body ---");
            println!("{}", String::from_utf8_lossy(&plain));
            println!("--- end ---");
        } else {
            println!("(template is not decodable zstd)");
        }
    }

    // ---- history first -------------------------------------------------------
    // Probing can come up empty for hours while the backend is in its other mode, but the recorder
    // has been logging every response header the whole time. Look there before spending a request.
    let installed_now = args
        .config
        .as_ref()
        .and_then(|_| crate::config::environment_token_file())
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_owned());

    let scan_args = crate::tsscan::ScanArgs {
        want: args.want.clone(),
        dirs: args.scan_dirs.clone(),
        max_depth: 4,
        fresh_within: args.scan_fresh_within,
        skip: if args.scan_anyway { None } else { installed_now.clone() },
        max_candidates: args.scan_max,
    };
    let found = crate::tsscan::scan(&scan_args);

    if !args.quiet {
        println!(
            "  history scan  : {} file(s) read, {} matching length, {} younger than {}s",
            found.considered, found.used_count, found.fresh_count, args.scan_fresh_within
        );
        if let Some(h) = found.newest.as_ref() {
            println!(
                "      newest overall: len={} age={}s from {}",
                h.len,
                h.age_secs(now_unix()),
                h.source.display()
            );
        }
        if let Some(h) = found.best.as_ref() {
            println!(
                "      USABLE       : len={} age={}s from {}",
                h.len,
                h.age_secs(now_unix()),
                h.source.display()
            );
        }
        let mut lens: Vec<_> = found.by_len.iter().collect();
        lens.sort_by_key(|(k, _)| **k);
        let dist = lens
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("      by length    : {dist}");
    }

    if args.scan_only {
        // `--quiet` must print the token and nothing else, so the shell can capture it.
        //
        // Only a *usable* hit counts, and in quiet mode a miss must print NOTHING. Reporting the
        // newest-but-too-old token here would look identical to success to a caller doing
        // `tok=$(... --scan-only --quiet)`, which is how a rotation loop ends up re-installing an
        // already-expired token forever.
        if let Some(h) = found.best.as_ref() {
            if args.quiet {
                println!("{}", h.token);
            } else {
                println!();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&h.to_json(now_unix())).unwrap_or_default()
                );
            }
            return Ok(());
        }
        if !args.quiet {
            println!();
            match found.newest.as_ref() {
                Some(h) => println!(
                    "nothing usable: the newest token is {}s old (limit {}s), from {}",
                    h.age_secs(now_unix()),
                    args.scan_fresh_within,
                    h.source.display()
                ),
                None => println!("no turn-state found in the scanned history"),
            }
        }
        return Err("nothing usable in the scan history".to_owned());
    }

    if let Some(hit) = found.best.clone() {
        let rec = hit.to_json(now_unix());
        if !args.quiet {
            println!();
            println!(
                "REUSED len={} age={}s from history (no request needed)",
                hit.len,
                hit.age_secs(now_unix())
            );
        }
        if let Some(out) = args.out.as_ref() {
            if let Some(parent) = out.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(out, serde_json::to_string_pretty(&rec).unwrap_or_default());
            if !args.quiet {
                println!("saved -> {}", out.display());
            }
        }
        if args.quiet {
            println!("{}", hit.token);
        } else {
            println!("token: {}", hit.token);
        }
        return Ok(());
    }

    let mut accepted: Option<Value> = None;
    let mut last: Option<Value> = None;

    for n in 1..=args.attempts {
        let id = identity();
        let (body, notes) = build_body(&template, &args.vary, &id, env_section.as_ref());
        for note in &notes {
            if !args.quiet {
                println!("      note: {note}");
            }
        }

        let mut req = client
            .post(&args.url)
            .header("content-type", "application/json")
            .body(body.clone());
        let enc = headers
            .iter()
            .find(|(k, _)| k == "content-encoding")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "zstd".to_owned());
        if is_zstd(&body) {
            req = req.header("content-encoding", enc);
        } else {
            req = req.header("content-encoding", "identity");
        }
        for (k, v) in &headers {
            if k == "content-encoding" || k == "content-type" {
                continue;
            }
            req = req.header(k.as_str(), v.as_str());
        }

        let t0 = std::time::Instant::now();
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                println!("[{n:02}] ERROR {}", one_line(&e.to_string()));
                sleep(args.gap_ms).await;
                continue;
            }
        };
        let status = resp.status().as_u16();
        let token = resp.headers().get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let sb = resp.headers().get("x-codex-safety-buffering-enabled")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let faster = resp.headers().get("x-codex-safety-buffering-faster-model")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let etag = resp.headers().get("x-models-etag")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let plan = resp.headers().get("x-codex-plan-type")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let used = resp.headers().get("x-codex-primary-used-percent")
            .and_then(|v| v.to_str().ok()).map(str::to_owned);
        let origin = {
            let lines: Vec<String> = resp.headers().iter()
                .map(|(k, v)| format!("{}: {}", k.as_str(), v.to_str().unwrap_or("")))
                .collect();
            lines.iter().find_map(|l| crate::decode_oailb(std::slice::from_ref(l)))
                .map(|h| h.replace("chat.gateway.", "").replace(".api.openai.com", ""))
        };
        let ms = t0.elapsed().as_millis();
        // *** the abort ***
        // `resp` still owns the unread body stream. Dropping it instead of polling it makes reqwest
        // discard the connection, so the upstream transfer stops here -- before any generation
        // could complete. That is the whole point of the probe.
        drop(resp);

        let len = token.as_ref().map(|t| token_len(t)).unwrap_or(0);
        let hit = match args.want.is_empty() {
            true => token.is_some(),
            false => args.want.iter().any(|w| w == &len.to_string()),
        };

        if !args.quiet {
            println!(
                "[{n:02}] HTTP {status} {ms:>5}ms  turn-state={len:>3}  origin={:<14} plan={:<7} sb={:<5} used%={:<3} etag={}",
                origin.clone().unwrap_or_else(|| "-".to_owned()),
                plan.clone().unwrap_or_else(|| "-".to_owned()),
                sb.clone().unwrap_or_else(|| "-".to_owned()),
                used.clone().unwrap_or_else(|| "-".to_owned()),
                etag.as_deref().map(short_etag).unwrap_or_else(|| "-".to_owned()),
            );
            if let Some(t) = token.as_ref() {
                println!("      token id={} issued={}", token_id(t),
                         token_issued(t).map(format_age).unwrap_or_else(|| "?".to_owned()));
            }
            if let Some(f) = faster.as_ref() {
                println!("      safety-buffering faster-model={f}");
            }
        }

        let rec = json!({
            "token": token,
            "len": len,
            "id": token.as_ref().map(|t| token_id(t)),
            "issued_at": token.as_ref().and_then(|t| token_issued(t)),
            "grabbed_at": now_unix(),
            "http_status": status,
            "origin": origin,
            "plan_type": plan,
            "safety_buffering": sb,
            "safety_buffering_faster_model": faster,
            "models_etag": etag,
            "used_percent": used,
            "template": args.body.display().to_string(),
            "attempt": n,
            "session": id.session,
            "turn": id.turn,
            "source_ip": source.map(|s| s.to_string()),
            "proxy": args.proxy.clone(),
            "auth_from": auth_source.as_ref().map(|p| p.display().to_string()),
        });
        last = Some(rec.clone());
        if hit {
            accepted = Some(rec);
            break;
        }
        sleep(args.gap_ms).await;
    }

    let Some(rec) = accepted.or(last.filter(|r| r["token"].is_null() && args.quiet)) else {
        if !args.quiet {
            println!();
            println!("no token matched {} in {} attempt(s)",
                     if args.want.is_empty() { "(any)".to_owned() } else { args.want.join("/") },
                     args.attempts);
        }
        return Err("no matching turn-state".to_owned());
    };

    if !rec["token"].is_null() {
        if let Some(out) = args.out.as_ref() {
            if let Some(parent) = out.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(out, serde_json::to_string_pretty(&rec).unwrap_or_default())
                .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
            if !args.quiet {
                println!("saved -> {}", out.display());
            }
        }
    }

    if args.quiet {
        // exactly the token, nothing else, so it composes with $(...)
        if let Some(t) = rec["token"].as_str() {
            println!("{t}");
        }
        return Ok(());
    }

    println!();
    match rec["token"].as_str() {
        Some(t) => {
            println!(
                "GOT len={} id={} issued={} origin={}",
                rec["len"],
                rec["id"].as_str().unwrap_or("-"),
                rec["issued_at"].as_u64().map(format_age).unwrap_or_else(|| "?".to_owned()),
                rec["origin"].as_str().unwrap_or("-"),
            );
            println!("token: {t}");
            Ok(())
        }
        None => Err("no token in the response".to_owned()),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_etag(etag: &str) -> String {
    etag.trim_matches(|c| c == 'W' || c == '/' || c == '"').chars().take(12).collect()
}

fn format_age(issued: u64) -> String {
    let now = now_unix();
    if issued == 0 || issued > now + 60 {
        return format!("unix={issued}");
    }
    format!("unix={issued} (age {}s)", now.saturating_sub(issued))
}

async fn sleep(ms: u64) {
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

