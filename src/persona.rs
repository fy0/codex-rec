//! Device persona: rebuild the outgoing identity (User-Agent, `originator`, `?client_version=`)
//! from the incoming request.
//!
//! Field semantics: a key that is **absent** inherits the client's value, exactly like the
//! explicit alias `"inherit"`; any other value replaces that part. `terminal = ""` removes the
//! terminal segment (`""` is rejected for the other fields, see `config::Persona::validate`).
//! If the incoming User-Agent cannot be parsed we leave it untouched rather than inventing one.
//!
//! Measured real-codex User-Agent shapes (codex-cli 0.153.4, Linux):
//!   full  : `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)`
//!   no tail: `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a`
//!   short : `codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)`

use crate::config::{Config, Field, Persona};

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

/// Resolves one persona field: absent or `"inherit"` -> the client's value, otherwise the value.
/// Returns `None` when the client's value was requested but the incoming User-Agent had none
/// (the caller then leaves the whole User-Agent untouched).
fn resolve_part(field: &Option<Field>, client: Option<&str>) -> Option<String> {
    match field {
        None => client.map(str::to_owned),
        Some(Field::Inherit) => client.map(str::to_owned),
        Some(Field::Set(value)) => Some(value.clone()),
    }
}

/// Returns the rewritten User-Agent (or `None` when nothing should change).
pub fn rewrite_user_agent(persona: &Persona, incoming: &str) -> Option<String> {
    // A literal User-Agent wins over every field above; "inherit" (or an absent key) means
    // "compose from the fields", so a persona that only pins the version still takes effect.
    if let Some(Field::Set(literal)) = persona.user_agent.as_ref() {
        return (literal != incoming).then(|| literal.clone());
    }

    let parsed = parse_ua(incoming)?;
    let originator = resolve_part(&persona.originator, Some(parsed.originator.as_str()))?;
    let version = resolve_part(&persona.codex_version, Some(parsed.version.as_str()))?;
    let os = resolve_part(&persona.os, Some(parsed.os.as_str()))?;
    let arch = resolve_part(&persona.arch, Some(parsed.arch.as_str()))?;
    let terminal = resolve_part(&persona.terminal, parsed.terminal.as_deref()).filter(|t| !t.is_empty());

    let parts = UaParts {
        originator,
        version,
        os,
        arch,
        terminal,
        suffix: parsed.suffix,
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

/// The originator value to force, or `None` when the client's own value stays.
fn override_originator(persona: &Persona) -> Option<&str> {
    match persona.originator.as_ref() {
        Some(Field::Set(value)) => Some(value.as_str()),
        _ => None,
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

    if let Some(originator) = override_originator(persona) {
        let key = reqwest::header::HeaderName::from_static("originator");
        if headers.contains_key(&key) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(originator) {
                headers.insert(key, value);
                notes.push(format!("originator -> {originator}"));
            }
        }
    }

    if persona.rewrite_client_version {
        if let Some(version) = persona.codex_version.as_ref().and_then(Field::value) {
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

    fn persona(toml_str: &str) -> Persona {
        toml::from_str(toml_str).expect("persona")
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
    fn omitted_fields_inherit_the_client() {
        // Only the version is pinned: OS, architecture and terminal stay exactly as the client sent.
        let p = persona(r#"codex_version = "0.145.0""#);
        let out = rewrite_user_agent(&p, CLIENT_UA).unwrap();
        assert_eq!(
            out,
            "codex-tui/0.145.0 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.145.0)"
        );
    }

    #[test]
    fn explicit_inherit_behaves_like_an_absent_key() {
        let absent = persona(r#"codex_version = "0.145.0""#);
        let explicit = persona(
            r#"
            codex_version = "0.145.0"
            originator = "inherit"
            os = "inherit"
            arch = "inherit"
            terminal = "inherit"
            "#,
        );
        assert_eq!(
            rewrite_user_agent(&absent, CLIENT_UA),
            rewrite_user_agent(&explicit, CLIENT_UA)
        );
    }

    #[test]
    fn empty_terminal_removes_the_segment() {
        let p = persona(r#"terminal = """#);
        let out = rewrite_user_agent(&p, CLIENT_UA).unwrap();
        assert_eq!(out, "codex-tui/0.153.4 (Debian 12.0.0; x86_64) (codex-tui; 0.153.4)");
    }

    #[test]
    fn literal_user_agent_wins() {
        let p = persona(
            r#"
            user_agent = "curl/7.88.1"
            originator = "x"
            "#,
        );
        assert_eq!(rewrite_user_agent(&p, CLIENT_UA).as_deref(), Some("curl/7.88.1"));
    }

    #[test]
    fn nothing_to_change_returns_none() {
        let p = persona(r#"originator = "codex-tui""#);
        assert_eq!(rewrite_user_agent(&p, CLIENT_UA), None);
        let p = persona(r#"os = "inherit""#);
        assert_eq!(rewrite_user_agent(&p, CLIENT_UA), None);
    }

    #[test]
    fn unparseable_user_agent_is_left_alone() {
        let p = persona(r#"codex_version = "0.145.0""#);
        assert_eq!(rewrite_user_agent(&p, "curl/7.88.1"), None);
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
