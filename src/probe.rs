//! Reproduce the codex client's local environment probe, exactly like the original.
//!
//! Three things are printed:
//!
//! 1. **Codex's view** — the same fields the real client derives locally: the User-Agent it builds
//!    (`<originator>/<version> (<os> <os-version>; <arch>) <terminal> (<originator>; <version>)`), the
//!    `originator` and `?client_version=` it sends, the `<environment_context>` block it injects
//!    (`<cwd>`, `<shell>`, `<current_date>`, `<timezone>`, `<filesystem>`), and `codex --version`.
//!
//!    Implemented from `core/src/login/src/auth/default_client.rs` (`get_codex_user_agent`),
//!    `terminal-detection` (the probe order documented in `docs/configuration-guide.md`) and
//!    `core/src/session/world_state.rs` (`current_date`/`timezone`, rendered through `chrono::Local`).
//!
//! 2. **`[persona]`** — the same information expressed as this proxy's config. Values that match the
//!    current machine are printed commented out ("inherit"); everything that differs between the two
//!    inputs is pinned.
//!
//! 3. **`[rewrite.environment]`** — the same for the injected block: the machine's timezone becomes an
//!    explicit `timezone`, and `cwd`/`shell` are pinned only when `--cwd`/`--shell` change them.
//!
//! Usage:
//!   codex-rec probe [--cwd <path>] [--shell <name>] [--client-ua "<user agent>"] [--originator <name>]
//!
//! `--client-ua` is the "paste another machine's environment here" switch: give it the value that a
//! remote machine's probe printed (or the `user-agent` header in that machine's recording) and every
//! codex-native field (terminal, os, arch, version) is taken from there instead of from this host.

use std::env;
use std::path::PathBuf;

use serde_json::json;

use crate::persona::{self, UaParts};
use crate::tz::{self, Zone};
use crate::timeutil;

#[derive(Debug, Default)]
pub struct Args {
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub client_ua: Option<String>,
    pub originator: Option<String>,
}

pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let value = |i: usize| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        match argv[i].as_str() {
            "--cwd" => {
                args.cwd = Some(value(i)?);
                i += 2;
            }
            "--shell" => {
                args.shell = Some(value(i)?);
                i += 2;
            }
            "--client-ua" => {
                args.client_ua = Some(value(i)?);
                i += 2;
            }
            "--originator" => {
                args.originator = Some(value(i)?);
                i += 2;
            }
            other => return Err(format!("unknown probe argument {other:?}")),
        }
    }
    Ok(args)
}

/// Detect the terminal segment exactly like `codex-rs/terminal-detection`.
pub fn terminal_token(env_lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let var = |name: &str| env_lookup(name).filter(|v| !v.trim().is_empty());
    let multiplexer = detect_multiplexer(env_lookup);
    // 1. TERM_PROGRAM (unless it names tmux, which is tracked as a multiplexer instead)
    if let Some(program) = var("TERM_PROGRAM") {
        if program.eq_ignore_ascii_case("tmux") {
            return multiplexer.unwrap_or_else(|| format_program(&program, var("TERM_PROGRAM_VERSION")));
        }
        return format_program(&program, var("TERM_PROGRAM_VERSION"));
    }
    let direct = [
        ("GHOSTTY_RESOURCES_DIR", "Ghostty", None),
        ("WEZTERM_VERSION", "WezTerm", Some("WEZTERM_VERSION")),
        ("ITERM_SESSION_ID", "iTerm.app", None),
        ("ITERM_PROFILE", "iTerm.app", None),
        ("ITERM_PROFILE_NAME", "iTerm.app", None),
        ("TERM_SESSION_ID", "Apple_Terminal", None),
        ("KITTY_WINDOW_ID", "kitty", None),
        ("ALACRITTY_SOCKET", "Alacritty", None),
        ("KONSOLE_VERSION", "Konsole", Some("KONSOLE_VERSION")),
        ("GNOME_TERMINAL_SCREEN", "gnome-terminal", None),
        ("VTE_VERSION", "VTE", Some("VTE_VERSION")),
        ("WT_SESSION", "WindowsTerminal", None),
    ];
    for (probe, name, version_var) in direct {
        if env_lookup(probe).is_some() {
            let version = version_var.and_then(var);
            if multiplexer.is_some() && name == "WindowsTerminal" {
                // codex keeps the multiplexer as the token when one is present
                return multiplexer.unwrap();
            }
            return format_program(name, version);
        }
    }
    if let Some(term) = var("TERM") {
        if term == "dumb" {
            return "dumb".to_owned();
        }
        return multiplexer.unwrap_or(term);
    }
    multiplexer.unwrap_or_else(|| "unknown".to_owned())
}

fn format_program(name: &str, version: Option<String>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(version) => format!("{name}/{version}"),
        None => name.to_owned(),
    }
}

fn detect_multiplexer(env_lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let non_empty = |name: &str| env_lookup(name).filter(|v| !v.trim().is_empty());
    if non_empty("TMUX").is_some() || non_empty("TMUX_PANE").is_some() {
        let version = env_lookup("TERM_PROGRAM")
            .filter(|p| p.eq_ignore_ascii_case("tmux"))
            .and_then(|_| non_empty("TERM_PROGRAM_VERSION"));
        return Some(format_program("tmux", version));
    }
    if non_empty("ZELLIJ").is_some()
        || non_empty("ZELLIJ_SESSION_NAME").is_some()
        || non_empty("ZELLIJ_VERSION").is_some()
    {
        return Some(format_program("zellij", non_empty("ZELLIJ_VERSION")));
    }
    None
}

/// `dirname $(ps -p $PPID -o comm=)`-style fallback: on Windows the shell is powershell (or cmd).
pub fn detect_shell() -> String {
    if cfg!(windows) {
        if env::var("PSModulePath").is_ok() {
            return "powershell".to_owned();
        }
        return "cmd".to_owned();
    }
    for key in ["SHELL", "COMSPEC"] {
        if let Some(value) = env::var(key).ok().filter(|v| !v.is_empty()) {
            if let Some(name) = PathBuf::from(&value).file_name().and_then(|f| f.to_str()) {
                return name.to_owned();
            }
        }
    }
    "sh".to_owned()
}

/// The machine's timezone, using the same order codex's `chrono::Local` ends up with.
pub fn local_zone() -> Zone {
    if let Some(zone) = tz::from_env() {
        return zone;
    }
    if let Some(zone) = tz::from_system() {
        return zone;
    }
    Zone::Offset(0)
}

/// The `codex --version` string, if the CLI is on PATH.
pub fn codex_cli_version() -> Option<String> {
    let mut command = std::process::Command::new("codex");
    if cfg!(windows) {
        command = std::process::Command::new("codex.cmd");
    }
    let output = command.arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

/// Takes `--client-ua` (a User-Agent from anywhere) plus this host, and produces the codex-native
/// fields and the two config sections.
fn build(args: &Args) -> (UaParts, Zone, String, String, String) {
    let lookup = |name: &str| env::var(name).ok();
    let host_terminal = terminal_token(&lookup);
    let host_os = format!("{} {}", os_info::get().os_type(), os_info::get().version());
    let host_arch = os_info::get()
        .architecture()
        .map(|a| a.to_string())
        .unwrap_or_else(|| env::consts::ARCH.to_owned());

    // Defaults describe *this* machine; --client-ua overrides the codex-native parts.
    let mut parts = UaParts {
        originator: args.originator.clone().unwrap_or_else(|| "codex-tui".to_owned()),
        version: codex_cli_version()
            .and_then(|v| v.rsplit(' ').next().map(str::to_owned))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
        os: host_os,
        arch: host_arch,
        terminal: Some(host_terminal),
        suffix: true,
    };
    if let Some(ua) = args.client_ua.as_deref() {
        if let Some(parsed) = persona::parse_ua(ua) {
            parts = parsed;
            if let Some(originator) = args.originator.clone() {
                parts.originator = originator;
            }
        } else {
            eprintln!("warning: --client-ua does not look like a codex User-Agent; ignoring it");
        }
    }

    let zone = local_zone();
    let cwd_own = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_owned());
    let cwd = args.cwd.clone().unwrap_or(cwd_own);
    let shell = args.shell.clone().unwrap_or_else(detect_shell);
    (parts, zone, cwd, shell, args.cwd.clone().unwrap_or_default())
}

/// `true` when the rendered value differs from what this machine would produce on its own.
fn differs(value: &str, own: &str) -> bool {
    value != own
}

pub fn run(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("probe: {err}");
            return 2;
        }
    };
    let (parts, zone, cwd, shell, cwd_override) = build(&args);
    let ua = persona::compose_ua(&parts);
    let today = zone.date_at((timeutil::now_ms() / 1000) as i64);
    let cli = codex_cli_version();
    let codex_version = cli
        .as_deref()
        .and_then(|v| v.rsplit(' ').next())
        .unwrap_or("unknown");

    let host_lookup = |name: &str| env::var(name).ok();
    let host_zone = local_zone();
    let host_cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let host_shell = detect_shell();

    println!("=== 1. codex's own local view (as the client would use it) ===");
    println!("  user-agent      : {ua}");
    println!("  originator      : {}", parts.originator);
    println!("  client_version  : {}", parts.version);
    println!("  user_agent suffix (tail segment present): {}", parts.suffix);
    println!("  os              : {}", parts.os);
    println!("  arch            : {}", parts.arch);
    println!("  terminal        : {}", parts.terminal.as_deref().unwrap_or("<none>"));
    println!("  terminal probe  : TERM={:?} TERM_PROGRAM={:?} TMUX={:?} WT_SESSION={:?}",
        env::var("TERM").ok(),
        env::var("TERM_PROGRAM").ok(),
        env::var("TMUX").ok(),
        env::var("WT_SESSION").ok());
    println!("  shell           : {shell}");
    println!("  cwd             : {cwd}");
    println!("  timezone        : {}", zone_label(&zone));
    println!("  current_date    : {today}");
    if let Some(cli) = cli.as_deref() {
        println!("  codex --version : {cli}");
    }
    println!("  environment_context block:");
    for line in env_block(&cwd, &shell, &today, &zone_label(&zone)).lines() {
        println!("    {line}");
    }

    println!();
    println!("=== 2. codex-rec config for this machine (paste into codex-rec.toml) ===");
    println!("# generated by `codex-rec probe` — values equal to this machine are left commented out");
    println!("# (a commented-out key = inherit the client's value; see docs/configuration-guide.md)");
    println!("[persona]");
    print_field("originator", &parts.originator, "codex-tui", true);
    print_field("codex_version", &parts.version, codex_version, true);
    print_field("os", &parts.os, &format!("{} {}", os_info::get().os_type(), os_info::get().version()), true);
    let host_arch = os_info::get()
        .architecture()
        .map(|a| a.to_string())
        .unwrap_or_else(|| env::consts::ARCH.to_owned());
    print_field("arch", &parts.arch, &host_arch, true);
    print_field(
        "terminal",
        parts.terminal.as_deref().unwrap_or(""),
        &terminal_token(&host_lookup),
        true,
    );
    if differs(codex_version, &parts.version) {
        println!("rewrite_client_version = true    # ?client_version= follows codex_version");
    } else {
        println!("# rewrite_client_version = false # commented out = leave ?client_version= alone");
    }

    println!();
    println!("[headers.request]");
    println!("# drop = [\"cf-*\"]              # add CDN headers you want removed");
    println!();
    println!("[rewrite.environment]");
    let zone_text = zone_label(&zone);
    let host_text = zone_label(&host_zone);
    if zone_text != host_text {
        println!("timezone     = \"{zone_text}\"   # pinned (differs from this machine's {host_text})");
    } else {
        println!("# timezone     = \"{zone_text}\"   # commented out = inherit the client's value");
    }
    println!("current_date = \"auto\"          # converted to the zone above");
    if differs(&cwd, &host_cwd) {
        println!("cwd          = \"{}\"", cwd.replace('\\', "/"));
    } else {
        println!("# cwd          = \"{}\"          # commented out = inherit", cwd.replace('\\', "/"));
    }
    if differs(&shell, &host_shell) {
        println!("shell        = \"{shell}\"");
    } else {
        println!("# shell        = \"{shell}\"          # commented out = inherit");
    }
    if !cwd_override.is_empty() {
        println!(
            "workspace_roots = [\"{}\"]   # follows the pinned cwd",
            cwd.replace('\\', "/")
        );
    } else {
        println!("# workspace_roots = []          # commented out = inherit");
    }
    println!("# drop         = []              # commented out = drop nothing");
    println!("# fill_missing = false           # never invent elements the client did not send");

    println!();
    println!("=== 3. machine-readable ===");
    let json = json!({
        "user_agent": ua,
        "originator": parts.originator,
        "client_version": parts.version,
        "os": parts.os,
        "arch": parts.arch,
        "terminal": parts.terminal,
        "shell": shell,
        "cwd": cwd,
        "timezone": zone_label(&zone),
        "current_date": today,
        "codex_cli_version": cli,
        "environment_context": env_block(&cwd, &shell, &today, &zone_label(&zone)),
    });
    println!("{}", serde_json::to_string_pretty(&json).unwrap_or_default());
    println!();
    println!("tip: to reuse another machine's environment, run `codex-rec probe` there and paste its");
    println!("     user-agent with: codex-rec probe --client-ua \"<that user agent>\"");
    0
}

fn print_field(name: &str, value: &str, own: &str, quote_always: bool) {
    if differs(value, own) {
        if quote_always {
            println!("{name:<14}= \"{value}\"");
        } else {
            println!("{name:<14}= {value}");
        }
    } else {
        println!("# {name:<12}= ...            # commented out = inherit this value");
    }
}

/// `+08:00` for fixed offsets, the IANA name for named zones (matching what codex renders).
fn zone_label(zone: &Zone) -> String {
    match zone {
        Zone::Named(name) => name.clone(),
        Zone::Offset(seconds) => {
            let sign = if *seconds < 0 { '-' } else { '+' };
            let abs = seconds.abs();
            format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
        }
    }
}

/// Renders the `<environment_context>` block the client injects (filesystem part with an
/// unrestricted profile, which is what a Windows host reports by default).
fn env_block(cwd: &str, shell: &str, today: &str, zone: &str) -> String {
    format!(
        "<environment_context>\n  <cwd>{cwd}</cwd>\n  <shell>{shell}</shell>\n  <current_date>{today}</current_date>\n  <timezone>{zone}</timezone>\n  <filesystem><workspace_roots><root>{cwd}</root></workspace_roots><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>\n</environment_context>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn terminal_probe_order_matches_codex() {
        // TERM_PROGRAM wins and keeps its version
        let e = env_from(&[("TERM_PROGRAM", "iTerm.app"), ("TERM_PROGRAM_VERSION", "3.5.0")]);
        assert_eq!(terminal_token(&e), "iTerm.app/3.5.0");
        // Windows Terminal is detected from WT_SESSION alone
        let e = env_from(&[("WT_SESSION", "abc")]);
        assert_eq!(terminal_token(&e), "WindowsTerminal");
        // a bare console has no probe at all
        let e = env_from(&[]);
        assert_eq!(terminal_token(&e), "unknown");
        // TERM is the last resort
        let e = env_from(&[("TERM", "xterm-256color")]);
        assert_eq!(terminal_token(&e), "xterm-256color");
        // tmux masks the terminal and supplies its own version
        let e = env_from(&[("TMUX", "/tmp/tmux"), ("TERM_PROGRAM", "tmux"), ("TERM_PROGRAM_VERSION", "3.3a")]);
        assert_eq!(terminal_token(&e), "tmux/3.3a");
        // ... but a real terminal underneath tmux still reports the multiplexer (codex behaviour)
        let e = env_from(&[("TMUX", "/tmp/tmux"), ("TERM", "screen")]);
        assert_eq!(terminal_token(&e), "tmux");
        // vscode / warp / ghostty / kitty / vte
        assert_eq!(terminal_token(&env_from(&[("TERM_PROGRAM", "vscode")])), "vscode");
        assert_eq!(terminal_token(&env_from(&[("GHOSTTY_RESOURCES_DIR", "/g")])), "Ghostty");
        assert_eq!(terminal_token(&env_from(&[("KITTY_WINDOW_ID", "1")])), "kitty");
        assert_eq!(terminal_token(&env_from(&[("VTE_VERSION", "7000")])), "VTE/7000");
        assert_eq!(terminal_token(&env_from(&[("TERM", "dumb")])), "dumb");
    }

    #[test]
    fn zone_labels_match_codex_rendering() {
        assert_eq!(zone_label(&Zone::Named("Asia/Taipei".to_owned())), "Asia/Taipei");
        assert_eq!(zone_label(&Zone::Offset(8 * 3600)), "+08:00");
        assert_eq!(zone_label(&Zone::Offset(-(5 * 3600 + 30 * 60))), "-05:30");
        assert_eq!(zone_label(&Zone::Offset(0)), "+00:00");
    }

    #[test]
    fn env_block_shape() {
        let block = env_block("/root/code", "bash", "2026-09-13", "Asia/Taipei");
        assert!(block.starts_with("<environment_context>\n  <cwd>/root/code</cwd>"));
        assert!(block.contains("<shell>bash</shell>"));
        assert!(block.contains("<current_date>2026-09-13</current_date>"));
        assert!(block.contains("<timezone>Asia/Taipei</timezone>"));
        assert!(block.ends_with("</environment_context>"));
    }

    #[test]
    fn client_ua_round_trips_through_the_parser() {
        let ua = "codex-tui/0.145.0 (Debian 12.0.0; x86_64) tmux/3.3a (codex-tui; 0.145.0)";
        let parsed = persona::parse_ua(ua).expect("parse");
        assert_eq!(persona::compose_ua(&parsed), ua);
        assert_eq!(parsed.os, "Debian 12.0.0");
        assert_eq!(parsed.terminal.as_deref(), Some("tmux/3.3a"));
    }

    #[test]
    fn args_parse() {
        let args = parse_args(&[
            "--cwd".to_owned(),
            "/srv/app".to_owned(),
            "--client-ua".to_owned(),
            "codex-tui/0.153.4 (Debian 12.0.0; x86_64)".to_owned(),
        ])
        .unwrap();
        assert_eq!(args.cwd.as_deref(), Some("/srv/app"));
        assert!(args.client_ua.is_some());
        assert!(parse_args(&["--nope".to_owned()]).is_err());
        assert!(parse_args(&["--cwd".to_owned()]).is_err());
    }
}
