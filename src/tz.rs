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
                // Windows has no zoneinfo files: ask the OS (DST aware) for known zone names.
                if let Some(id) = iana_to_windows_tz(name) {
                    if let Some(seconds) = windows_offset_at(id, utc_secs) {
                        if std::env::var_os("CODEX_REC_TZ_DEBUG").is_some() {
                            eprintln!(
                                "[timezone] {name} -> {:+03}:{:02} (windows api, id {id})",
                                seconds / 3600,
                                (seconds.abs() % 3600) / 60
                            );
                        }
                        return seconds;
                    }
                }
                if let Some(seconds) = tzfile_offset(name, utc_secs) {
                    if std::env::var_os("CODEX_REC_TZ_DEBUG").is_some() {
                        eprintln!("[timezone] {name} -> {:+03}:{:02} (tzdata file)", seconds / 3600, (seconds.abs() % 3600) / 60);
                    }
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

    /// The newest transition time in the zone's tzdata (used by tests to stay independent of the
    /// wall clock: a packaged tzfile only knows the future up to its last transition).
    #[cfg(test)]
    pub fn last_transition(&self) -> Option<i64> {
        let Zone::Named(name) = self else {
            return None;
        };
        let dir = tzdata_dir()?;
        let bytes = std::fs::read(dir.join(name)).ok()?;
        let (base, wide) = block_base(&bytes)?;
        let (_, _, _, timecnt, _, _) = tzif_counts(&bytes, base)?;
        if timecnt == 0 {
            return None;
        }
        let size = if wide { 8 } else { 4 };
        let at = base + 44 + (timecnt - 1) * size;
        Some(if wide {
            i64::from_be_bytes(bytes[at..at + 8].try_into().ok()?)
        } else {
            i32::from_be_bytes(bytes[at..at + 4].try_into().ok()?) as i64
        })
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

/// Finds the data block to read and whether its transitions are 64-bit.
///
/// A v2/v3/v4 file holds a 32-bit block followed by a 64-bit one; the 64-bit block is located by the
/// second `TZif` magic (computing the v1 length by hand is easy to get wrong, and the optional
/// isstd/isut arrays differ between distributions). A single-block v2+ file is read from offset 0.
fn block_base(bytes: &[u8]) -> Option<(usize, bool)> {
    if bytes.len() < 44 || &bytes[0..4] != b"TZif" {
        return None;
    }
    let version = *bytes.get(4)?;
    if version >= b'2' {
        match bytes[1..].windows(4).position(|w| w == b"TZif") {
            Some(rel) => Some((rel + 1, true)),
            None => Some((0, true)),
        }
    } else {
        Some((0, false))
    }
}

/// Reads the offset for an IANA name out of the system tzdata (TZif v1/v2/v3/v4).
fn tzfile_offset(name: &str, utc_secs: i64) -> Option<i32> {
    let dir = tzdata_dir()?;
    if name.contains("..") || name.starts_with('/') {
        return None;
    }
    let bytes = std::fs::read(dir.join(name)).ok()?;
    if bytes.len() < 44 || &bytes[0..4] != b"TZif" {
        return None;
    }
    let (base, wide) = block_base(&bytes)?;
    if bytes.len() < base + 44 {
        return None;
    }
    let (_isutcnt, _isstdcnt, _leapcnt, timecnt, typecnt, _charcnt) = tzif_counts(&bytes, base)?;

    // transition times are 8-byte in v2+ blocks, 4-byte in the 32-bit v1 block
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
/// Maps a Windows timezone id (`tzutil` / `Get-TimeZone`) to an IANA name, so a Windows host keeps
/// its DST rules instead of collapsing to a fixed offset. Only the common ids are listed; anything
/// unknown falls back to the machine's current base offset.
fn windows_tz_to_iana(id: &str) -> Option<&'static str> {
    Some(match id.trim() {
        "UTC" | "Dateline Standard Time" => "Etc/UTC",
        "Pacific Standard Time" => "America/Los_Angeles",
        "Mountain Standard Time" => "America/Denver",
        "US Mountain Standard Time" => "America/Phoenix",
        "Central Standard Time" => "America/Chicago",
        "Eastern Standard Time" => "America/New_York",
        "Atlantic Standard Time" => "America/Halifax",
        "E. South America Standard Time" => "America/Sao_Paulo",
        "GMT Standard Time" => "Europe/London",
        "W. Europe Standard Time" => "Europe/Berlin",
        "Central Europe Standard Time" => "Europe/Budapest",
        "Central European Standard Time" => "Europe/Warsaw",
        "Romance Standard Time" => "Europe/Paris",
        "E. Europe Standard Time" => "Europe/Chisinau",
        "Russian Standard Time" => "Europe/Moscow",
        "Turkey Standard Time" => "Europe/Istanbul",
        "Israel Standard Time" => "Asia/Jerusalem",
        "Arab Standard Time" => "Asia/Riyadh",
        "India Standard Time" => "Asia/Kolkata",
        "Bangladesh Standard Time" => "Asia/Dhaka",
        "SE Asia Standard Time" => "Asia/Bangkok",
        "China Standard Time" => "Asia/Shanghai",
        "Taipei Standard Time" => "Asia/Taipei",
        "Singapore Standard Time" => "Asia/Singapore",
        "W. Australia Standard Time" => "Australia/Perth",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "Korea Standard Time" => "Asia/Seoul",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        "New Zealand Standard Time" => "Pacific/Auckland",
        _ => return None,
    })
}

/// Windows timezone ids that correspond to the IANA names we accept, so an offset query can be made
/// through the OS instead of through tzdata (Windows ships no zoneinfo files). Mirrors
/// `windows_tz_to_iana` in the other direction.
fn iana_to_windows_tz(name: &str) -> Option<&'static str> {
    Some(match name {
        "Etc/UTC" | "UTC" | "GMT" => "UTC",
        "America/Los_Angeles" => "Pacific Standard Time",
        "America/Denver" => "Mountain Standard Time",
        "America/Phoenix" => "US Mountain Standard Time",
        "America/Chicago" => "Central Standard Time",
        "America/New_York" => "Eastern Standard Time",
        "America/Halifax" => "Atlantic Standard Time",
        "America/Sao_Paulo" => "E. South America Standard Time",
        "Europe/London" => "GMT Standard Time",
        "Europe/Berlin" => "W. Europe Standard Time",
        "Europe/Budapest" => "Central Europe Standard Time",
        "Europe/Warsaw" => "Central European Standard Time",
        "Europe/Paris" => "Romance Standard Time",
        "Europe/Chisinau" => "E. Europe Standard Time",
        "Europe/Moscow" => "Russian Standard Time",
        "Europe/Istanbul" => "Turkey Standard Time",
        "Asia/Jerusalem" => "Israel Standard Time",
        "Asia/Riyadh" => "Arab Standard Time",
        "Asia/Kolkata" => "India Standard Time",
        "Asia/Dhaka" => "Bangladesh Standard Time",
        "Asia/Bangkok" => "SE Asia Standard Time",
        "Asia/Shanghai" => "China Standard Time",
        "Asia/Taipei" => "Taipei Standard Time",
        "Asia/Singapore" => "Singapore Standard Time",
        "Asia/Tokyo" => "Tokyo Standard Time",
        "Asia/Seoul" => "Korea Standard Time",
        "Australia/Perth" => "W. Australia Standard Time",
        "Australia/Sydney" => "AUS Eastern Standard Time",
        "Pacific/Auckland" => "New Zealand Standard Time",
        _ => return None,
    })
}

/// Asks the OS for the offset of a Windows timezone id at a given instant (DST aware).
#[cfg(windows)]
fn windows_offset_at(id: &str, utc_secs: i64) -> Option<i32> {
    let script = format!(
        "$id='{id}'; $z=[System.TimeZoneInfo]::FindSystemTimeZoneById($id);          $t=[DateTimeOffset]::FromUnixTimeSeconds({utc_secs}).UtcDateTime;          Write-Output ([int]($z.GetUtcOffset($t).TotalSeconds))"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse::<i32>().ok()
}

#[cfg(not(windows))]
fn windows_offset_at(_id: &str, _utc_secs: i64) -> Option<i32> {
    None
}

/// Reads the Windows timezone (id + current base offset) by asking PowerShell.
#[cfg(windows)]
fn windows_zone() -> Option<Zone> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$tz=Get-TimeZone; Write-Output $tz.Id; Write-Output ([int]($tz.BaseUtcOffset.TotalSeconds))",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let id = lines.next()?;
    let base_offset = lines.next().and_then(|v| v.parse::<i32>().ok());
    if let Some(iana) = windows_tz_to_iana(id) {
        return Some(Zone::Named(iana.to_owned()));
    }
    base_offset.map(Zone::Offset)
}

#[cfg(not(windows))]
fn windows_zone() -> Option<Zone> {
    None
}

/// Reads the system zone name (`/etc/localtime` symlink or `/etc/timezone`).
pub fn from_system() -> Option<Zone> {
    if let Some(zone) = windows_zone() {
        return Some(zone);
    }
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
    fn windows_timezone_ids_map_to_iana_names() {
        assert_eq!(windows_tz_to_iana("China Standard Time"), Some("Asia/Shanghai"));
        assert_eq!(windows_tz_to_iana("Taipei Standard Time"), Some("Asia/Taipei"));
        assert_eq!(windows_tz_to_iana("Pacific Standard Time"), Some("America/Los_Angeles"));
        assert_eq!(windows_tz_to_iana("UTC"), Some("Etc/UTC"));
        assert_eq!(windows_tz_to_iana("Nowhere Standard Time"), None);
    }

    #[test]
    fn iana_names_map_back_to_windows_ids() {
        assert_eq!(iana_to_windows_tz("Asia/Shanghai"), Some("China Standard Time"));
        assert_eq!(iana_to_windows_tz("America/Los_Angeles"), Some("Pacific Standard Time"));
        assert_eq!(iana_to_windows_tz("Etc/UTC"), Some("UTC"));
        assert_eq!(iana_to_windows_tz("Mars/Phobos"), None);
        // and the two tables agree on the ids they share
        for (id, iana) in [
            ("China Standard Time", "Asia/Shanghai"),
            ("Pacific Standard Time", "America/Los_Angeles"),
            ("Taipei Standard Time", "Asia/Taipei"),
        ] {
            assert_eq!(windows_tz_to_iana(id), Some(iana));
            assert_eq!(iana_to_windows_tz(iana), Some(id));
        }
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
    ///
    /// The assertions are deliberately independent of how much future a given tzdata release carries:
    /// they only require the *relative* behaviour around a zone's own transitions. Absolute dates are
    /// verified end to end on the landing box against the system `date` command.
    #[test]
    fn tzdata_path_is_used_when_present() {
        let Some(dir) = tzdata_dir() else {
            eprintln!("no tzdata on this host; skipping");
            return;
        };
        // Asia/Taipei: its final transition (1980) left it on +08:00 for good.
        let taipei = Zone::Named("Asia/Taipei".to_owned());
        eprintln!(
            "tzdata dir {} | Asia/Taipei file {} bytes | last transition {:?}",
            dir.display(),
            std::fs::metadata(dir.join("Asia/Taipei")).map(|m| m.len()).unwrap_or(0),
            taipei.last_transition()
        );
        for ts in [0i64, 1_789_254_600, 2_500_000_000] {
            let off = taipei.offset_at(ts);
            eprintln!("  Asia/Taipei at {ts} -> {off} ({}h)", off / 3600);
            assert_eq!(off, 8 * 3600, "Asia/Taipei must be +08:00 at {ts}");
        }
        // America/Los_Angeles: a DST zone. Around its own latest transition the offset must switch
        // between PST (-8) and PDT (-7); which side comes first depends on the release, so compare as
        // a set.
        let la = Zone::Named("America/Los_Angeles".to_owned());
        let latest = la.last_transition().expect("Los Angeles has transitions");
        let before = la.offset_at(latest - 86_400 * 30);
        let after = la.offset_at(latest + 86_400 * 30);
        eprintln!("  America/Los_Angeles last transition {latest}: before={before} after={after}");
        let mut pair = [before, after];
        pair.sort();
        assert_eq!(
            pair,
            [-8 * 3600, -7 * 3600],
            "the offsets around the latest transition must be PST and PDT (got {before}/{after})"
        );
    }

    /// A synthetic TZif v2 file with one 64-bit block: proves the reader picks the second block and
    /// the 8-byte transitions (the previous bug read the 32-bit block's bytes instead).
    #[test]
    fn reader_handles_a_synthetic_v2_file() {
        // Build a v2 file: [v1 header + v1 data (1 transition, 1 type)] then [v2 header + v2 data].
        let mut out = Vec::new();
        let header = |version: u8, timecnt: u32, typecnt: u32| {
            let mut h = Vec::new();
            h.extend_from_slice(b"TZif");
            h.push(version);
            h.extend_from_slice(&[0u8; 15]);
            h.extend_from_slice(&0u32.to_be_bytes()); // isutcnt
            h.extend_from_slice(&0u32.to_be_bytes()); // isstdcnt
            h.extend_from_slice(&0u32.to_be_bytes()); // leapcnt
            h.extend_from_slice(&timecnt.to_be_bytes());
            h.extend_from_slice(&typecnt.to_be_bytes());
            h.extend_from_slice(&4u32.to_be_bytes()); // charcnt
            h
        };
        // v1 block: one transition at t=0 to type 0 (+00:00)
        out.extend(header(b'2', 1, 1));
        out.extend(0i32.to_be_bytes());
        out.push(0);
        out.extend(0i32.to_be_bytes()); // gmtoff 0
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"UTC ");
        // v2 block: one transition at t=0 to type 0 (+08:00)
        let v2_start = out.len();
        out.extend(header(b'2', 1, 1));
        out.extend(0i64.to_be_bytes());
        out.push(0);
        out.extend((8 * 3600i32).to_be_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"TPE ");

        let (base, wide) = block_base(&out).expect("v2 block located");
        assert_eq!(base, v2_start, "the second TZif magic must be used");
        assert!(wide, "v2 transitions are 64-bit");
        let (_, _, _, timecnt, typecnt, _) = tzif_counts(&out, base).unwrap();
        assert_eq!((timecnt, typecnt), (1, 1));
        let at = base + 44 + 8; // transition (8 bytes) -> type index
        let ty = at + 1;
        let off = i32::from_be_bytes(out[ty..ty + 4].try_into().unwrap());
        assert_eq!(off, 8 * 3600);
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
