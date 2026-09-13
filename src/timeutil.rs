//! Small UTC time helpers — no chrono dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since the Unix epoch -> (year, month, day): Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `YYMMDD-HHMMSS` in UTC — fixed width, so lexical order == chronological order.
pub fn stamp() -> String {
    let secs = (now_ms() / 1000) as i64;
    let (y, m, d) = civil_from_days(secs / 86_400);
    let sod = secs.rem_euclid(86_400);
    format!(
        "{:02}{:02}{:02}-{:02}{:02}{:02}",
        y % 100,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn stamp_shape_is_sortable() {
        let s = stamp();
        assert_eq!(s.len(), 13, "{s}");
        assert_eq!(&s[6..7], "-");
        let a = civil_from_days(20_000);
        assert_eq!(a.2, 4);
    }
}
