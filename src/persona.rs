//! Device persona: rebuild the outgoing identity (User-Agent, `originator`, `?client_version=`)
//! from the incoming request.
//!
//! Field semantics (v0.3): omitted -> built-in default, `"inherit"` -> the client's value,
//! anything else -> that literal value. See [`crate::config::PERSONA_DEFAULTS`].
//!
//! Measured real-codex User-Agent shapes (codex-cli 0.153.4, Linux):
//!   full  : `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)`
//!   no tail: `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a`
//!   short : `codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)`

use crate::config::{Config, Field, Persona, PERSONA_DEFAULTS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UaParts {
    pub originator: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub terminal: Option<String>,
    /// Whether the trailing `(<originator>; <version>)` suffix was present.
    pub suffix: bool,
}

/// Parses the codex User-Agent. Returns `None` for anything that does not look like it.
pub fn parse_ua(ua: &str) -> Option<UaParts> {
    let (prod, rest) = ua.split_once('/')?;
    let (version, rest) = rest.split_once(" (")?;
    let (inside, tail) = rest.split_once(')')?;
    let (os, arch) = inside.split_once("; ")?;
    let tail = tail.trim();
    let (terminal, suffix) = if tail.is_empty() {
        (None, false)
    } else if let Some(idx) = tail.find(" (") {
        (Some(tail[..idx].trim().to_owned()), true)
    } else {
        (Some(tail.to_owned()), false)
    };
    Some(UaParts {
        originator: prod.to_owned(),
        version: version.to_owned(),
        os: os.to_owned(),
        arch: arch.to_owned(),
        terminal,
        suffix,
    })
}

/// Renders the canonical codex User-Agent form.
pub fn compose_ua(parts: &UaParts) -> String {
    let mut out = format!(
        "{}/{} ({}; {})",
        parts.originator, parts.version, parts.os, parts.arch
    );
    if let Some(terminal) = parts.terminal.as_deref() {
        if !terminal.is_empty() {
            out.push(' ');
            out.push_str(terminal);
        }
    }
    if parts.suffix {
        out.push_str(&format!(" ({}; {})", parts.originator, parts.version));
    }
    out
}

/// Resolves one persona field to a concrete value, or `None` when the client's value must be kept
/// but is unavailable (in which case the whole User-Agent is left untouched).
fn resolve_part(
    field: &Field,
    client: Option<&str>,
    default: &str,
    default_inherits: bool,
) -> Option<String> {
    match field {
        Field::Set(value) => Some(value.to_owned()),
        Field::Inherit => client.map(str::to_owned),
        Field::Default => {
            if default_inherits {
                client.map(str::to_owned)
            } else {
                Some(default.to_owned())
            }
        }
    }
}

/// Returns the rewritten User-Agent (or `None` when nothing should change).
pub fn rewrite_user_agent(persona: &Persona, incoming: &str) -> Option<String> {
    if let Field::Set(literal) = &persona.user_agent {
        return (literal != incoming).then(|| literal.clone());
    }
    if persona.user_agent.is_inherit() {
        return None;
    }

    let parsed = parse_ua(incoming);
    let defaults = PERSONA_DEFAULTS;
    let originator = resolve_part(
        &persona.originator,
        parsed.as_ref().map(|p| p.originator.as_str()),
        defaults.originator,
        false,
    )?;
    let version = resolve_part(
        &persona.codex_version,
        parsed.as_ref().map(|p| p.version.as_str()),
        defaults.originator,
        defaults.codex_version_inherits,
    )?;
    let os = resolve_part(
        &persona.os,
        parsed.as_ref().map(|p| p.os.as_str()),
        defaults.os,
        false,
    )?;
    let arch = resolve_part(
        &persona.arch,
        parsed.as_ref().map(|p| p.arch.as_str()),
        defaults.arch,
        false,
    )?;
    let terminal = resolve_part(
        &persona.terminal,
        parsed.as_ref().and_then(|p| p.terminal.as_deref()),
        defaults.terminal,
        false,
    )
    .filter(|t| !t.is_empty());

    let parts = UaParts {
        originator,
        version,
        os,
        arch,
        terminal,
        suffix: parsed.as_ref().map(|p| p.suffix).unwrap_or(false),
    };
    let composed = compose_ua(&parts);
    (composed != incoming).then_some(composed)
}

/// Rewrites `client_version=` inside a path+query string.
pub fn rewrite_client_version(query: &str, version: &str) -> Option<String> {
    let (path, query) = match query.split_once('?') {
        Some((p, q)) => (p, q),
        None => return None,
    };
    let mut changed = false;
    let rebuilt: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if k == "client_version" => {
                changed = true;
                format!("{k}={version}")
            }
            _ => pair.to_owned(),
        })
        .collect();
    if changed {
        Some(format!("{path}?{}", rebuilt.join("&")))
    } else {
        None
    }
}

fn resolved_originator(persona: &Persona) -> Option<String> {
    match &persona.originator {
        Field::Set(value) => Some(value.clone()),
        Field::Inherit => None,
        Field::Default => Some(PERSONA_DEFAULTS.originator.to_owned()),
    }
}

/// Applies the persona to the outgoing header set and returns what was changed (for the log).
pub fn apply(cfg: &Config, headers: &mut reqwest::header::HeaderMap, uri: &mut String) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(persona) = cfg.persona.as_ref() else {
        return notes;
    };

    let incoming_ua = headers
        .get(reqwest::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(incoming) = incoming_ua {
        if let Some(rewritten) = rewrite_user_agent(persona, &incoming) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&rewritten) {
                headers.insert(reqwest::header::USER_AGENT, value);
                notes.push(format!("user-agent: {incoming} -> {rewritten}"));
            }
        }
    }

    if let Some(originator) = resolved_originator(persona) {
        let key = reqwest::header::HeaderName::from_static("originator");
        if headers.contains_key(&key) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&originator) {
                headers.insert(key, value);
                notes.push(format!("originator -> {originator}"));
            }
        }
    }

    if persona.rewrite_client_version {
        if let Some(version) = persona.codex_version.value() {
            if let Some(rewritten) = rewrite_client_version(uri, version) {
                notes.push(format!("path: {uri} -> {rewritten}"));
                *uri = rewritten;
            }
        }
    }

    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn persona_from(value: serde_json::Value) -> Persona {
        serde_json::from_value(value).expect("persona")
    }

    const CLIENT_UA: &str =
        "codex-tui/0.153.4 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.153.4)";

    #[test]
    fn parses_all_measured_shapes() {
        let full = parse_ua(CLIENT_UA).unwrap();
        assert_eq!(full.originator, "codex-tui");
        assert_eq!(full.version, "0.153.4");
        assert_eq!(full.os, "Debian 12.0.0");
        assert_eq!(full.arch, "x86_64");
        assert_eq!(full.terminal.as_deref(), Some("xterm-256color"));
        assert!(full.suffix);
        assert_eq!(compose_ua(&full), CLIENT_UA);

        let short = parse_ua("codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)").unwrap();
        assert_eq!(short.terminal, None);
        assert!(!short.suffix);
        assert_eq!(compose_ua(&short), "codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)");
    }

    #[test]
    fn omitted_fields_take_the_built_in_defaults() {
        // Only the version is pinned; everything else falls back to the defaults
        // (originator codex-tui, Linux/x86_64, no terminal segment).
        let persona = persona_from(json!({ "codex_version": "0.145.0" }));
        let out = rewrite_user_agent(&persona, CLIENT_UA).unwrap();
        assert_eq!(out, "codex-tui/0.145.0 (Linux; x86_64) (codex-tui; 0.145.0)");
    }

    #[test]
    fn inherit_keeps_the_clients_parts() {
        let persona = persona_from(json!({
            "codex_version": "0.145.0",
            "os": "inherit",
            "arch": "inherit",
            "terminal": "inherit"
        }));
        let out = rewrite_user_agent(&persona, CLIENT_UA).unwrap();
        assert_eq!(
            out,
            "codex-tui/0.145.0 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.145.0)"
        );
    }

    #[test]
    fn empty_terminal_removes_the_segment() {
        let persona = persona_from(json!({ "terminal": "" }));
        let out = rewrite_user_agent(&persona, CLIENT_UA).unwrap();
        assert_eq!(out, "codex-tui/0.153.4 (Linux; x86_64) (codex-tui; 0.153.4)");
    }

    #[test]
    fn literal_user_agent_wins() {
        let persona = persona_from(json!({ "user_agent": "curl/7.88.1", "originator": "x" }));
        assert_eq!(rewrite_user_agent(&persona, CLIENT_UA).as_deref(), Some("curl/7.88.1"));
    }

    #[test]
    fn client_version_query_rewrite() {
        assert_eq!(
            rewrite_client_version("/backend-api/codex/models?client_version=0.153.4&x=1", "0.145.0")
                .as_deref(),
            Some("/backend-api/codex/models?client_version=0.145.0&x=1")
        );
        assert_eq!(rewrite_client_version("/models", "0.145.0"), None);
    }
}
