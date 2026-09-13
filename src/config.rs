//! Configuration: a TOML file plus the existing CLI flags.
//!
//! Persona fields have three states, distinguished by whether the key is *present*:
//!   * the key is absent            -> inherit the client's value
//!   * the key is `"inherit"`       -> same as absent (kept as a readable alias)
//!   * the key has any other value  -> use that value
//!   * `""` is only meaningful for `terminal` (it removes the terminal segment); an empty
//!     `originator` / `codex_version` / `os` / `arch` / `user_agent` is a config error.
//!
//! `[record]` decides what is written at all (every artifact has its own switch, and
//! `enabled = false` turns the recorder into a pure forwarder), `[limits]` bounds memory, and
//! header drop lists accept glob patterns (`cf-*`) matched with the `globset` crate.
//!
//! Precedence: built-in defaults < TOML file < CLI flags.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// A persona field: inherit from the client, or use a fixed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// Explicitly `"inherit"`: keep the client's value (same as leaving the key out).
    Inherit,
    /// A concrete replacement. An empty string is only valid for `terminal`.
    Set(String),
}

impl Field {
    pub fn value(&self) -> Option<&str> {
        match self {
            Field::Set(v) => Some(v.as_str()),
            Field::Inherit => None,
        }
    }
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(if raw.eq_ignore_ascii_case("inherit") {
            Field::Inherit
        } else {
            Field::Set(raw)
        })
    }
}

/// Device identity that the upstream should see. `None` means "inherit the client's value".
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Persona {
    /// `originator` header and the product name inside the User-Agent.
    #[serde(default)]
    pub originator: Option<Field>,
    /// Codex version: both User-Agent version segments (and `?client_version=` if enabled).
    #[serde(default)]
    pub codex_version: Option<Field>,
    /// `<os>; <arch>` inside the User-Agent.
    #[serde(default)]
    pub os: Option<Field>,
    /// Architecture inside the User-Agent.
    #[serde(default)]
    pub arch: Option<Field>,
    /// Terminal segment of the User-Agent. `""` removes it; absent keeps the client's.
    #[serde(default)]
    pub terminal: Option<Field>,
    /// Escape hatch: a complete literal User-Agent (wins over every field above).
    #[serde(default)]
    pub user_agent: Option<Field>,
    /// Also rewrite `?client_version=` (needs `codex_version` to be set).
    #[serde(default)]
    pub rewrite_client_version: bool,
}

impl Persona {
    /// True when at least one field carries a value (otherwise nothing is rewritten at all).
    pub fn is_active(&self) -> bool {
        [
            &self.originator,
            &self.codex_version,
            &self.os,
            &self.arch,
            &self.terminal,
            &self.user_agent,
        ]
        .iter()
        .any(|f| matches!(f, Some(Field::Set(_))))
    }

    /// Rejects empty values where they would produce an invalid User-Agent.
    fn validate(&self) -> Result<(), String> {
        for (name, field) in [
            ("originator", &self.originator),
            ("codex_version", &self.codex_version),
            ("os", &self.os),
            ("arch", &self.arch),
            ("user_agent", &self.user_agent),
        ] {
            if field.as_ref().and_then(Field::value) == Some("") {
                return Err(format!(
                    "persona.{name} must not be empty: omit the key (or write \"inherit\") to keep \
                     the client's value, write a value to replace it — only persona.terminal may be \"\""
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRules {
    /// Glob patterns (e.g. `cf-*`); matched case-insensitively against header names.
    #[serde(default)]
    pub drop: Vec<String>,
    /// Literal values; a value of the form `env:NAME` is read from the environment instead.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// `header = "ENV_VAR"` pairs: read the value from the environment (keeps secrets out of the file).
    #[serde(default)]
    pub set_from_env: BTreeMap<String, String>,
}

impl HeaderRules {
    fn compile(&self) -> Result<Option<Arc<GlobSet>>, String> {
        if self.drop.is_empty() {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.drop {
            let glob = Glob::new(pattern).map_err(|e| format!("bad glob {pattern:?}: {e}"))?;
            builder.add(glob);
        }
        builder
            .build()
            .map(|set| Some(Arc::new(set)))
            .map_err(|e| format!("cannot compile header globs: {e}"))
    }

    /// Resolves `set` + `set_from_env` into concrete `(name, value)` pairs.
    fn resolved(&self, section: &str, warnings: &mut Vec<String>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for (name, raw) in self.set.iter() {
            let name = name.to_ascii_lowercase();
            if let Some(var) = raw.strip_prefix("env:") {
                match env::var(var) {
                    Ok(value) => out.push((name, value)),
                    Err(_) => warnings.push(format!(
                        "[headers.{section}] {name} = \"env:{var}\" skipped: {var} is not set"
                    )),
                }
            } else {
                out.push((name, raw.clone()));
            }
        }
        for (name, var) in self.set_from_env.iter() {
            let name = name.to_ascii_lowercase();
            match env::var(var) {
                Ok(value) => out.push((name, value)),
                Err(_) => warnings.push(format!(
                    "[headers.{section}] {name} from env {var} skipped: {var} is not set"
                )),
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderSection {
    #[serde(default)]
    pub request: HeaderRules,
    #[serde(default)]
    pub response: HeaderRules,
}

/// Path mapping between what clients call and what the codex backend expects.
///
/// `upstream_prefix` defaults to `/backend-api/codex` **on purpose**: codex only treats a provider
/// as the codex backend when its `base_url` ends with that path
/// (`model-provider-info/src/lib.rs`, `supports_codex_backend_routes`); with any other path it stops
/// sending codex-backend-only headers such as `x-codex-routing-hint` and the request shape changes.
/// `/v1` and `/` are accepted as *incoming* aliases for other OpenAI-style clients — they are not
/// the default, and codex itself should keep using `/backend-api/codex`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesSection {
    /// Prefix prepended to the normalized path before the request goes upstream.
    #[serde(default = "default_upstream_prefix")]
    pub upstream_prefix: String,
    /// Incoming prefixes that are stripped before `upstream_prefix` is added (longest match wins).
    #[serde(default = "default_strip_prefixes")]
    pub strip_prefixes: Vec<String>,
}

fn default_upstream_prefix() -> String {
    "/backend-api/codex".to_owned()
}

fn default_strip_prefixes() -> Vec<String> {
    vec![
        "/backend-api/codex".to_owned(),
        "/v1".to_owned(),
        "/api/v1".to_owned(),
    ]
}

impl Default for RoutesSection {
    fn default() -> Self {
        Self {
            upstream_prefix: default_upstream_prefix(),
            strip_prefixes: default_strip_prefixes(),
        }
    }
}

impl RoutesSection {
    /// Maps an incoming request path to the path sent upstream:
    /// strip one known incoming prefix (longest first), then prepend `upstream_prefix`.
    pub fn upstream_path(&self, path: &str) -> String {
        let mut rest = path;
        let mut prefixes: Vec<&String> = self.strip_prefixes.iter().collect();
        prefixes.sort_by_key(|p| std::cmp::Reverse(p.trim_end_matches('/').len()));
        for prefix in prefixes {
            let prefix = prefix.trim_end_matches('/');
            if prefix.is_empty() {
                continue;
            }
            if rest == prefix {
                rest = "/";
                break;
            }
            if let Some(tail) = rest.strip_prefix(prefix) {
                if tail.starts_with('/') {
                    rest = tail;
                    break;
                }
            }
        }
        let prefix = self.upstream_prefix.trim_end_matches('/');
        if prefix.is_empty() {
            rest.to_owned()
        } else {
            format!("{prefix}{rest}")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionOrder {
    /// native-tls / OpenSSL: fixed extension order, exactly like the real codex client.
    Fixed,
    /// rustls backend: extension order is shuffled per connection (rustls behaviour).
    Randomize,
}

impl Default for ExtensionOrder {
    fn default() -> Self {
        ExtensionOrder::Fixed
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    /// `fixed` (default) or `randomize`.
    #[serde(default)]
    pub extension_order: Option<String>,
}

/// What to write, and for how long.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordSection {
    /// Master switch. `false` = pure forwarder: nothing is written at all (rewrites still apply).
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub index: bool,
    #[serde(default = "yes")]
    pub request_headers: bool,
    #[serde(default = "yes")]
    pub request_headers_out: bool,
    #[serde(default = "yes")]
    pub request_body_raw: bool,
    #[serde(default = "yes")]
    pub request_body_json: bool,
    /// Store the rewritten request body when `--drop-body-key` / `--set-body-key` changed it.
    #[serde(default = "yes")]
    pub request_body_out: bool,
    #[serde(default = "yes")]
    pub request_summary: bool,
    #[serde(default = "yes")]
    pub response_headers: bool,
    #[serde(default = "yes")]
    pub response_stream: bool,
    /// Keep the full `/models` catalog in the stream file (off: only its etag + hash are recorded).
    #[serde(default = "no")]
    pub response_stream_catalog: bool,
    #[serde(default = "yes")]
    pub response_summary: bool,
    /// Delete recorded files older than this many days.
    #[serde(default)]
    pub retention_days: Option<u64>,
    /// Gzip recorded files older than this many days.
    #[serde(default)]
    pub gzip_after_days: Option<u64>,
    /// Delete the oldest recorded files until the tree fits into this many bytes.
    #[serde(default)]
    pub max_total_bytes: Option<u64>,
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

impl Default for RecordSection {
    fn default() -> Self {
        Self {
            enabled: true,
            index: true,
            request_headers: true,
            request_headers_out: true,
            request_body_raw: true,
            request_body_json: true,
            request_body_out: true,
            request_summary: true,
            response_headers: true,
            response_stream: true,
            response_stream_catalog: false,
            response_summary: true,
            retention_days: None,
            gzip_after_days: None,
            max_total_bytes: None,
        }
    }
}

/// Memory bounds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSection {
    /// In-memory cap for a request body; above it `request_body_over_limit` decides what happens.
    #[serde(default = "default_request_body_bytes")]
    pub request_body_bytes: usize,
    /// `spill` (default) writes the body to a file and forwards from there, `reject` answers 413,
    /// `stream` forwards without buffering (recording of the body is then limited to a prefix).
    #[serde(default = "default_over_limit")]
    pub request_body_over_limit: String,
    /// In-memory copy kept for the response summary when the body is not SSE (0 = keep nothing).
    #[serde(default = "default_summary_bytes")]
    pub summary_buffer_bytes: usize,
    /// Write buffer for recorded files (0 = write every chunk straight through).
    #[serde(default = "default_write_bytes")]
    pub write_buffer_bytes: usize,
}

fn default_request_body_bytes() -> usize {
    4 * 1024 * 1024
}

fn default_over_limit() -> String {
    "spill".to_owned()
}

fn default_summary_bytes() -> usize {
    4 * 1024 * 1024
}

fn default_write_bytes() -> usize {
    256 * 1024
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            request_body_bytes: default_request_body_bytes(),
            request_body_over_limit: default_over_limit(),
            summary_buffer_bytes: default_summary_bytes(),
            write_buffer_bytes: default_write_bytes(),
        }
    }
}

impl LimitsSection {
    fn validate(&self) -> Result<(), String> {
        match self.request_body_over_limit.trim() {
            "spill" | "reject" | "stream" => Ok(()),
            other => Err(format!(
                "limits.request_body_over_limit must be \"spill\", \"reject\" or \"stream\", got {other:?}"
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    upstream: Option<String>,
    #[serde(default)]
    log_dir: Option<PathBuf>,
    /// Present at all -> the persona is active (fields left out inherit the client's value).
    #[serde(default)]
    persona: Option<Persona>,
    #[serde(default)]
    headers: HeaderSection,
    #[serde(default)]
    routes: RoutesSection,
    #[serde(default)]
    tls: TlsSection,
    #[serde(default)]
    record: RecordSection,
    #[serde(default)]
    limits: LimitsSection,
    /// `true` -> also write one JSONL file per session under `session_dir` (off by default).
    #[serde(default)]
    session_capture: bool,
    #[serde(default)]
    session_dir: Option<PathBuf>,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub upstream: String,
    pub log_dir: PathBuf,
    pub persona: Option<Persona>,
    #[allow(dead_code)]
    pub request_rules: HeaderRules,
    #[allow(dead_code)]
    pub response_rules: HeaderRules,
    /// `set` + `set_from_env`, already resolved (missing env vars produce warnings).
    pub request_sets: Vec<(String, String)>,
    pub response_sets: Vec<(String, String)>,
    pub request_drop_globs: Option<Arc<GlobSet>>,
    pub response_drop_globs: Option<Arc<GlobSet>>,
    pub routes: RoutesSection,
    pub extension_order: ExtensionOrder,
    pub record: RecordSection,
    pub limits: LimitsSection,
    pub body_drop: Vec<String>,
    pub body_set: Vec<(String, String)>,
    pub rewrite_catalog: bool,
    pub session_capture: bool,
    pub session_dir: PathBuf,
    pub config_path: Option<PathBuf>,
}

fn cli_args() -> Vec<String> {
    env::args().skip(1).collect()
}

fn cli_pairs(args: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut flags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                values.insert(key.to_owned(), args[i + 1].clone());
                i += 2;
            } else {
                flags.push(key.to_owned());
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    (values, flags)
}

fn config_path(values: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(p) = values.get("config") {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = env::var("CODEX_REC_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    for candidate in ["codex-rec.toml", "/etc/codex-rec.toml"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Loads the TOML file (if any) and applies CLI overrides. Returns the config and warnings.
pub fn load() -> Result<(Config, Vec<String>), String> {
    let args = cli_args();
    let (values, flags) = cli_pairs(&args);
    let mut warnings = Vec::new();

    let path = config_path(&values);
    let file: FileConfig = match path.as_ref() {
        Some(p) => {
            let text =
                fs::read_to_string(p).map_err(|e| format!("cannot read config {}: {e}", p.display()))?;
            toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", p.display()))?
        }
        None => FileConfig::default(),
    };

    if let Some(persona) = file.persona.as_ref() {
        persona.validate()?;
        if !persona.is_active() {
            warnings.push(
                "persona has no values: every field inherits the client's identity, so nothing is \
                 rewritten"
                    .to_owned(),
            );
        }
    }
    file.limits.validate()?;

    let list = |key: &str| -> Vec<String> {
        values
            .get(key)
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let pairs = |key: &str| -> Vec<(String, String)> {
        values
            .get(key)
            .map(|v| {
                v.split(';')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(a, b)| (a.trim().to_ascii_lowercase(), b.trim().to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    };

    let extension_order = match file.tls.extension_order.as_deref().map(str::trim) {
        None | Some("") | Some("fixed") => ExtensionOrder::Fixed,
        Some("randomize") | Some("randomised") | Some("randomized") => ExtensionOrder::Randomize,
        Some(other) => {
            return Err(format!(
                "tls.extension_order must be \"fixed\" or \"randomize\", got {other:?}"
            ))
        }
    };

    // CLI header flags extend the file rules rather than replacing them.
    let mut request_rules = file.headers.request.clone();
    request_rules.drop.extend(list("drop-req-header"));
    for (k, v) in pairs("set-req-header") {
        request_rules.set.insert(k, v);
    }
    let mut response_rules = file.headers.response.clone();
    response_rules.drop.extend(list("drop-res-header"));
    for (k, v) in pairs("set-res-header") {
        response_rules.set.insert(k, v);
    }
    let request_drop_globs = request_rules.compile()?;
    let response_drop_globs = response_rules.compile()?;
    let request_sets = request_rules.resolved("request", &mut warnings);
    let response_sets = response_rules.resolved("response", &mut warnings);

    let session_capture = file.session_capture || flags.iter().any(|f| f == "session-capture");
    let session_dir = values
        .get("session-dir")
        .map(PathBuf::from)
        .or(file.session_dir)
        .unwrap_or_else(|| PathBuf::from("sessions"));

    let cfg = Config {
        listen: values
            .get("listen")
            .cloned()
            .or(file.listen)
            .unwrap_or_else(|| "127.0.0.1:18080".to_owned()),
        upstream: values
            .get("upstream")
            .cloned()
            .or(file.upstream)
            .unwrap_or_else(|| "https://chatgpt.com".to_owned()),
        log_dir: values
            .get("log-dir")
            .map(PathBuf::from)
            .or(file.log_dir)
            .unwrap_or_else(|| PathBuf::from("/root/rec")),
        persona: file.persona.clone(),
        request_rules,
        response_rules,
        request_sets,
        response_sets,
        request_drop_globs,
        response_drop_globs,
        routes: file.routes.clone(),
        extension_order,
        record: file.record.clone(),
        limits: file.limits.clone(),
        body_drop: values
            .get("drop-body-key")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        body_set: values
            .get("set-body-key")
            .map(|v| {
                v.split(';')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(a, b)| (a.trim().to_owned(), b.trim().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        rewrite_catalog: flags.iter().any(|f| f == "rewrite-catalog"),
        session_capture,
        session_dir,
        config_path: path,
    };

    if cfg.persona.is_none() {
        warnings.push("no [persona] section: the client's identity passes through untouched".to_owned());
    }
    if cfg.extension_order == ExtensionOrder::Randomize && !cfg!(feature = "rustls-backend") {
        warnings.push(
            "tls.extension_order = \"randomize\" needs the rustls-backend cargo feature; \
             falling back to native-tls (fixed order)"
                .to_owned(),
        );
    }
    if !cfg.record.enabled {
        warnings.push(
            "[record] enabled = false: nothing is recorded (persona, header and path rewriting still apply)"
                .to_owned(),
        );
    }
    Ok((cfg, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_inherit_and_empty_are_distinct() {
        let persona: Persona = toml::from_str(
            r#"
            codex_version = "0.145.0"
            os = "inherit"
            terminal = ""
            "#,
        )
        .unwrap();
        assert_eq!(persona.codex_version.as_ref().and_then(Field::value), Some("0.145.0"));
        assert!(matches!(persona.os, Some(Field::Inherit)));
        assert_eq!(persona.terminal.as_ref().and_then(Field::value), Some(""));
        assert!(persona.arch.is_none());
        assert!(persona.is_active());
    }

    #[test]
    fn empty_values_are_rejected_where_they_make_no_sense() {
        for bad in [
            "os = \"\"",
            "originator = \"\"",
            "arch = \"\"",
            "user_agent = \"\"",
            "codex_version = \"\"",
        ] {
            let persona: Persona = toml::from_str(bad).unwrap();
            assert!(persona.validate().is_err(), "{bad} should be rejected");
        }
        let ok: Persona = toml::from_str("terminal = \"\"").unwrap();
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn only_inherits_means_inactive() {
        let persona: Persona = toml::from_str("os = \"inherit\"\narch = \"inherit\"").unwrap();
        assert!(!persona.is_active());
        assert!(persona.validate().is_ok());
    }

    #[test]
    fn route_mapping_defaults_to_the_codex_path() {
        let routes = RoutesSection::default();
        assert_eq!(
            routes.upstream_path("/backend-api/codex/responses"),
            "/backend-api/codex/responses"
        );
        assert_eq!(routes.upstream_path("/v1/responses"), "/backend-api/codex/responses");
        assert_eq!(routes.upstream_path("/v1/models"), "/backend-api/codex/models");
        assert_eq!(routes.upstream_path("/api/v1/responses"), "/backend-api/codex/responses");
        assert_eq!(routes.upstream_path("/responses"), "/backend-api/codex/responses");
        assert_eq!(routes.upstream_path("/"), "/backend-api/codex/");
        assert_eq!(routes.upstream_path("/backend-api/codex"), "/backend-api/codex/");
    }

    #[test]
    fn route_mapping_can_be_reconfigured() {
        // An empty upstream prefix means "forward the stripped path as-is".
        let routes: RoutesSection = toml::from_str("upstream_prefix = \"\"").unwrap();
        assert_eq!(routes.upstream_path("/v1/responses"), "/responses");

        let routes: RoutesSection = toml::from_str(
            r#"
            upstream_prefix = "/backend-api/codex"
            strip_prefixes = ["/openai/v1", "/v1"]
            "#,
        )
        .unwrap();
        assert_eq!(routes.upstream_path("/openai/v1/models"), "/backend-api/codex/models");
        assert_eq!(routes.upstream_path("/v1/models"), "/backend-api/codex/models");
        assert_eq!(routes.upstream_path("/other/models"), "/backend-api/codex/other/models");
    }

    #[test]
    fn record_defaults_and_switch_parsing() {
        let record = RecordSection::default();
        assert!(record.enabled);
        assert!(record.request_body_json);
        assert!(!record.response_stream_catalog, "the catalog stream is off by default");
        assert_eq!(record.retention_days, None);

        let parsed: RecordSection = toml::from_str(
            "enabled = false\nresponse_stream = false\nretention_days = 7\nmax_total_bytes = 1000",
        )
        .unwrap();
        assert!(!parsed.enabled);
        assert!(!parsed.response_stream);
        assert_eq!(parsed.retention_days, Some(7));
        assert_eq!(parsed.max_total_bytes, Some(1000));

        let limits = LimitsSection::default();
        assert_eq!(limits.request_body_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.request_body_over_limit, "spill");
        assert!(limits.validate().is_ok());

        let bad: LimitsSection = toml::from_str("request_body_over_limit = \"explode\"").unwrap();
        assert!(bad.validate().is_err());

        let tuned: LimitsSection =
            toml::from_str("request_body_bytes = 0\nsummary_buffer_bytes = 0\nwrite_buffer_bytes = 0")
                .unwrap();
        assert_eq!(tuned.request_body_bytes, 0);
        assert_eq!(tuned.summary_buffer_bytes, 0);
    }

    #[test]
    fn header_sets_resolve_env_forms() {
        std::env::set_var("CODEX_REC_TEST_TOKEN", "secret-value");
        let rules: HeaderRules = toml::from_str(
            r#"
            set = { "x-fixed" = "plain", authorization = "env:CODEX_REC_TEST_TOKEN" }
            set_from_env = { "chatgpt-account-id" = "CODEX_REC_TEST_ACCOUNT" }
            "#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let resolved = rules.resolved("request", &mut warnings);
        assert!(resolved.iter().any(|(k, v)| k == "x-fixed" && v == "plain"));
        assert!(resolved
            .iter()
            .any(|(k, v)| k == "authorization" && v == "secret-value"));
        assert!(!resolved.iter().any(|(k, _)| k == "chatgpt-account-id"));
        assert_eq!(warnings.len(), 1);
        std::env::remove_var("CODEX_REC_TEST_TOKEN");
    }
}
