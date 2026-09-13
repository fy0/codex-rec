//! Rewriting the `<environment_context>` user block inside a Responses request (§ timezone.).
//!
//! codex injects the block as a `role: "user"` item in `input`, so this module walks the request body
//! looking for items whose text contains `<environment_context>` and edits only the tag values that
//! were configured:
//!
//! * `timezone`      — replace `<timezone>…</timezone>`
//! * `current_date`  — replace `<current_date>…</current_date>`; the value is **converted** to the
//!   configured zone (or the element's own zone), never copied blindly. `"auto"` follows `timezone`,
//!   an explicit `YYYY-MM-DD` is used as-is, and `"shift:+1d"` / `"shift:-7d"` are relative.
//! * `cwd`, `shell`, `workspace_roots` — plain replacements
//! * `drop`          — remove the whole `<environment_context>` block (or named tags) outright
//!
//! Absent keys change nothing (same rule as the persona); `fill_missing` decides whether an element
//! that the client did not send is created.
//!
//! `now` is injectable so the date conversion is testable and deterministic.

use serde::Deserialize;
use serde_json::Value;

use crate::tz::{self, Zone};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSection {
    /// Replacement `<timezone>` value: `+08:00`, `UTC`, or an IANA name.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Replacement `<current_date>`: `auto`, `YYYY-MM-DD`, or `shift±Nd`.
    #[serde(default)]
    pub current_date: Option<String>,
    /// Replacement `<cwd>` value.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Replacement `<shell>` value.
    #[serde(default)]
    pub shell: Option<String>,
    /// Replacement `<root>…</root>` entries inside `<workspace_roots>`.
    #[serde(default)]
    pub workspace_roots: Option<Vec<String>>,
    /// Tags to delete; the special value `environment_context` removes the whole block.
    #[serde(default)]
    pub drop: Vec<String>,
    /// Create an element the client did not send (default `false` = never invent fields).
    #[serde(default)]
    pub fill_missing: bool,
}

impl EnvironmentSection {
    pub fn is_active(&self) -> bool {
        !self.drop.is_empty()
            || self.timezone.is_some()
            || self.current_date.is_some()
            || self.cwd.is_some()
            || self.shell.is_some()
            || self.workspace_roots.is_some()
    }
}

/// What the rewrite did, for the audit trail.
#[derive(Debug, Default)]
pub struct Notes {
    pub changed: bool,
    pub items_touched: usize,
    pub detail: Vec<String>,
}

/// Rewrites `body["input"]` in place. Returns what changed (empty when the section is inactive or no
/// block was found).
pub fn apply_env(section: &EnvironmentSection, body: &mut Value, now: Option<i64>) -> Notes {
    let mut notes = Notes::default();
    let mut zone: Option<Zone> = match section.timezone.as_deref() {
        Some(raw) => match Zone::parse(raw) {
            Ok(zone) => {
                notes.detail.push(format!("timezone -> {}", zone_name(raw)));
                Some(zone)
            }
            Err(err) => {
                notes.detail.push(format!("timezone ignored: {err}"));
                None
            }
        },
        None => None,
    };
    if zone.is_none() {
        // the requested zone could not be parsed (already reported); fall back to the block's own
        if let Err(err) = section.timezone.as_deref().map(Zone::parse).unwrap_or(Ok(Zone::Offset(0))) {
            notes.detail.push(format!("timezone ignored: {err}"));
        }
    }
    // When only current_date is configured the date still has to be converted, using the element's
    // own zone (or the requested one when the configured value is `auto`).
    let Some(items) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return notes;
    };
    for item in items.iter_mut() {
        if let Some(text) = item_text_mut(item) {
            if !text.contains("<environment_context>") {
                continue;
            }
            if let Some(updated) = rewrite_block(section, text, &mut zone, now, &mut notes) {
                *text = updated;
                notes.items_touched += 1;
                notes.changed = true;
            }
        }
    }
    if notes.items_touched > 0 {
        notes
            .detail
            .push(format!("environment_context rewritten in {} item(s)", notes.items_touched));
    }
    notes
}

fn zone_name(raw: &str) -> String {
    raw.trim().to_owned()
}

/// Finds the mutable text of a `message`-style input item.
fn item_text_mut(item: &mut Value) -> Option<&mut String> {
    let parts = item.get_mut("content")?.as_array_mut()?;
    for part in parts.iter_mut() {
        let is_text = matches!(part.get("text"), Some(Value::String(_)));
        if is_text {
            if let Some(Value::String(text)) = part.get_mut("text") {
                return Some(text);
            }
        }
    }
    None
}

fn rewrite_block(
    section: &EnvironmentSection,
    text: &str,
    zone: &mut Option<Zone>,
    now: Option<i64>,
    notes: &mut Notes,
) -> Option<String> {
    let mut out = text.to_owned();
    let mut touched = false;

    // 1. resolve the effective zone: explicit config first, otherwise the one the client sent.
    if zone.is_none() {
        if let Some((_, vs, ve, _)) = tz::element_span(&out, "timezone") {
            let sent = out[vs..ve].to_owned();
            if let Ok(parsed) = Zone::parse(&sent) {
                *zone = Some(parsed);
            }
        }
    }
    let effective_zone = zone.clone();

    // 2. drop whole tags before editing anything else.
    for tag in section.drop.iter() {
        let tag = tag.trim();
        if tag == "environment_context" {
            if let Some(start) = out.find("<environment_context>") {
                if let Some(end) = out[start..].find("</environment_context>") {
                    let end = start + end + "</environment_context>".len();
                    out.replace_range(start..end, "");
                    touched = true;
                    notes.detail.push("dropped the whole environment_context block".to_owned());
                }
            }
            continue;
        }
        if let Some((s, _, _, e)) = tz::element_span(&out, tag) {
            out.replace_range(s..e, "");
            touched = true;
            notes.detail.push(format!("dropped <{tag}>"));
        }
    }

    // 3. timezone
    if let Some(raw) = section.timezone.as_deref() {
        let value = raw.trim();
        match tz::element_span(&out, "timezone") {
            Some((_, vs, ve, _)) => {
                if &out[vs..ve] != value {
                    out.replace_range(vs..ve, value);
                    touched = true;
                }
            }
            None if section.fill_missing => {
                if let Some(idx) = insert_point(&out, &["current_date", "network", "filesystem"]) {
                    out.insert_str(idx, &format!("  <timezone>{value}</timezone>\n"));
                    touched = true;
                }
            }
            None => {}
        }
    }

    // 4. current_date (converted, not copied)
    if let Some(spec) = section.current_date.as_deref() {
        if let Some(value) = resolve_date(spec, effective_zone.as_ref(), now, notes) {
            match tz::element_span(&out, "current_date") {
                Some((_, vs, ve, _)) => {
                    if &out[vs..ve] != value {
                        out.replace_range(vs..ve, &value);
                        touched = true;
                    }
                }
                None if section.fill_missing => {
                    if let Some(idx) = insert_point(&out, &["timezone", "network", "filesystem"]) {
                        out.insert_str(idx, &format!("  <current_date>{value}</current_date>
"));
                        touched = true;
                    }
                }
                None => {}
            }
        }
    }


    // 5. plain text elements
    if let Some(value) = section.cwd.as_deref() {
        if replace_or_insert(&mut out, "cwd", value, section.fill_missing) {
            touched = true;
            notes.detail.push("cwd replaced".to_owned());
        }
    }
    if let Some(value) = section.shell.as_deref() {
        if replace_or_insert(&mut out, "shell", value, section.fill_missing) {
            touched = true;
            notes.detail.push("shell replaced".to_owned());
        }
    }

    // 6. workspace roots
    if let Some(roots) = section.workspace_roots.as_ref() {
        let rendered: String = roots
            .iter()
            .map(|root| format!("<root>{}</root>", xml_escape(root)))
            .collect();
        if let Some((_, vs, ve, _)) = tz::element_span(&out, "workspace_roots") {
            if &out[vs..ve] != rendered {
                out.replace_range(vs..ve, &rendered);
                touched = true;
                notes.detail.push(format!("workspace_roots -> {roots:?}"));
            }
        } else if section.fill_missing {
            if let Some((_, vs, _, _)) = tz::element_span(&out, "filesystem") {
                out.insert_str(vs, &format!("<workspace_roots>{rendered}</workspace_roots>"));
                touched = true;
            }
        }
    }

    touched.then_some(out)
}

/// Replaces `<tag>…</tag>`; optionally inserts it after `<cwd>` when missing.
fn replace_or_insert(out: &mut String, tag: &str, value: &str, fill_missing: bool) -> bool {
    match tz::element_span(out, tag) {
        Some((_, vs, ve, _)) => {
            if &out[vs..ve] == value {
                return false;
            }
            out.replace_range(vs..ve, value);
            true
        }
        None if fill_missing => {
            let escaped = xml_escape(value);
            if let Some(idx) = insert_point(out, &["shell", "filesystem"]) {
                let element = format!("  <{tag}>{escaped}</{tag}>\n");
                out.insert_str(idx, &element);
                true
            } else {
                false
            }
        }
        None => false,
    }
}

/// Byte offset just before the first of `anchors` that appears inside the block (used to keep the
/// element order stable when filling in missing values).
fn insert_point(out: &str, anchors: &[&str]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for anchor in anchors {
        if let Some(idx) = out.find(&format!("<{anchor}>")) {
            best = Some(best.map_or(idx, |b: usize| b.min(idx)));
        }
    }
    best.or_else(|| out.find("</environment_context>"))
}

fn resolve_date(
    spec: &str,
    zone: Option<&Zone>,
    now: Option<i64>,
    notes: &mut Notes,
) -> Option<String> {
    let spec = spec.trim();
    // Tests (or an operator debugging a zone) can pin the instant; otherwise use the wall clock.
    let secs = now
        .or_else(|| std::env::var("CODEX_REC_NOW_TS").ok().and_then(|v| v.trim().parse::<i64>().ok()))
        .unwrap_or_else(|| (crate::timeutil::now_ms() / 1000) as i64);
    if spec.eq_ignore_ascii_case("auto") {
        let zone = zone?;
        let date = zone.date_at(secs);
        notes.detail.push(format!("current_date -> {date} (converted)"));
        return Some(date);
    }
    if let Some(days) = spec
        .strip_prefix("shift")
        .map(|rest| rest.trim_start_matches([':', '+']))
        .and_then(|rest| rest.strip_suffix('d').or(Some(rest)))
        .and_then(|rest| rest.trim().parse::<i64>().ok())
    {
        let zone = zone.cloned().unwrap_or(Zone::Offset(0));
        let date = zone.date_at(secs + days * 86_400);
        notes.detail.push(format!("current_date -> {date} (shift {days}d)"));
        return Some(date);
    }
    // literal date: keep as-is (validated lightly)
    if spec.len() == 10 && spec.as_bytes()[4] == b'-' && spec.as_bytes()[7] == b'-' {
        notes.detail.push(format!("current_date -> {spec} (fixed)"));
        return Some(spec.to_owned());
    }
    notes
        .detail
        .push(format!("current_date ignored: cannot parse {spec:?}"));
    None
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BLOCK: &str = "<environment_context>\n  <cwd>/root/code</cwd>\n  <shell>bash</shell>\n  <current_date>2026-09-12</current_date>\n  <timezone>Etc/UTC</timezone>\n  <filesystem><workspace_roots><root>/root/code</root></workspace_roots><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>\n</environment_context>";

    /// 2026-09-12T20:30:00Z
    const NOW: i64 = 1_789_254_600;

    fn body() -> Value {
        json!({
            "model": "gpt-6-astra",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": BLOCK}]},
            ]
        })
    }

    fn text_of(body: &Value) -> String {
        body["input"][1]["content"][0]["text"].as_str().unwrap().to_owned()
    }

    #[test]
    fn absent_keys_change_nothing() {
        let section = EnvironmentSection::default();
        let mut b = body();
        let before = text_of(&b);
        let notes = apply_env(&section, &mut b, Some(NOW));
        assert!(!notes.changed);
        assert_eq!(text_of(&b), before);
    }

    #[test]
    fn timezone_and_date_are_rewritten_and_converted() {
        let section: EnvironmentSection =
            toml::from_str("timezone = \"+08:00\"\ncurrent_date = \"auto\"").unwrap();
        let mut b = body();
        let notes = apply_env(&section, &mut b, Some(NOW));
        let text = text_of(&b);
        assert!(notes.changed);
        assert!(text.contains("<timezone>+08:00</timezone>"), "{text}");
        // 20:30Z on the 12th is already the 13th at +08:00
        assert!(text.contains("<current_date>2026-09-13</current_date>"), "{text}");
    }

    #[test]
    fn date_follows_the_blocks_own_zone_when_only_date_is_configured() {
        // Etc/UTC in the block + explicit Los Angeles request -> converted back to the 12th
        let section: EnvironmentSection =
            toml::from_str("timezone = \"America/Los_Angeles\"\ncurrent_date = \"auto\"").unwrap();
        let mut b = body();
        apply_env(&section, &mut b, Some(NOW));
        let text = text_of(&b);
        assert!(text.contains("<timezone>America/Los_Angeles</timezone>"), "{text}");
        assert!(text.contains("<current_date>2026-09-12</current_date>"), "{text}");
    }

    #[test]
    fn shift_and_literal_dates_work() {
        let shift: EnvironmentSection =
            toml::from_str("timezone = \"+00:00\"\ncurrent_date = \"shift:+2d\"").unwrap();
        let mut b = body();
        apply_env(&shift, &mut b, Some(NOW));
        assert!(text_of(&b).contains("<current_date>2026-09-14</current_date>"));

        let literal: EnvironmentSection =
            toml::from_str("current_date = \"2020-01-01\"").unwrap();
        let mut b = body();
        apply_env(&literal, &mut b, Some(NOW));
        assert!(text_of(&b).contains("<current_date>2020-01-01</current_date>"));
    }

    #[test]
    fn cwd_shell_and_roots_are_replaced() {
        let section: EnvironmentSection = toml::from_str(
            "cwd = \"/srv/app\"\nshell = \"zsh\"\nworkspace_roots = [\"/srv/app\", \"/srv/lib\"]",
        )
        .unwrap();
        let mut b = body();
        apply_env(&section, &mut b, Some(NOW));
        let text = text_of(&b);
        assert!(text.contains("<cwd>/srv/app</cwd>"), "{text}");
        assert!(text.contains("<shell>zsh</shell>"), "{text}");
        assert!(
            text.contains("<workspace_roots><root>/srv/app</root><root>/srv/lib</root></workspace_roots>"),
            "{text}"
        );
    }

    #[test]
    fn drop_removes_tags_or_the_whole_block() {
        let one: EnvironmentSection = toml::from_str("drop = [\"timezone\"]").unwrap();
        let mut b = body();
        apply_env(&one, &mut b, Some(NOW));
        assert!(!text_of(&b).contains("<timezone>"));

        let whole: EnvironmentSection = toml::from_str("drop = [\"environment_context\"]").unwrap();
        let mut b = body();
        let notes = apply_env(&whole, &mut b, Some(NOW));
        assert!(notes.changed);
        assert_eq!(text_of(&b), "");
    }

    #[test]
    fn missing_elements_are_only_created_when_fill_missing_is_on() {
        let block = "<environment_context>\n  <cwd>/x</cwd>\n</environment_context>";
        let make = || {
            json!({"input": [{"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": block}]}]})
        };
        let off: EnvironmentSection = toml::from_str("timezone = \"+09:00\"").unwrap();
        let mut b = make();
        apply_env(&off, &mut b, Some(NOW));
        assert!(!b["input"][0]["content"][0]["text"].as_str().unwrap().contains("timezone"));

        let on: EnvironmentSection =
            toml::from_str("timezone = \"+09:00\"\nfill_missing = true").unwrap();
        let mut b = make();
        apply_env(&on, &mut b, Some(NOW));
        assert!(b["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<timezone>+09:00</timezone>"));
    }

    #[test]
    fn only_the_environment_item_is_touched() {
        let section: EnvironmentSection = toml::from_str("timezone = \"+08:00\"").unwrap();
        let mut b = body();
        apply_env(&section, &mut b, Some(NOW));
        assert_eq!(b["input"][0]["content"][0]["text"], "hi");
        assert_eq!(b["model"], "gpt-6-astra");
    }

    #[test]
    fn a_body_without_the_block_is_untouched() {
        let section: EnvironmentSection = toml::from_str("timezone = \"+08:00\"").unwrap();
        let mut b = json!({"input": [{"role": "user", "content": [{"type": "input_text", "text": "hello"}]}]});
        let notes = apply_env(&section, &mut b, Some(NOW));
        assert!(!notes.changed);
        assert_eq!(b["input"][0]["content"][0]["text"], "hello");
    }
}
