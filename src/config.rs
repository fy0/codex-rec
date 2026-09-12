//! Configuration: a TOML file plus the existing CLI flags, with per-field
//! "override or inherit" semantics for the device persona.
//!
//! Precedence: built-in defaults < TOML file < CLI flags.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

/// A persona field: keep whatever the client sent, or replace it with a fixed value.
/// In TOML, the literal string `"inherit"` means "do not touch this field".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Inherit,
    Set(String),
}

impl Default for Field {
    fn default() -> Self {
        Field::Inherit
    }
}

impl Field {
    pub fn is_inherit(&self) -> bool {
        matches!(self, Field::Inherit)
    }

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

/// Device identity that the upstream should see. Every field may be overridden or inherited.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Persona {
    /// `originator` header and the product name inside the User-Agent.
    #[serde(default)]
    pub originator: Field,
    /// Codex version: the User-Agent version segments and `?client_version=`.
    #[serde(default)]
    pub codex_version: Field,
    /// The `<os>; <arch>` part of the User-Agent, e.g. `Debian 12.0.0`.
    #[serde(default)]
    pub os: Field,
    /// The architecture part of the User-Agent, e.g. `x86_64`.
    #[serde(default)]
    pub arch: Field,
    /// The terminal segment of the User-Agent, e.g. `tmux/3.3a`. An empty string removes it.
    #[serde(default)]
    pub terminal: Field,
    /// Escape hatch: a complete literal User-Agent (wins over every field above).
    #[serde(default)]
    pub user_agent: Field,
    /// Also rewrite `?client_version=` on the request path.
    #[serde(default)]
    pub rewrite_client_version: bool,
}

impl Persona {
    /// True when at least one field is an override (otherwise requests pass through untouched).
    pub fn is_active(&self) -> bool {
        !(self.originator.is_inherit()
            && self.codex_version.is_inherit()
            && self.os.is_inherit()
            && self.arch.is_inherit()
            && self.terminal.is_inherit()
            && self.user_agent.is_inherit())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRules {
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
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
    #[serde(default)]
    persona: Persona,
    #[serde(default)]
    headers: HeaderSection,
    #[serde(default)]
    tls: TlsSection,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub upstream: String,
    pub log_dir: PathBuf,
    pub persona: Persona,
    pub request_rules: HeaderRules,
    pub response_rules: HeaderRules,
    pub extension_order: ExtensionOrder,
    pub body_drop: Vec<String>,
    pub body_set: Vec<(String, String)>,
    pub rewrite_catalog: bool,
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
            let text = fs::read_to_string(p)
                .map_err(|e| format!("cannot read config {}: {e}", p.display()))?;
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
        config_path: path,
    };

    if !cfg.persona.is_active() {
        warnings.push("persona is inactive (every field inherits the client's value)".to_owned());
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
