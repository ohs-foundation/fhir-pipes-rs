//! DHIS2 periods as dates, for `--source data-values`, where no analytics
//! response says when a period starts and ends. ISO calendar only:
//! days, ISO weeks, months, bi-months, quarters, six-months and years.
//! Other period types (BiWeekly, weeks starting Wednesday to Sunday,
//! financial years, relative periods) have no dates here.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn month_days(y: i64, m: i64) -> i64 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

pub fn days_in_year(y: i64) -> i64 {
    if leap(y) {
        366
    } else {
        365
    }
}

/// A day count as `YYYY-MM-DD`.
pub fn iso(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD` as a day count.
pub fn day_of(date: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| date.get(r)?.parse::<i64>().ok();
    if date.len() != 10 {
        return None;
    }
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    ((1..=12).contains(&m) && (1..=month_days(y, m)).contains(&d)).then(|| days_from_civil(y, m, d))
}

/// First and last day (inclusive, as day counts) of a DHIS2 period code,
/// or `None` for a period type this module does not know.
pub fn period_days(pe: &str) -> Option<(i64, i64)> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let num = |s: &str| s.parse::<i64>().ok();
    let year = num(pe.get(0..4)?).filter(|_| digits(&pe[0..4]))?;
    let rest = &pe[4..];
    let months = |first: i64, n: i64| {
        let last = first + n - 1;
        Some((
            days_from_civil(year, first, 1),
            days_from_civil(year, last, month_days(year, last)),
        ))
    };
    match rest {
        "" => months(1, 12),
        r if r.len() == 2 && digits(r) => {
            let m = num(r)?;
            (1..=12).contains(&m).then_some(())?;
            months(m, 1)
        }
        r if r.len() == 4 && digits(r) => {
            let (m, d) = (num(&r[0..2])?, num(&r[2..4])?);
            ((1..=12).contains(&m) && (1..=month_days(year, m)).contains(&d)).then_some(())?;
            let day = days_from_civil(year, m, d);
            Some((day, day))
        }
        r if r.len() == 3 && r.ends_with('B') && digits(&r[0..2]) => {
            let b = num(&r[0..2])?;
            (1..=6).contains(&b).then_some(())?;
            months(2 * b - 1, 2)
        }
        r if r.len() == 2 && r.starts_with('Q') && digits(&r[1..]) => {
            let q = num(&r[1..])?;
            (1..=4).contains(&q).then_some(())?;
            months(3 * q - 2, 3)
        }
        r if r.len() == 2 && r.starts_with('S') && digits(&r[1..]) => {
            let s = num(&r[1..])?;
            (1..=2).contains(&s).then_some(())?;
            months(6 * s - 5, 6)
        }
        r if r.starts_with('W') && digits(&r[1..]) => {
            // ISO week: Monday to Sunday; week 1 holds 4 January.
            let w = num(&r[1..])?;
            let jan4 = days_from_civil(year, 1, 4);
            let monday1 = jan4 - (jan4 + 3).rem_euclid(7);
            let next_jan4 = days_from_civil(year + 1, 1, 4);
            let weeks = (next_jan4 - (next_jan4 + 3).rem_euclid(7) - monday1) / 7;
            (1..=weeks).contains(&w).then_some(())?;
            let start = monday1 + (w - 1) * 7;
            Some((start, start + 6))
        }
        _ => None,
    }
}

/// The current UTC time as a FHIR dateTime, to the second.
pub fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (h, rem) = (secs.rem_euclid(86_400) / 3600, secs.rem_euclid(3600));
    format!(
        "{}T{h:02}:{:02}:{:02}Z",
        iso(secs.div_euclid(86_400)),
        rem / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dates(pe: &str) -> Option<(String, String)> {
        period_days(pe).map(|(a, b)| (iso(a), iso(b)))
    }

    #[test]
    fn day_counts_round_trip() {
        for d in [-1000, 0, 19_000, 20_739, 100_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(iso(0), "1970-01-01");
        assert_eq!(day_of("2024-02-29"), Some(days_from_civil(2024, 2, 29)));
        assert_eq!(day_of("2025-02-29"), None);
    }

    #[test]
    fn every_iso_period_type_has_its_dates() {
        let d = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(dates("2025"), d("2025-01-01", "2025-12-31"));
        assert_eq!(dates("202502"), d("2025-02-01", "2025-02-28"));
        assert_eq!(dates("202402"), d("2024-02-01", "2024-02-29"));
        assert_eq!(dates("20250105"), d("2025-01-05", "2025-01-05"));
        assert_eq!(dates("202502B"), d("2025-03-01", "2025-04-30"));
        assert_eq!(dates("2025Q4"), d("2025-10-01", "2025-12-31"));
        assert_eq!(dates("2025S2"), d("2025-07-01", "2025-12-31"));
        // 2025-W01 starts Monday 2024-12-30; 2026 has 53 ISO weeks.
        assert_eq!(dates("2025W1"), d("2024-12-30", "2025-01-05"));
        assert_eq!(dates("2026W53"), d("2026-12-28", "2027-01-03"));
        assert_eq!(dates("2025W53"), None);
        for bad in [
            "LAST_12_MONTHS",
            "2025April",
            "2025BiW1",
            "2025WedW1",
            "202513",
            "20250230",
            "2025Q5",
        ] {
            assert_eq!(dates(bad), None, "{bad}");
        }
    }

    #[test]
    fn now_is_a_utc_datetime() {
        let n = now_utc();
        assert_eq!(n.len(), 20);
        assert!(n.ends_with('Z') && n.as_bytes()[10] == b'T');
    }
}
