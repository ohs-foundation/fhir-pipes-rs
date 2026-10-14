//! FHIR `instant` values: parse to (seconds, nanos) since the epoch for
//! comparison, and format the current time. No calendar crate: the civil
//! date arithmetic is Howard Hinnant's days-from-civil.

use std::time::{SystemTime, UNIX_EPOCH};

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (m as u64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDThh:mm:ss[.fraction](Z|±hh:mm)` -> (unix seconds, nanoseconds).
///
/// A seconds field of 60 (a leap second) is deliberately accepted: this
/// crate has no leap-second table, so it collapses into the following
/// second rather than being rejected.
pub fn parse_instant(s: &str) -> Option<(i64, u32)> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    // Every numeric field must be plain ASCII digits: `parse::<i64>` alone
    // would also accept a leading `+`/`-` sign, which has no business
    // inside a date/time component.
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let slice = s.get(r)?;
        if slice.is_empty() || !slice.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    let (y, mo, d) = (digits(0..4)?, digits(5..7)? as u32, digits(8..10)? as u32);
    let (h, mi, sec) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Reject days that don't exist in their month (e.g. 2026-02-31, or
    // 2026-02-29 in a non-leap year) by round-tripping through the
    // civil-calendar conversion.
    let days = days_from_civil(y, mo, d);
    if civil_from_days(days) != (y, mo, d) {
        return None;
    }
    let mut i = 19;
    let mut nanos = 0u32;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return None;
        }
        let frac_digits = &s[start..end];
        let scaled: String = format!("{frac_digits:0<9}").chars().take(9).collect();
        nanos = scaled.parse().ok()?;
        i = end;
    }
    let offset = match b.get(i) {
        Some(b'Z') if i + 1 == b.len() => 0i64,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = digits(i + 1..i + 3)?;
            let om = digits(i + 4..i + 6)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let secs = oh * 3600 + om * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    Some((days * 86400 + h * 3600 + mi * 60 + sec - offset, nanos))
}

/// The greater of a current best and a candidate; unparseable candidates lose.
pub fn later(current: Option<&str>, candidate: &str) -> Option<String> {
    let Some(c) = parse_instant(candidate) else {
        return current.map(str::to_string);
    };
    match current.and_then(|s| parse_instant(s).map(|p| (s, p))) {
        Some((s, p)) if p >= c => Some(s.to_string()),
        _ => Some(candidate.to_string()),
    }
}

/// Formats `t` as a FHIR `instant` in UTC. This is meant for "now"
/// timestamps only: a time before the Unix epoch is clamped to
/// `1970-01-01T00:00:00Z` rather than producing a negative date.
pub fn format_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fhir_instants_with_fraction_and_offset() {
        assert_eq!(parse_instant("2026-01-02T03:04:05Z"), Some((1767323045, 0)));
        assert_eq!(
            parse_instant("2026-01-02T03:04:05.123Z"),
            Some((1767323045, 123_000_000))
        );
        assert_eq!(
            parse_instant("2026-01-02T04:04:05+01:00"),
            Some((1767323045, 0))
        );
        assert_eq!(
            parse_instant("2026-01-02T03:04:05-05:00"),
            Some((1767341045, 0))
        );
        assert_eq!(parse_instant("2024-02-29T12:34:56Z"), Some((1709210096, 0)));
        assert_eq!(parse_instant("1969-07-20T20:17:40Z"), Some((-14182940, 0)));
        assert_eq!(
            parse_instant("2026-01-02T03:04:05.5Z"),
            Some((1767323045, 500_000_000))
        );
        assert_eq!(
            parse_instant("2026-01-02T03:04:05.123456789Z"),
            Some((1767323045, 123_456_789))
        );
        assert_eq!(parse_instant("2026-02-31T00:00:00Z"), None);
        assert_eq!(parse_instant("2026-01-01T00:00:00+99:00"), None);
        assert_eq!(parse_instant("2026-01-02"), None);
        assert_eq!(parse_instant("garbage"), None);
    }

    #[test]
    fn later_picks_the_greater_instant_and_ignores_unparseable() {
        assert_eq!(
            later(None, "2026-01-01T00:00:00Z"),
            Some("2026-01-01T00:00:00Z".to_string())
        );
        assert_eq!(
            later(Some("2026-01-01T00:00:00Z"), "2025-12-31T23:59:59Z"),
            Some("2026-01-01T00:00:00Z".to_string())
        );
        assert_eq!(
            later(Some("2026-01-01T00:00:00Z"), "2026-01-01T01:00:00+01:00"),
            Some("2026-01-01T00:00:00Z".to_string()),
            "equal instants keep the first"
        );
        assert_eq!(
            later(Some("2026-01-01T00:00:00Z"), "nope"),
            Some("2026-01-01T00:00:00Z".to_string())
        );
        assert_eq!(later(None, "nope"), None);
        assert_eq!(
            later(Some("2026-01-01T00:00:00.1Z"), "2026-01-01T00:00:00.9Z"),
            Some("2026-01-01T00:00:00.9Z".to_string()),
            "equal seconds: the greater nanos wins"
        );
    }

    #[test]
    fn formats_utc_now_as_an_instant() {
        let s = format_utc(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1767323045));
        assert_eq!(s, "2026-01-02T03:04:05Z");
        assert!(parse_instant(&format_utc(std::time::SystemTime::now())).is_some());
    }
}
