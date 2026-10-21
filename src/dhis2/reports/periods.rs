//! DHIS2 period codes and `--range` date ranges: the kind of a period, so
//! that only periods of one kind share a request, and the code and label a
//! range takes in place of a period.

use crate::error::{PipesError, Result};

/// A `--range`: two ISO dates, both included. DHIS2 is asked for it with
/// `startDate` and `endDate` and answers one value per indicator and org
/// unit. Its code, `20260405-20260520`, takes the place of a period code in
/// report ids; the `dhis2-period` extension reads `2026-04-05/2026-05-20`
/// (an ISO 8601 interval). No DHIS2 period code has that shape.
#[derive(Debug, Clone, PartialEq)]
pub struct DateRange {
    pub start: String,
    pub end: String,
}

impl DateRange {
    pub fn parse(s: &str) -> Result<Self> {
        let bad = |why: &str| {
            PipesError::Usage(format!(
                "--range {s:?}: {why}; expected START:END as ISO dates, e.g. 2026-04-05:2026-05-20"
            ))
        };
        let (start, end) = s.split_once(':').ok_or_else(|| bad("no colon"))?;
        for d in [start, end] {
            if !crate::calendar::is_date(d) {
                return Err(bad(&format!("{d:?} is not a date")));
            }
        }
        if start > end {
            return Err(bad("START is after END"));
        }
        Ok(Self {
            start: start.into(),
            end: end.into(),
        })
    }

    pub fn code(&self) -> String {
        format!(
            "{}-{}",
            self.start.replace('-', ""),
            self.end.replace('-', "")
        )
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.start, self.end)
    }

    /// The range a report id's period code stands for, if it is one.
    pub fn from_code(code: &str) -> Option<Self> {
        let (a, b) = code.split_once('-')?;
        let date = |d: &str| {
            (d.len() == 8 && d.bytes().all(|c| c.is_ascii_digit()))
                .then(|| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]))
        };
        Some(Self {
            start: date(a)?,
            end: date(b)?,
        })
    }
}

/// The `dhis2-period` extension's value for a row's period code: the code,
/// or a range's label.
pub fn period_value(pe: &str) -> String {
    match DateRange::from_code(pe) {
        Some(range) => range.label(),
        None => pe.to_string(),
    }
}

/// The shape of a DHIS2 period code, so that only periods of one kind share
/// a request. The leading number keeps its length and every later number
/// becomes `#`: `20250101` is `8` (a day), `202501` is `6` (a month),
/// `2025W5` and `2025W12` are both `4W#`, `2025Q1` is `4Q#`. A relative
/// period (`LAST_12_MONTHS`) starts with a letter and has no kind: DHIS2
/// expands it to several periods, so it is asked for alone.
pub fn period_kind(pe: &str) -> Option<String> {
    let lead = pe.bytes().take_while(u8::is_ascii_digit).count();
    if lead == 0 {
        return None;
    }
    let mut kind = lead.to_string();
    let mut in_number = false;
    for c in pe[lead..].chars() {
        if c.is_ascii_digit() {
            if !in_number {
                kind.push('#');
            }
            in_number = true;
        } else {
            kind.push(c);
            in_number = false;
        }
    }
    Some(kind)
}

/// Periods grouped by kind, each kind where it first appears; a relative
/// period is a group of its own.
pub(super) fn period_groups(periods: &[String]) -> Vec<Vec<String>> {
    let mut groups: Vec<(Option<String>, Vec<String>)> = Vec::new();
    for p in periods {
        let kind = period_kind(p);
        match groups
            .iter_mut()
            .find(|(k, _)| kind.is_some() && *k == kind)
        {
            Some((_, list)) => list.push(p.clone()),
            None => groups.push((kind, vec![p.clone()])),
        }
    }
    groups.into_iter().map(|(_, list)| list).collect()
}

/// `2025`, or `20250421…20250427 (7 periods)`.
pub(super) fn periods_label(periods: &[String]) -> String {
    match periods {
        [one] => one.clone(),
        [first, .., last] => format!("{first}…{last} ({} periods)", periods.len()),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_of_one_kind_share_a_shape() {
        let kind = |p: &str| period_kind(p);
        assert_eq!(kind("20250101").as_deref(), Some("8"));
        assert_eq!(kind("202501").as_deref(), Some("6"));
        assert_eq!(kind("2025").as_deref(), Some("4"));
        assert_eq!(kind("2025W5"), kind("2025W12"));
        assert_eq!(kind("2025Q1"), kind("2024Q4"));
        assert_ne!(kind("2025W5"), kind("2025BiW5"));
        assert_ne!(kind("2025Q1"), kind("2025S1"));
        assert_ne!(kind("202501"), kind("202501B"));
        assert_eq!(kind("2025April"), kind("2024April"));
        assert_eq!(kind("LAST_12_MONTHS"), None);
    }

    #[test]
    fn a_range_is_two_real_dates_in_order() {
        let r = DateRange::parse("2026-04-05:2026-05-20").unwrap();
        assert_eq!(r.code(), "20260405-20260520");
        assert_eq!(r.label(), "2026-04-05/2026-05-20");
        assert_eq!(DateRange::from_code(&r.code()), Some(r));
        assert!(DateRange::parse("2026-04-05:2026-04-05").is_ok());
        assert!(DateRange::parse("2024-02-29:2024-03-01").is_ok());
        for bad in [
            "2026-05-20:2026-04-05",
            "2026-04-05",
            "2026-04-05/2026-05-20",
            "2025-02-29:2025-03-01",
            "2026-04-31:2026-05-01",
            "2026-13-01:2026-12-01",
            "2026-4-5:2026-5-20",
            "20260405:20260520",
        ] {
            assert!(DateRange::parse(bad).is_err(), "{bad}");
        }
        // Period codes are never read as ranges.
        for pe in [
            "2025",
            "20250101",
            "2025W5",
            "LAST_12_MONTHS",
            "2025AprilS1",
        ] {
            assert_eq!(DateRange::from_code(pe), None, "{pe}");
        }
    }
}
