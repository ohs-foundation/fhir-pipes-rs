//! Calendar dates as day counts since 1970-01-01, in the proleptic Gregorian
//! calendar, through chrono. FHIR instants (`snapshot::instant`), DHIS2
//! periods and timestamps (`dhis2::calendar`, `dhis2::api`) and the date
//! checks of `population` and `dhis2 reports` all count days with this.

use chrono::{Datelike, NaiveDate};

/// 1970-01-01 as chrono counts days, from 0001-01-01 as day 1.
const UNIX_EPOCH_DAY: i64 = 719_163;

/// Days since 1970-01-01 of a date, or `None` when there is no such date
/// (`2025-02-29`, month 13).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let date = NaiveDate::from_ymd_opt(
        i32::try_from(y).ok()?,
        u32::try_from(m).ok()?,
        u32::try_from(d).ok()?,
    )?;
    Some(i64::from(date.num_days_from_ce()) - UNIX_EPOCH_DAY)
}

/// (year, month, day) of a day count since 1970-01-01. A count outside
/// chrono's range of years (about ±262,000) is clamped to it.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let ce = (z + UNIX_EPOCH_DAY).clamp(
        i64::from(NaiveDate::MIN.num_days_from_ce()),
        i64::from(NaiveDate::MAX.num_days_from_ce()),
    );
    let date = NaiveDate::from_num_days_from_ce_opt(ce as i32).expect("clamped to the range");
    (
        i64::from(date.year()),
        i64::from(date.month()),
        i64::from(date.day()),
    )
}

/// The days in month `m` of year `y`; `None` for no such month.
pub fn month_days(y: i64, m: i64) -> Option<i64> {
    let first = days_from_civil(y, m, 1)?;
    let next = if m == 12 {
        days_from_civil(y + 1, 1, 1)?
    } else {
        days_from_civil(y, m + 1, 1)?
    };
    Some(next - first)
}

pub fn days_in_year(y: i64) -> i64 {
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    if leap { 366 } else { 365 }
}

/// A day count as `YYYY-MM-DD`.
pub fn iso(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD`, a real date, as a day count. Four-digit years only, every
/// field ASCII digits.
pub fn day_of(date: &str) -> Option<i64> {
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = date.get(r)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?)
}

/// A real calendar date written `YYYY-MM-DD`.
pub fn is_date(s: &str) -> bool {
    day_of(s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_counts_round_trip() {
        for d in [-1000, 0, 19_000, 20_739, 100_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), Some(d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(iso(0), "1970-01-01");
        assert_eq!(days_from_civil(2025, 2, 29), None);
        assert_eq!(days_from_civil(2025, 13, 1), None);
    }

    #[test]
    fn months_and_years_have_their_days() {
        assert_eq!(month_days(2024, 2), Some(29));
        assert_eq!(month_days(2025, 2), Some(28));
        assert_eq!(month_days(2100, 2), Some(28));
        assert_eq!(month_days(2000, 2), Some(29));
        assert_eq!(month_days(2025, 12), Some(31));
        assert_eq!(month_days(2025, 4), Some(30));
        assert_eq!(month_days(2025, 13), None);
        assert_eq!(days_in_year(2024), 366);
        assert_eq!(days_in_year(1900), 365);
    }

    #[test]
    fn only_real_dates_are_dates() {
        assert_eq!(day_of("2024-02-29"), days_from_civil(2024, 2, 29));
        assert!(is_date("2026-01-01"));
        for bad in [
            "2025-02-29",
            "2025-13-45",
            "2025-00-10",
            "2025-01-00",
            "2026-1-1",
            "20260101",
            "2026-01-01T00:00:00Z",
            "+026-01-01",
            "2026-+1-01",
            "",
        ] {
            assert!(!is_date(bad), "{bad}");
        }
    }
}
