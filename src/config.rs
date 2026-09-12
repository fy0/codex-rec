//! Configuration: a TOML file plus the existing CLI flags.
//!
//! Three-state persona fields (v0.3):
//!   * omitted            -> the built-in default (see [`persona_defaults`])
//!   * `"inherit"`        -> keep whatever the client sent
//!   * any other value    -> use that value
//!
//! Header drop lists accept simple glob patterns (`cf-*`, `x-forwarded-*`), matched with the
//! `globset` crate, compiled once at startup.
//!
//! Precedence: built-in defaults < TOML file < CLI flags.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// A persona field: default / inherit from the client / fixed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// Not written in the config: use the built-in default for this field.
    Default,
    /// Explicitly `"inherit"`: keep the client's value.
    Inherit,
    /// A concrete replacement.
    Set(String),
}

impl Default for Field {
    fn default() -> Self {
        Field::Default
    }
}

impl Field {
    pub fn is_inherit(&self) -> bool {
        matches!(self, Field::Inherit)
    }

    pub fn value(&self) -> Option<&str> {
        match self {
            Field::Set(v) => Some(v.as_str()),
            _ => None,
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

/// Built-in defaults used when a `[persona]` field is omitted.
///
/// `codex_version` is intentionally "inherit": we never invent a version, because the version we
/// claim decides which model catalog the server returns (a mismatched claim costs you metadata).
#[derive(Debug, Clone, Copy)]
pub struct PersonaDefaults {
    pub originator: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub terminal: &'static str,
    pub codex_version_inherits: bool,
}

pub const PERSONA_DEFAULTS: PersonaDefaults = PersonaDefaults {
    originator: "codex-tui",
    os: "Linux",
    arch: "x86_64",
    terminal: "",
    codex_version_inherits: true,
};

/// Device identity that the upstream should see.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Persona {
    /// `originator` header and the product name inside the User-Agent. Default `codex-tui`.
    #[serde(default)]
    pub originator: Field,
    /// Codex version: both User-Agent version segments. Default: inherit from the client.
    #[serde(default)]
    pub codex_version: Field,
    /// `<os>; <arch>` inside the User-Agent. Default `Linux`.
    #[serde(default)]
    pub os: Field,
    /// Architecture inside the User-Agent. Default `x86_64`.
    #[serde(default)]
    pub arch: Field,
    /// Terminal segment of the User-Agent. Default: omitted; set `""` to force-remove it.
    #[serde(default)]
    pub terminal: Field,
    /// Escape hatch: a complete literal User-Agent (wins over every field above). Default: composed.
    #[serde(default)]
    pub user_agent: Field,
    /// Also rewrite `?client_version=`. Default false.
    #[serde(default)]
    pub rewrite_client_version: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRules {
    /// Glob patterns (e.g. `cf-*`); matched case-insensitively against header names.
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
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
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderSection {
    #[serde(default)]
    pub request: HeaderRules,
    #[serde(default)]
    pub response: HeaderRules,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    upstream: Option<String>,
    #[serde(default)]
    log_dir: Option<PathBuf>,
    /// Present at all -> the persona is active (omitted fields use the built-in defaults).
    #[serde(default)]
    persona: Option<Persona>,
    #[serde(default)]
    headers: HeaderSection,
    #[serde(default)]
    tls: TlsSection,
    /// `true` -> also write one JSONL file per session under `session_dir`.
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
    pub request_rules: HeaderRules,
    pub response_rules: HeaderRules,
    pub request_drop_globs: Option<Arc<GlobSet>>,
    pub response_drop_globs: Option<Arc<GlobSet>>,
    pub extension_order: ExtensionOrder,
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
        request_drop_globs,
        response_drop_globs,
        extension_order,
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
    Ok((cfg, warnings))
}
