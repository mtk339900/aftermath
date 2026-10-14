//! Minimal UTC timestamp formatting without an external time crate.
//!
//! Aftermath only needs RFC 3339 timestamps for report metadata; pulling a
//! full date-time library for that would be disproportionate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as RFC 3339 UTC (`2026-01-31T12:00:00Z`).
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_to_rfc3339(secs)
}

/// Convert a Unix timestamp to RFC 3339 UTC.
pub fn epoch_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch to (year, month, day) — Howard Hinnant's civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_timestamps() {
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-01-01T00:00:00Z == 1767225600
        assert_eq!(epoch_to_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
        // A leap-year boundary: 2024-02-29T00:00:00Z == 1709164800
        assert_eq!(epoch_to_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        // End of 2025: 2025-12-31T23:59:59Z == 1767225599
        assert_eq!(epoch_to_rfc3339(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn now_is_plausible() {
        let s = now_rfc3339();
        assert!(s.ends_with('Z') && s.len() == 20, "got {s}");
        let year: u32 = s[..4].parse().unwrap();
        assert!((2020..=2100).contains(&year), "year {year}");
    }
}
