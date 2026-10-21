//! The units of work of a run, in the order their reports are written, each
//! sized from its org unit count to stay under `--max-rows`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::cli::{Dhis2ReportsArgs, ReportSource};
use crate::dhis2::api::{Dhis2, under};
use crate::dhis2::measures::Indicator;
use crate::dhis2::raw::{RawSource, Span, load_parents, prepare};
use crate::error::{PipesError, Result};

use super::periods::{DateRange, period_groups, periods_label};
use super::unique;

/// One unit of work: a chunk of indicators for a batch of periods, or for
/// one date range, at one level (from analytics) or at every requested
/// level (from data values, where one download serves them all). Its
/// totals and every breakdown are fetched together, so its reports are
/// complete, and written, as soon as it is done.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub levels: Vec<u32>,
    /// Empty for a date range.
    pub periods: Vec<String>,
    pub range: Option<DateRange>,
    pub indicators: Vec<String>,
}

impl Unit {
    /// `2025`, `20250421…20250427 (7 periods)`, or `2026-04-05/2026-05-20`.
    pub(super) fn when(&self) -> String {
        match &self.range {
            Some(r) => r.label(),
            None => periods_label(&self.periods),
        }
    }

    /// `level 4`, `levels 1, 2, 3`.
    pub(super) fn where_(&self) -> String {
        let list: Vec<String> = self.levels.iter().map(u32::to_string).collect();
        match list.len() {
            1 => format!("level {}", list[0]),
            _ => format!("levels {}", list.join(", ")),
        }
    }
}

/// Every unit of work, in the order its reports are written: level by
/// level, then indicator chunk, then period batch, then each date range.
/// `levels` pairs each level with its org unit count. A batch holds as many
/// periods of one kind as keep indicators × periods × org units within
/// `max_rows`, and at least one. Breakdowns are not counted: they come back
/// as separate requests.
pub fn plan(
    levels: &[(u32, u64)],
    periods: &[String],
    ranges: &[DateRange],
    ids: &[String],
    chunk: usize,
    max_rows: usize,
) -> Vec<Unit> {
    let groups = period_groups(periods);
    levels
        .iter()
        .flat_map(|&(level, org_units)| {
            batches(&[level], &groups, ranges, ids, chunk, &|indicators| {
                fit(max_rows, indicators.len() as u64 * org_units.max(1))
            })
        })
        .collect()
}

/// Units for `--source data-values`: every level at once, by indicator
/// chunk, then period batch, then range. A batch holds as many periods of
/// one kind as keep data elements × periods × `org_units` (every org unit
/// data can sit at) within `max_rows`; `elements` counts a chunk's data
/// elements.
#[allow(clippy::too_many_arguments)]
pub fn plan_all_levels(
    levels: &[u32],
    org_units: u64,
    periods: &[String],
    ranges: &[DateRange],
    ids: &[String],
    chunk: usize,
    max_rows: usize,
    elements: &dyn Fn(&[String]) -> usize,
) -> Vec<Unit> {
    let groups = period_groups(periods);
    batches(levels, &groups, ranges, ids, chunk, &|indicators| {
        fit(
            max_rows,
            elements(indicators).max(1) as u64 * org_units.max(1),
        )
    })
}

/// The units at `levels`: by indicator chunk, then period batch (each
/// group cut into batches of `per_batch` periods for the chunk), then range.
fn batches(
    levels: &[u32],
    groups: &[Vec<String>],
    ranges: &[DateRange],
    ids: &[String],
    chunk: usize,
    per_batch: &dyn Fn(&[String]) -> usize,
) -> Vec<Unit> {
    let mut units = Vec::new();
    for indicators in ids.chunks(chunk) {
        let per_batch = per_batch(indicators);
        let unit = |periods: Vec<String>, range: Option<DateRange>| Unit {
            levels: levels.to_vec(),
            periods,
            range,
            indicators: indicators.to_vec(),
        };
        for group in groups {
            for batch in group.chunks(per_batch) {
                units.push(unit(batch.to_vec(), None));
            }
        }
        for range in ranges {
            units.push(unit(Vec::new(), Some(range.clone())));
        }
    }
    units
}

/// How many periods of `per_period` rows fit in `max_rows`; at least one.
fn fit(max_rows: usize, per_period: u64) -> usize {
    (max_rows as u64 / per_period).max(1) as usize
}

/// Every org unit DHIS2 has at `level` (under `root`).
fn org_unit_ids(api: &Dhis2, level: u32, root: Option<&str>) -> Result<Vec<String>> {
    let mut query = vec![
        ("filter", format!("level:eq:{level}")),
        ("fields", "id".into()),
    ];
    if let Some(root) = root {
        query.push(("filter", under(root)));
    }
    Ok(api
        .paged("organisationUnits.json", "organisationUnits", &query)?
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
        .collect())
}

/// How many org units DHIS2 has at `level` (under `root`), to size requests.
fn org_unit_count(api: &Dhis2, level: u32, root: Option<&str>) -> Result<u64> {
    let mut query = vec![
        ("filter", format!("level:eq:{level}")),
        ("fields", "id".into()),
        ("pageSize", "1".into()),
    ];
    if let Some(root) = root {
        query.push(("filter", under(root)));
    }
    api.get("organisationUnits.json", &query)?
        .pointer("/pager/total")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            PipesError::Environment(format!(
                "organisationUnits.json: no pager.total in the count of level {level}"
            ))
        })
}

/// The batches of a run and what decided them.
pub(super) struct Planned {
    /// `--period`, less those with no updated values under `--updated-since`.
    pub(super) periods: Vec<String>,
    /// `--range`, likewise.
    pub(super) ranges: Vec<DateRange>,
    /// From data values: the data elements and org unit tree to compute with.
    pub(super) raw: Option<RawSource>,
    /// Each `--level` with its org unit count.
    pub(super) levels: Vec<(u32, u64)>,
    pub(super) units: Vec<Unit>,
}

/// Plans the batches. From data values each level's org units are read (and
/// with `--updated-since` only the periods with updated values are kept);
/// from analytics requests are sized from each level's org unit count, so
/// several periods can share one.
pub(super) fn plan_run(
    args: &Dhis2ReportsArgs,
    api: &Dhis2,
    indicators: &BTreeMap<String, Indicator>,
    ids: &[String],
    system: &str,
    ranges: Vec<DateRange>,
) -> Result<Planned> {
    let mut periods = unique(&args.periods);
    let mut ranges = ranges;
    let mut raw: Option<RawSource> = None;
    let mut levels: Vec<(u32, u64)> = Vec::new();
    let units = if args.source == ReportSource::DataValues {
        if let Some(p) = periods.iter().find(|p| Span::of_period(p).is_none()) {
            return Err(PipesError::Usage(format!(
                "--period {p}: --source data-values knows the dates of days, ISO weeks, months, bi-months, quarters, six-months and years only (no relative periods); use --range for other spans"
            )));
        }
        let mut src = prepare(api, indicators, &args.breakdowns)?;
        let mut by_level = Vec::new();
        for level in unique(&args.levels) {
            let ous = org_unit_ids(api, level, args.root.as_deref())?;
            eprintln!("level {level}: {} org units", ous.len());
            levels.push((level, ous.len() as u64));
            by_level.push((level, ous));
        }
        let parents = load_parents(&args.locations, system)?;
        src.place(&parents, &by_level, args.root.as_deref());
        if let Some(since) = &args.updated_since {
            let spans: Vec<Span> = periods
                .iter()
                .filter_map(|p| Span::of_period(p))
                .chain(ranges.iter().filter_map(Span::of_range))
                .collect();
            let changed = src.changed(api, &src.elements_of(ids), &spans, since)?;
            let before = periods.len() + ranges.len();
            periods.retain(|p| changed.contains(p));
            ranges.retain(|r| changed.contains(&r.code()));
            eprintln!(
                "{} of {before} periods and ranges have data values updated since {since}",
                periods.len() + ranges.len()
            );
        }
        let units = plan_all_levels(
            &unique(&args.levels),
            parents.len() as u64,
            &periods,
            &ranges,
            ids,
            args.chunk,
            args.max_rows,
            &|chunk| src.elements_of(chunk).len(),
        );
        eprintln!("{} batches, one dataValueSets request each", units.len());
        raw = Some(src);
        units
    } else {
        for level in unique(&args.levels) {
            let n = org_unit_count(api, level, args.root.as_deref())?;
            eprintln!("level {level}: {n} org units");
            levels.push((level, n));
        }
        let units = plan(&levels, &periods, &ranges, ids, args.chunk, args.max_rows);
        eprintln!(
            "{} batches, {} analytics requests unless DHIS2 calls some too large",
            units.len(),
            units.len() * (1 + args.breakdowns.len())
        );
        units
    };
    Ok(Planned {
        periods,
        ranges,
        raw,
        levels,
        units,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn codes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn batches_stay_under_the_row_budget_and_never_mix_kinds() {
        let periods = codes(&[
            "20250101",
            "2025W1",
            "20250102",
            "20250103",
            "LAST_12_MONTHS",
            "2025W2",
            "202501",
            "THIS_YEAR",
        ]);
        let ids = codes(&["a", "b", "c"]);
        // 2 items × 3 org units = 6 rows a period: 2 periods fit in 12.
        let units = plan(&[(2, 3)], &periods, &[], &ids, 2, 12);
        let batches: Vec<(Vec<String>, usize)> = units
            .iter()
            .map(|u| (u.periods.clone(), u.indicators.len()))
            .collect();
        let b = |p: &[&str], n: usize| (codes(p), n);
        assert_eq!(
            batches,
            [
                b(&["20250101", "20250102"], 2),
                b(&["20250103"], 2),
                b(&["2025W1", "2025W2"], 2),
                b(&["LAST_12_MONTHS"], 2),
                b(&["202501"], 2),
                b(&["THIS_YEAR"], 2),
                // The last chunk has 1 item, so 4 periods would fit.
                b(&["20250101", "20250102", "20250103"], 1),
                b(&["2025W1", "2025W2"], 1),
                b(&["LAST_12_MONTHS"], 1),
                b(&["202501"], 1),
                b(&["THIS_YEAR"], 1),
            ]
        );
        // A level too large for one period still gets one per request.
        let units = plan(&[(4, 100_000)], &periods[..2], &[], &ids, 10, 20_000);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn a_large_export_needs_hundreds_of_batches_not_thousands() {
        // 58 items, 46 days, 31 weeks, 7 months, levels of 1, 9, 112 and 912
        // org units, chunks of 10, the default budget.
        let ids: Vec<String> = (0..58).map(|i| format!("i{i:02}")).collect();
        let mut periods: Vec<String> = (1..=46).map(|d| format!("2026{:04}", 100 + d)).collect();
        periods.extend((1..=31).map(|w| format!("2026W{w}")));
        periods.extend((1..=7).map(|m| format!("2026{m:02}")));
        let units = plan(
            &[(1, 1), (2, 9), (3, 112), (4, 912)],
            &periods,
            &[],
            &ids,
            10,
            20_000,
        );
        let per_level = |l: u32| units.iter().filter(|u| u.levels == [l]).count();
        assert_eq!(
            [per_level(1), per_level(2), per_level(3), per_level(4)],
            [18, 18, 36, 258]
        );
        assert_eq!(units.len(), 330);
    }

    #[test]
    fn from_data_values_a_batch_counts_data_elements_and_every_level_shares_it() {
        let periods = codes(&["202501", "202502", "202503", "2025Q1", "2025Q2", "202504"]);
        let ranges = [DateRange::parse("2026-04-05:2026-05-20").unwrap()];
        let ids = codes(&["a", "b", "c"]);
        // "a" uses 3 data elements, the others 1: a chunk's elements add up.
        let elements = |chunk: &[String]| chunk.iter().map(|i| if i == "a" { 3 } else { 1 }).sum();
        // [a, b]: 4 elements × 5 org units = 20 rows a period, 2 fit in 40;
        // [c]: 1 × 5 = 5, so 8 would fit.
        let units = plan_all_levels(&[2, 3], 5, &periods, &ranges, &ids, 2, 40, &elements);
        let batches: Vec<(Vec<u32>, Vec<String>, String, usize)> = units
            .iter()
            .map(|u| {
                (
                    u.levels.clone(),
                    u.periods.clone(),
                    u.when(),
                    u.indicators.len(),
                )
            })
            .collect();
        let b = |p: &[&str], when: &str, n: usize| (vec![2, 3], codes(p), when.to_string(), n);
        assert_eq!(
            batches,
            [
                b(&["202501", "202502"], "202501…202502 (2 periods)", 2),
                b(&["202503", "202504"], "202503…202504 (2 periods)", 2),
                b(&["2025Q1", "2025Q2"], "2025Q1…2025Q2 (2 periods)", 2),
                b(&[], "2026-04-05/2026-05-20", 2),
                b(
                    &["202501", "202502", "202503", "202504"],
                    "202501…202504 (4 periods)",
                    1
                ),
                b(&["2025Q1", "2025Q2"], "2025Q1…2025Q2 (2 periods)", 1),
                b(&[], "2026-04-05/2026-05-20", 1),
            ]
        );
        // No data elements and no org units still give one period a batch
        // at most the budget.
        let units = plan_all_levels(&[1], 0, &periods[..2], &[], &ids[..1], 1, 1, &|_| 0);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn ranges_come_after_period_batches_one_unit_each() {
        let ranges = [
            DateRange::parse("2026-04-05:2026-05-20").unwrap(),
            DateRange::parse("2026-06-01:2026-06-30").unwrap(),
        ];
        let units = plan(
            &[(2, 3)],
            &codes(&["202601", "202602"]),
            &ranges,
            &codes(&["a", "b", "c"]),
            2,
            20_000,
        );
        let when: Vec<String> = units.iter().map(Unit::when).collect();
        assert_eq!(
            when,
            [
                "202601…202602 (2 periods)",
                "2026-04-05/2026-05-20",
                "2026-06-01/2026-06-30",
                "202601…202602 (2 periods)",
                "2026-04-05/2026-05-20",
                "2026-06-01/2026-06-30",
            ]
        );
        assert!(units[1].periods.is_empty());
    }
}
