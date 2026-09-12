//! Device persona: rebuild the outgoing identity (User-Agent, `originator`, `?client_version=`)
//! from the incoming request, honouring per-field override-or-inherit settings.
//!
//! Measured real-codex User-Agent shapes (codex-cli 0.153.4, Linux):
//!   full  : `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)`
//!   no tail: `codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a`
//!   short : `codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)`
//! The last two are used for `/backend-api/codex/models` and `codex exec` respectively.

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

fn apply_field(field: &Field, target: &mut String) -> bool {
    match field.value() {
        Some(v) => {
            *target = v.to_owned();
            true
        }
        None => false,
    }
}

/// Returns the rewritten User-Agent (or `None` when nothing should change).
pub fn rewrite_user_agent(persona: &Persona, incoming: &str) -> Option<String> {
    if let Some(literal) = persona.user_agent.value() {
        return if literal == incoming {
            None
        } else {
            Some(literal.to_owned())
        };
    }
    if !persona.is_active() {
        return None;
    }
    let parsed = parse_ua(incoming);
    let mut parts = match parsed {
        Some(p) => p,
        None => {
            // Unknown shape: only rebuild when we have the pieces needed for the canonical form.
            let originator = persona.originator.value()?;
            let version = persona.codex_version.value()?;
            UaParts {
                originator: originator.to_owned(),
                version: version.to_owned(),
                os: persona.os.value().unwrap_or("Linux").to_owned(),
                arch: persona.arch.value().unwrap_or("x86_64").to_owned(),
                terminal: persona
                    .terminal
                    .value()
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned),
                suffix: false,
            }
        }
    };

    apply_field(&persona.originator, &mut parts.originator);
    apply_field(&persona.codex_version, &mut parts.version);
    apply_field(&persona.os, &mut parts.os);
    apply_field(&persona.arch, &mut parts.arch);
    match persona.terminal.value() {
        Some("") => parts.terminal = None,
        Some(v) => parts.terminal = Some(v.to_owned()),
        None => {}
    }

    let composed = compose_ua(&parts);
    if composed == incoming {
        None
    } else {
        Some(composed)
    }
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
        .map(|pair| {
            match pair.split_once('=') {
                Some((k, _)) if k == "client_version" => {
                    changed = true;
                    format!("{k}={version}")
                }
                _ => pair.to_owned(),
            }
        })
        .collect();
    if changed {
        Some(format!("{path}?{}", rebuilt.join("&")))
    } else {
        None
    }
}

/// Applies the persona to the outgoing header set and returns the headers that were changed.
pub fn apply(cfg: &Config, headers: &mut reqwest::header::HeaderMap, uri: &mut String) -> Vec<String> {
    let mut notes = Vec::new();
    let persona = &cfg.persona;
    if !persona.is_active() {
        return notes;
    }

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

    if let Some(originator) = persona.originator.value() {
        let key = reqwest::header::HeaderName::from_static("originator");
        if headers.contains_key(&key) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(originator) {
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

    fn persona(pairs: &[(&str, &str)]) -> Persona {
        let mut p = Persona::default();
        for (k, v) in pairs {
            let field = Field::Set((*v).to_owned());
            match *k {
                "originator" => p.originator = field,
                "codex_version" => p.codex_version = field,
                "os" => p.os = field,
                "arch" => p.arch = field,
                "terminal" => p.terminal = field,
                "user_agent" => p.user_agent = field,
                _ => {}
            }
        }
        p
    }

    #[test]
    fn parses_all_measured_shapes() {
        let full = parse_ua("codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)").unwrap();
        assert_eq!(full.originator, "codex-tui");
        assert_eq!(full.version, "0.153.4");
        assert_eq!(full.os, "Debian 12.0.0");
        assert_eq!(full.arch, "x86_64");
        assert_eq!(full.terminal.as_deref(), Some("tmux/3.3a"));
        assert!(full.suffix);
        assert_eq!(
            compose_ua(&full),
            "codex-tui/0.153.4 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.153.4)"
        );

        let short = parse_ua("codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)").unwrap();
        assert_eq!(short.terminal, None);
        assert!(!short.suffix);
        assert_eq!(compose_ua(&short), "codex_cli_rs/0.153.4 (Debian 12.0.0; x86_64)");
    }

    #[test]
    fn overrides_only_the_configured_fields() {
        let p = persona(&[("codex_version", "0.145.0"), ("terminal", "tmux/3.3a")]);
        let out = rewrite_user_agent(&p, "codex-tui/0.153.4 (Debian 12.0.0; x86_64) xterm-256color (codex-tui; 0.153.4)").unwrap();
        assert_eq!(out, "codex-tui/0.145.0 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.145.0)");
    }

    #[test]
    fn inherit_leaves_everything_alone() {
        let p = Persona::default();
        assert!(!p.is_active());
        assert_eq!(rewrite_user_agent(&p, "anything"), None);
    }

    #[test]
    fn client_version_query_rewrite() {
        assert_eq!(
            rewrite_client_version("/backend-api/codex/models?client_version=0.153.4&x=1", "0.145.0").as_deref(),
            Some("/backend-api/codex/models?client_version=0.145.0&x=1")
        );
        assert_eq!(rewrite_client_version("/models", "0.145.0"), None);
    }
}
