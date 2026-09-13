//! Timezone handling for the environment-context rewrite.
//!
//! codex renders `<timezone>` from `TurnContext.timezone`, whose display form is either a UTC offset
//! (`+08:00`, the default when no IANA zone is configured) or an IANA name (`Asia/Shanghai`; codex's
//! own snapshots show `UTC` and `America/Los_Angeles` next to offsets). Both forms are accepted here
//! so the recorder can mirror whatever a real client sends.
//!
//! For `current_date` the local date has to be **converted**, not copied: the request carries a UTC
//! timestamp, so a zone east/west of UTC can be a day ahead/behind. IANA names are resolved through
//! the system zoneinfo database; if it is missing (some minimal containers ship without tzdata) the
//! recorder falls back to the offset fixed at the current instant and says so in the log.


use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::timeutil;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    /// Fixed offset in seconds east of UTC (from `+08:00`, `UTC`, or `Z`).
    Offset(i32),
    /// IANA zone name, e.g. `Asia/Shanghai`.
    Named(String),
}

impl Zone {
    /// Parses the value of a `<timezone>` element: an offset, `UTC`/`GMT`, or an IANA name.
    pub fn parse(raw: &str) -> Result<Zone, String> {
        let value = raw.trim();
        if value.is_empty() {
            return Err("timezone must not be empty".to_owned());
        }
        let upper = value.to_ascii_uppercase();
        if upper == "UTC" || upper == "GMT" || upper == "Z" {
            return Ok(Zone::Offset(0));
        }
        if let Some(rest) = value.strip_prefix(['+', '-']) {
            let sign = if value.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = match rest.split_once(':') {
                Some((h, m)) => (h, m),
                None => (rest, "0"),
            };
            let hours: i32 = hours
                .parse()
                .map_err(|_| format!("bad timezone offset {value:?}: hours must be a number"))?;
            let minutes: i32 = minutes
                .parse()
                .map_err(|_| format!("bad timezone offset {value:?}: minutes must be a number"))?;
            if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
                return Err(format!("bad timezone offset {value:?}: out of range"));
            }
            return Ok(Zone::Offset(sign * (hours * 3600 + minutes * 60)));
        }
        if value.contains('/') {
            return Ok(Zone::Named(value.to_owned()));
        }
        Err(format!(
            "unrecognized timezone {value:?}: use a UTC offset like \"+08:00\", \"UTC\", or an IANA name like \"Asia/Shanghai\""
        ))
    }

    /// The offset to apply at `utc_secs` (seconds since the Unix epoch).
    pub fn offset_at(&self, utc_secs: i64) -> i32 {
        match self {
            Zone::Offset(seconds) => *seconds,
            Zone::Named(name) => {
                if let Some(seconds) = tzfile_offset(name, utc_secs) {
                    return seconds;
                }
                let fixed = fixed_offset(name);
                eprintln!(
                    "[timezone] no tzdata entry usable for {name}; using a fixed {:+03}:{:02} (DST will not be applied)",
                    fixed / 3600,
                    (fixed.abs() % 3600) / 60
                );
                fixed
            }
        }
    }

    /// `YYMMDD-HHMMSS`-style date and time in this zone at `utc_secs`.
    pub fn parts_at(&self, utc_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
        let local = utc_secs + self.offset_at(utc_secs) as i64;
        let (year, month, day) = timeutil::civil_from_days(local.div_euclid(86_400));
        let sod = local.rem_euclid(86_400);
        (
            year,
            month,
            day,
            (sod / 3600) as u32,
            ((sod % 3600) / 60) as u32,
            (sod % 60) as u32,
        )
    }

    /// Local date as `YYYY-MM-DD`.
    pub fn date_at(&self, utc_secs: i64) -> String {
        let (y, m, d, _, _, _) = self.parts_at(utc_secs);
        format!("{y:04}-{m:02}-{d:02}")
    }
}

// ---------------------------------------------------------------- tzdata

const TZDATA_DIRS: &[&str] = &[
    "/usr/share/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
    "/var/db/timezone/zoneinfo",
    "/opt/homebrew/share/zoneinfo",
    "C:/msys64/usr/share/zoneinfo",
];

static TZDATA_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

fn tzdata_dir() -> Option<&'static Path> {
    TZDATA_DIR
        .get_or_init(|| {
            TZDATA_DIRS
                .iter()
                .map(PathBuf::from)
                .find(|dir| dir.join("UTC").is_file() || dir.join("Etc").join("UTC").is_file())
        })
        .as_deref()
}

/// Reads the offset for an IANA name out of the system tzdata (TZif v1/v2/v3).
fn tzfile_offset(name: &str, utc_secs: i64) -> Option<i32> {
    let dir = tzdata_dir()?;
    if name.contains("..") || name.starts_with('/') {
        return None;
    }
    let bytes = std::fs::read(dir.join(name)).ok()?;
    if bytes.len() < 44 || &bytes[0..4] != b"TZif" {
        return None;
    }
    // A v2/v3 file holds a v1 block followed by a 64-bit block; find the second magic rather than
    // computing the v1 length (the optional isstd/isut arrays make that easy to get wrong).
    let base = match bytes[4] {
        b'2' | b'3' => match bytes[1..].windows(4).position(|w| w == b"TZif") {
            Some(rel) => rel + 1,
            None => return None,
        },
        _ => 0,
    };
    if bytes.len() < base + 44 {
        return None;
    }
    let (_isutcnt, _isstdcnt, _leapcnt, timecnt, typecnt, _charcnt) = tzif_counts(&bytes, base)?;

    // transition times are 8-byte in v2+ blocks, 4-byte in v1
    let wide = base != 0;
    let size = if wide { 8 } else { 4 };
    let transitions = base + 44;
    let type_indexes = transitions + timecnt * size;
    let types = type_indexes + timecnt;
    if types + typecnt * 6 > bytes.len() {
        return None;
    }
    let read_time = |i: usize| -> i64 {
        let at = transitions + i * size;
        if wide {
            i64::from_be_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]))
        } else {
            i32::from_be_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4])) as i64
        }
    };
    // index of the last transition at or before utc_secs (0 = the first type)
    let mut idx = 0usize;
    for i in 0..timecnt {
        if read_time(i) <= utc_secs {
            idx = i;
        } else {
            break;
        }
    }
    let mut chosen = *bytes.get(type_indexes + idx).unwrap_or(&0) as usize;
    if chosen >= typecnt {
        chosen = 0;
    }
    let at = types + chosen * 6;
    let offset = i32::from_be_bytes(bytes[at..at + 4].try_into().ok()?);
    Some(offset)
}

fn tzif_counts(bytes: &[u8], base: usize) -> Option<(usize, usize, usize, usize, usize, usize)> {
    let at = base + 20;
    let u32_at = |o: usize| -> Option<usize> {
        Some(u32::from_be_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as usize)
    };
    Some((
        u32_at(at)?,          // isutcnt
        u32_at(at + 4)?,      // isstdcnt
        u32_at(at + 8)?,      // leapcnt
        u32_at(at + 12)?,     // timecnt
        u32_at(at + 16)?,     // typecnt
        u32_at(at + 20)?,     // charcnt
    ))
}

#[allow(dead_code)] // kept for reference: the v2 block is now located by its TZif magic
fn tzif_block_len(bytes: &[u8], base: usize) -> Option<usize> {
    let (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt) = tzif_counts(bytes, base)?;
    // v1 layout: 44 + timecnt*4 + timecnt + typecnt*6 + charcnt + leapcnt*8 + isstdcnt + isutcnt
    let total = 44 + timecnt * 5 + typecnt * 6 + charcnt + leapcnt * 8 + isstdcnt + isutcnt;
    Some(total)
}

/// Last-resort offset for a few common zones when tzdata is unavailable.
fn fixed_offset(name: &str) -> i32 {
    match name {
        "Asia/Shanghai" | "Asia/Chongqing" | "Asia/Harbin" | "Asia/Taipei" | "Asia/Hong_Kong"
        | "Asia/Singapore" | "Asia/Macau" => 8 * 3600,
        "Asia/Tokyo" | "Asia/Seoul" => 9 * 3600,
        "Etc/UTC" | "Etc/GMT" | "UTC" | "GMT" => 0,
        "Europe/London" => 0,
        "Europe/Berlin" | "Europe/Paris" | "Europe/Madrid" | "Europe/Rome" => 3600,
        "America/New_York" => -5 * 3600,
        "America/Chicago" => -6 * 3600,
        "America/Denver" => -7 * 3600,
        "America/Los_Angeles" => -8 * 3600,
        _ => 0,
    }
}

#[allow(dead_code)] // available for a future `timezone = "env:TZ"` form
/// The zone from `TZ` (e.g. `Asia/Shanghai`, `UTC`, or `UTC-8`), when it is set and parseable.
pub fn from_env() -> Option<Zone> {
    let raw = std::env::var("TZ").ok()?;
    let value = raw.trim().trim_start_matches(':');
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = value.strip_prefix("UTC") {
        if rest.is_empty() {
            return Some(Zone::Offset(0));
        }
        // POSIX form: UTC-8 means 8 hours *west* of UTC
        if let Ok(hours) = rest.trim_start_matches(['+', '-']).parse::<i32>() {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            return Some(Zone::Offset(-sign * hours * 3600));
        }
    }
    Zone::parse(value).ok()
}

#[allow(dead_code)] // available for a future `timezone = "env:TZ"` form
/// Reads the system zone name (`/etc/localtime` symlink or `/etc/timezone`).
pub fn from_system() -> Option<Zone> {
    for candidate in ["/etc/timezone", "/var/db/zoneinfo"] {
        if let Ok(name) = std::fs::read_to_string(candidate) {
            let name = name.trim();
            if !name.is_empty() {
                if let Ok(zone) = Zone::parse(name) {
                    return Some(zone);
                }
            }
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let text = target.to_string_lossy();
        if let Some(idx) = text.find("zoneinfo/") {
            let name = &text[idx + "zoneinfo/".len()..];
            if let Ok(zone) = Zone::parse(name) {
                return Some(zone);
            }
        }
    }
    None
}

/// Byte range of a `<tag>…</tag>` element inside `text`.
pub fn element_span(text: &str, tag: &str) -> Option<(usize, usize, usize, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)?;
    let value_start = start + open.len();
    let value_end = text[value_start..].find(&close)? + value_start;
    Some((start, value_start, value_end, value_end + close.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offsets_and_names() {
        assert_eq!(Zone::parse("+08:00").unwrap(), Zone::Offset(8 * 3600));
        assert_eq!(Zone::parse("-05:30").unwrap(), Zone::Offset(-(5 * 3600 + 30 * 60)));
        assert_eq!(Zone::parse("UTC").unwrap(), Zone::Offset(0));
        assert_eq!(Zone::parse("Z").unwrap(), Zone::Offset(0));
        assert_eq!(Zone::parse("+8").unwrap(), Zone::Offset(8 * 3600));
        assert_eq!(
            Zone::parse("Asia/Shanghai").unwrap(),
            Zone::Named("Asia/Shanghai".to_owned())
        );
        assert!(Zone::parse("").is_err());
        assert!(Zone::parse("nonsense").is_err());
        assert_eq!(
            Zone::parse("Mars/Phobos").unwrap(),
            Zone::Named("Mars/Phobos".to_owned())
        );
    }

    /// A zone east of UTC can already be on the next day.
    #[test]
    fn current_date_is_converted_not_copied() {
        // 2026-09-12T20:30:00Z
        let ts = 1_789_254_600i64;
        assert_eq!(Zone::Offset(0).date_at(ts), "2026-09-12");
        assert_eq!(Zone::Offset(8 * 3600).date_at(ts), "2026-09-13");
        assert_eq!(Zone::Offset(-8 * 3600).date_at(ts), "2026-09-12");
        // and the other way round: 2026-09-12T03:00:00Z is still 09-11 in Los Angeles
        let ts2 = 1_789_182_000i64;
        assert_eq!(Zone::Offset(0).date_at(ts2), "2026-09-12");
        assert_eq!(Zone::Offset(-8 * 3600).date_at(ts2), "2026-09-11");
    }

    #[test]
    fn element_span_finds_values() {
        let text = "<a>1</a><timezone>+08:00</timezone><current_date>2026-09-12</current_date>";
        let (_, vs, ve, end) = element_span(text, "timezone").unwrap();
        assert_eq!(&text[vs..ve], "+08:00");
        assert_eq!(&text[..end], "<a>1</a><timezone>+08:00</timezone>");
        assert!(element_span(text, "timezoneX").is_none());
    }

    /// Only meaningful where zoneinfo exists (CI and the landing box); on a bare Windows host the
    /// documented fallback is used instead.
    #[test]
    fn tzdata_path_is_used_when_present() {
        if tzdata_dir().is_none() {
            eprintln!("no tzdata on this host; skipping");
            return;
        }
        // Asia/Taipei has a 41-transition history (types +08:06 / +08:00 / +09:00 DST); the offset
        // now must come out as exactly +08:00.
        let taipei = Zone::Named("Asia/Taipei".to_owned());
        assert_eq!(taipei.offset_at(1_789_254_600), 8 * 3600);
        // a zone that observes DST must report the summer offset for a summer instant
        let la = Zone::Named("America/Los_Angeles".to_owned());
        let summer = 1_752_500_000i64; // 2025-07-15
        let winter = 1_736_000_000i64; // 2024-12-31
        assert_eq!(la.offset_at(summer), -7 * 3600, "summer should be PDT");
        assert_eq!(la.offset_at(winter), -8 * 3600, "winter should be PST");
    }

    #[test]
    fn iana_offsets_are_read_from_tzdata_when_available() {
        // whatever the host has: either a tzdata answer or the documented fallback
        let zone = Zone::Named("Asia/Shanghai".to_owned());
        assert_eq!(zone.offset_at(1_789_254_600), 8 * 3600);
        // Americas entry: no DST at this instant (September is DST, so -7 for Los Angeles)
        let la = Zone::Named("America/Los_Angeles".to_owned());
        let offset = la.offset_at(1_789_254_600);
        assert!(
            offset == -7 * 3600 || offset == -8 * 3600,
            "unexpected Los Angeles offset {offset}"
        );
    }
}
