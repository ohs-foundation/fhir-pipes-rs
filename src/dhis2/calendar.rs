//! DHIS2 periods as dates, for `--source data-values`, where no analytics
//! response says when a period starts and ends. ISO calendar only:
//! days, ISO weeks, months, bi-months, quarters, six-months and years.
//! Other period types (BiWeekly, weeks starting Wednesday to Sunday,
//! financial years, relative periods) have no dates here. Days are counted
//! by `crate::calendar`.

use crate::calendar::{days_from_civil, month_days};

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
            days_from_civil(year, first, 1)?,
            days_from_civil(year, last, month_days(year, last)?)?,
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
            let day = days_from_civil(year, num(&r[0..2])?, num(&r[2..4])?)?;
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
            let jan4 = days_from_civil(year, 1, 4)?;
            let monday1 = jan4 - (jan4 + 3).rem_euclid(7);
            let next_jan4 = days_from_civil(year + 1, 1, 4)?;
            let weeks = (next_jan4 - (next_jan4 + 3).rem_euclid(7) - monday1) / 7;
            (1..=weeks).contains(&w).then_some(())?;
            let start = monday1 + (w - 1) * 7;
            Some((start, start + 6))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::iso;

    fn dates(pe: &str) -> Option<(String, String)> {
        period_days(pe).map(|(a, b)| (iso(a), iso(b)))
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
}
