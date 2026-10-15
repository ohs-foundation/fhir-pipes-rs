//! `fhir-pipes dhis2 reports`: indicator values from DHIS2 analytics as
//! MeasureReports, one per indicator, org unit and period.
//!
//! DHIS2 computes every value itself: it adds up the numerator and the
//! denominator for the org unit and period, then divides, so values are
//! requested at each level and never summed here. `includeNumDen` returns
//! the numerator and denominator beside the value; `skipRounding` returns
//! the value unrounded. Period start and end dates come from the response's
//! own metadata, so every DHIS2 period type works without fhir-pipes knowing its
//! calendar rules. A `--range` of dates is asked for with `startDate` and
//! `endDate` instead, and gives one value for the whole range.
//!
//! Org units are mapped to Locations through the identifier that
//! `fhir-pipes dhis2 locations` writes. A row whose org unit has no Location is
//! reported and skipped.
//!
//! An export can run to millions of values, so the work is planned in
//! units (one level, one chunk of indicators, one batch of periods of the
//! same kind, sized from the level's org unit count to stay under
//! `--max-rows`). Workers fetch units in parallel; each unit's totals and
//! breakdowns come back together, and its reports are written as soon as
//! every unit before it has been, so memory holds a few units at a time and
//! the file is the same at any `--concurrency`. After each unit the file is
//! synced and a checkpoint saved, so `--resume` can continue an interrupted
//! run; `--previous` compares with an earlier run (see `rerun`).
//!
//! `--source data-values` computes the same tables from raw data values
//! instead (see `raw`), so everything after the fetch is shared.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::api::{Dhis2, ServerInfo, check_analytics_age};
use super::calendar::now_utc;
use super::measures::{Indicator, MEASURE_POPULATION_SYSTEM, Scoring, fetch_indicators, fhir_key};
use super::raw::{RawSource, Span, load_parents, prepare};
use super::rerun::{
    Change, Checkpoint, Previous, Scope, file_stamp, plan_differences, write_lines,
};
use super::{Instance, NdjsonWriter, check_out};
use crate::cli::{Dhis2ReportsArgs, ReportSource};
use crate::error::{PipesError, Result};
use crate::extract::client::FetchError;
use crate::fhir::group::is_valid_id;
use crate::fhir::ndjson::NdjsonReader;
use crate::report::Report;
use crate::transform::write_report;

/// DHIS2 UIDs are 11 characters.
const UID_LEN: usize = 11;
/// FHIR `integer` is a signed 32-bit number.
const MAX_COUNT: f64 = i32::MAX as f64;

/// A Location an org unit maps to: its id, and its name for `subject.display`.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub id: String,
    pub name: Option<String>,
}

/// Org unit UID → Location, from every Location in `path` that carries an
/// identifier under `system`.
pub fn load_mapping(path: &Path, system: &str) -> Result<HashMap<String, Target>> {
    let mut out = HashMap::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let v: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        if v.get("resourceType").and_then(Value::as_str) != Some("Location") {
            continue;
        }
        let Some(id) = v.get("id").and_then(Value::as_str) else {
            continue;
        };
        for ident in v
            .get("identifier")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if ident.get("system").and_then(Value::as_str) == Some(system)
                && let Some(uid) = ident.get("value").and_then(Value::as_str)
            {
                out.insert(
                    uid.to_string(),
                    Target {
                        id: id.to_string(),
                        name: v.get("name").and_then(Value::as_str).map(str::to_string),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// One analytics response: rows by column name, and each period's dates.
#[derive(Debug, Default)]
pub struct Analytics {
    pub rows: Vec<AnalyticsRow>,
    /// Period code → (start, end) as `YYYY-MM-DD`.
    pub periods: HashMap<String, (String, String)>,
    /// Display name of every item in the response (dimensions and their options).
    pub names: HashMap<String, String>,
    /// Every org unit and period the request covered, data or not, as
    /// DHIS2 lists them in `metaData.dimensions` (relative periods expanded).
    pub org_units: Vec<String>,
    pub period_codes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnalyticsRow {
    pub dx: String,
    pub pe: String,
    pub ou: String,
    pub value: String,
    pub numerator: String,
    pub denominator: String,
    /// The breakdown option (category option, org unit group, …), when the
    /// request had a breakdown dimension.
    pub option: Option<String>,
}

/// A request by date range has no `pe` column; its rows get the range's code
/// from the caller.
pub fn parse_analytics(v: &Value, breakdown: Option<&str>) -> Result<Analytics> {
    let headers: Vec<&str> = v
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| PipesError::Environment("analytics response has no headers".into()))?
        .iter()
        .map(|h| h.get("name").and_then(Value::as_str).unwrap_or(""))
        .collect();
    let col = |name: &str| headers.iter().position(|h| *h == name);
    let need = |name: &str| {
        col(name).ok_or_else(|| {
            PipesError::Environment(format!("analytics response has no {name} column"))
        })
    };
    let (dx, pe, ou, value) = (need("dx")?, col("pe"), need("ou")?, need("value")?);
    let (num, den) = (col("numerator"), col("denominator"));
    let opt = match breakdown {
        Some(b) => Some(need(b)?),
        None => None,
    };
    let cell = |row: &[Value], i: Option<usize>| -> String {
        i.and_then(|i| row.get(i))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|r| AnalyticsRow {
            dx: cell(r, Some(dx)),
            pe: cell(r, pe),
            ou: cell(r, Some(ou)),
            value: cell(r, Some(value)),
            numerator: cell(r, num),
            denominator: cell(r, den),
            option: opt.map(|i| cell(r, Some(i))),
        })
        .collect();
    let mut periods = HashMap::new();
    let mut names = HashMap::new();
    if let Some(items) = v.pointer("/metaData/items").and_then(Value::as_object) {
        for (code, item) in items {
            if let Some(n) = item.get("name").and_then(Value::as_str) {
                names.insert(code.clone(), n.to_string());
            }
            let date = |k: &str| {
                item.get(k)
                    .and_then(Value::as_str)
                    .filter(|s| s.len() >= 10)
                    .map(|s| s[..10].to_string())
            };
            if let (Some(start), Some(end)) = (date("startDate"), date("endDate")) {
                periods.insert(code.clone(), (start, end));
            }
        }
    }
    let dimension = |d: &str| -> Vec<String> {
        v.pointer(&format!("/metaData/dimensions/{d}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    Ok(Analytics {
        rows,
        periods,
        names,
        org_units: dimension("ou"),
        period_codes: dimension("pe"),
    })
}

/// A DHIS2 count as a FHIR integer. Counts that are not whole numbers
/// (averages, constants, subtractions in the formula) are rounded and
/// reported; blank means DHIS2 gave none.
fn count(raw: &str, key: &str, what: &str, report: &mut Report) -> Option<i64> {
    if raw.trim().is_empty() {
        return None;
    }
    let Ok(x) = raw.trim().parse::<f64>() else {
        report.add(
            "bad_number",
            key,
            &format!("{what} {raw:?} is not a number; left out"),
        );
        return None;
    };
    if !x.is_finite() || x.abs() > MAX_COUNT {
        report.add(
            "bad_number",
            key,
            &format!("{what} {raw} does not fit a FHIR integer; left out"),
        );
        return None;
    }
    let r = x.round();
    if (x - r).abs() > 1e-6 {
        report.add(
            "fractional_count",
            key,
            &format!("{what} {raw} rounded to {r}"),
        );
    }
    Some(r as i64)
}

fn population(code: &str, display: &str, count: i64) -> Value {
    json!({
        "code": {"coding": [{"system": MEASURE_POPULATION_SYSTEM, "code": code, "display": display}]},
        "count": count,
    })
}

pub fn report_id(instance: &Instance, row: &AnalyticsRow) -> String {
    format!(
        "{}-{}-{}-{}",
        instance.name,
        fhir_key(&row.dx),
        row.ou,
        row.pe
    )
}

/// The MeasureReport for one analytics row, or `None` after reporting why
/// there is none.
#[allow(clippy::too_many_arguments)]
pub fn measure_report(
    row: &AnalyticsRow,
    ind: &Indicator,
    location: &Target,
    period: &(String, String),
    instance: &Instance,
    info: &ServerInfo,
    report: &mut Report,
) -> Option<Value> {
    let id = report_id(instance, row);
    if !is_valid_id(&id) {
        report.add("invalid_id", &id, "not a valid FHIR id; row skipped");
        return None;
    }
    let (populations, score) = figures(row, ind, &id, report)?;
    let mut group = json!({});
    if !populations.is_empty() {
        group["population"] = Value::Array(populations);
    }
    group["measureScore"] = json!({"value": score});
    let pe = period_value(&row.pe);
    let mut r = json!({
        "resourceType": "MeasureReport",
        "id": id,
        "meta": {"tag": [instance.tag()]},
        "extension": [{"url": instance.urls.period_extension, "valueString": pe}],
        "status": "complete",
        "type": "summary",
        "measure": instance.measure_url(&ind.fhir_key()),
        "subject": {"reference": format!("Location/{}", location.id)},
    });
    if let Some(name) = &location.name {
        r["subject"]["display"] = json!(name);
    }
    if let Some(d) = info
        .last_analytics
        .as_deref()
        .and_then(|t| info.timestamp(t))
    {
        r["date"] = json!(d);
    }
    if let Some(name) = &info.system_name {
        r["reporter"] = json!({"display": name});
    }
    r["period"] = json!({"start": period.0, "end": period.1});
    r["group"] = json!([group]);
    Some(r)
}

/// The populations and score of one analytics row (a whole report, or one
/// stratum of it); `None` after reporting why there are none. `key` names
/// the report in the run report.
pub fn figures(
    row: &AnalyticsRow,
    ind: &Indicator,
    id: &str,
    report: &mut Report,
) -> Option<(Vec<Value>, f64)> {
    let Ok(value) = row.value.trim().parse::<f64>() else {
        report.add(
            "bad_number",
            id,
            &format!("value {:?} is not a number; row skipped", row.value),
        );
        return None;
    };
    if !value.is_finite() {
        report.add("bad_number", id, "value is not finite; row skipped");
        return None;
    }
    let score = ((value / ind.factor) * 1e10).round() / 1e10;
    let populations: Vec<Value> = match ind.scoring() {
        // A data element row has no numerator column: its value is the count.
        Scoring::Cohort => count(
            if row.numerator.trim().is_empty() {
                &row.value
            } else {
                &row.numerator
            },
            id,
            "count",
            report,
        )
        .map(|n| population("initial-population", "Initial Population", n))
        .into_iter()
        .collect(),
        _ => [
            count(&row.numerator, id, "numerator", report)
                .map(|n| population("numerator", "Numerator", n)),
            count(&row.denominator, id, "denominator", report)
                .map(|n| population("denominator", "Denominator", n)),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    Some((populations, score))
}

/// One stratum: a breakdown option with its own populations and score.
pub fn stratum(
    row: &AnalyticsRow,
    ind: &Indicator,
    breakdown: &str,
    names: &HashMap<String, String>,
    instance: &Instance,
    report: &mut Report,
) -> Option<Value> {
    let option = row.option.as_deref()?;
    let key = format!("{}/{breakdown}:{option}", report_id(instance, row));
    let (populations, score) = figures(row, ind, &key, report)?;
    let name = names.get(option).map(String::as_str).unwrap_or(option);
    let mut s = json!({
        "value": {"coding": [{"system": instance.dimension_system(Some(breakdown)), "code": option, "display": name}], "text": name},
    });
    if !populations.is_empty() {
        s["population"] = Value::Array(populations);
    }
    s["measureScore"] = json!({"value": score});
    Some(s)
}

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
            if !is_date(d) {
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

/// A real calendar date written `YYYY-MM-DD`.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    let shaped = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !shaped {
        return false;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u32>().unwrap_or(0);
    let (y, m, d) = (n(0..4), n(5..7), n(8..10));
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

/// A `--campaign` reference, `ResourceType/id`.
fn check_campaign(reference: &str) -> Result<()> {
    let ok = match reference.split_once('/') {
        Some((kind, id)) => {
            kind.starts_with(|c: char| c.is_ascii_uppercase())
                && kind.chars().all(|c| c.is_ascii_alphanumeric())
                && is_valid_id(id)
        }
        None => false,
    };
    if ok {
        Ok(())
    } else {
        Err(PipesError::Usage(format!(
            "--campaign {reference:?}: expected a reference ResourceType/id, e.g. CarePlan/bgd-mr-sia-2026"
        )))
    }
}

/// DHIS2 refuses analytics queries whose result is over its row limit
/// (E7128) or over the table-layout limit (E7151).
fn too_large(e: &FetchError) -> bool {
    matches!(e, FetchError::Status { status: 409, body } if body.contains("E7128") || body.contains("E7151"))
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
fn period_groups(periods: &[String]) -> Vec<Vec<String>> {
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
    fn when(&self) -> String {
        match &self.range {
            Some(r) => r.label(),
            None => periods_label(&self.periods),
        }
    }

    /// `level 4`, `levels 1, 2, 3`.
    fn where_(&self) -> String {
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
    let mut units = Vec::new();
    for &(level, org_units) in levels {
        for indicators in ids.chunks(chunk) {
            let per_period = indicators.len() as u64 * org_units.max(1);
            let per_batch = (max_rows as u64 / per_period).max(1) as usize;
            for group in &groups {
                for batch in group.chunks(per_batch) {
                    units.push(Unit {
                        levels: vec![level],
                        periods: batch.to_vec(),
                        range: None,
                        indicators: indicators.to_vec(),
                    });
                }
            }
            for range in ranges {
                units.push(Unit {
                    levels: vec![level],
                    periods: Vec::new(),
                    range: Some(range.clone()),
                    indicators: indicators.to_vec(),
                });
            }
        }
    }
    units
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
    let mut units = Vec::new();
    for indicators in ids.chunks(chunk) {
        let per_period = elements(indicators).max(1) as u64 * org_units.max(1);
        let per_batch = (max_rows as u64 / per_period).max(1) as usize;
        let unit = |periods: Vec<String>, range: Option<DateRange>| Unit {
            levels: levels.to_vec(),
            periods,
            range,
            indicators: indicators.to_vec(),
        };
        for group in &groups {
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

/// Every org unit DHIS2 has at `level` (under `root`).
fn org_unit_ids(api: &Dhis2, level: u32, root: Option<&str>) -> Result<Vec<String>> {
    let mut query = vec![
        ("filter", format!("level:eq:{level}")),
        ("fields", "id".into()),
    ];
    if let Some(root) = root {
        query.push(("filter", format!("path:like:{root}")));
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
        query.push(("filter", format!("path:like:{root}")));
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

/// `2025`, or `20250421…20250427 (7 periods)`.
fn periods_label(periods: &[String]) -> String {
    match periods {
        [one] => one.clone(),
        [first, .., last] => format!("{first}…{last} ({} periods)", periods.len()),
        [] => String::new(),
    }
}

/// One analytics request; the parts a too-large result is split along.
#[derive(Clone, Copy)]
struct Ask<'a> {
    level: u32,
    root: Option<&'a str>,
    periods: &'a [String],
    range: Option<&'a DateRange>,
    indicators: &'a [String],
    breakdown: Option<&'a str>,
}

impl Ask<'_> {
    fn when(&self) -> String {
        match self.range {
            Some(r) => r.label(),
            None => periods_label(self.periods),
        }
    }

    fn label(&self) -> String {
        format!(
            "level {} {}{}",
            self.level,
            self.when(),
            self.breakdown
                .map(|b| format!(" by {b}"))
                .unwrap_or_default()
        )
    }
}

/// A unit's analytics responses (or the same tables computed from data
/// values): the totals, then each breakdown's.
#[derive(Debug, Default)]
struct Fetched {
    totals: Vec<Analytics>,
    breakdowns: Vec<(String, Vec<Analytics>)>,
    requests: usize,
    /// Issues met while computing, as (kind, key, detail).
    issues: Vec<(String, String, String)>,
}

/// Where the values come from.
#[derive(Clone, Copy)]
enum Values<'a> {
    Analytics {
        root: Option<&'a str>,
    },
    DataValues {
        raw: &'a RawSource,
        indicators: &'a BTreeMap<String, Indicator>,
    },
}

fn fetch_unit(api: &Dhis2, values: Values, breakdowns: &[String], unit: &Unit) -> Result<Fetched> {
    let root = match values {
        Values::Analytics { root } => root,
        Values::DataValues { raw, indicators } => {
            let targets: Vec<Span> = match &unit.range {
                Some(r) => Span::of_range(r).into_iter().collect(),
                None => unit
                    .periods
                    .iter()
                    .filter_map(|p| Span::of_period(p))
                    .collect(),
            };
            let items: Vec<&Indicator> = unit
                .indicators
                .iter()
                .filter_map(|i| indicators.get(i))
                .collect();
            let (totals, by, issues) = raw.compute(api, &items, &targets)?;
            return Ok(Fetched {
                totals: vec![totals],
                breakdowns: breakdowns
                    .iter()
                    .cloned()
                    .zip(by.into_iter().map(|t| vec![t]))
                    .collect(),
                requests: 1,
                issues,
            });
        }
    };
    let mut f = Fetched::default();
    let ask = Ask {
        level: unit.levels[0],
        root,
        periods: &unit.periods,
        range: unit.range.as_ref(),
        indicators: &unit.indicators,
        breakdown: None,
    };
    fetch(api, ask, &mut f.requests, &mut f.totals)?;
    for b in breakdowns {
        let mut tables = Vec::new();
        let ask = Ask {
            breakdown: Some(b),
            ..ask
        };
        fetch(api, ask, &mut f.requests, &mut tables)?;
        f.breakdowns.push((b.clone(), tables));
    }
    Ok(f)
}

/// Asks DHIS2, splitting the request in two and asking again while DHIS2
/// says the result is too large: the periods first, then the indicators.
/// A date range is sent as `startDate` and `endDate` (DHIS2 takes them
/// instead of a period dimension, never with one).
fn fetch(api: &Dhis2, ask: Ask, requests: &mut usize, out: &mut Vec<Analytics>) -> Result<()> {
    let when = match ask.range {
        Some(r) => [("startDate", r.start.clone()), ("endDate", r.end.clone())].to_vec(),
        None => vec![("dimension", format!("pe:{}", ask.periods.join(";")))],
    };
    let mut query = vec![("dimension", format!("dx:{}", ask.indicators.join(";")))];
    query.extend(when);
    query.extend([
        (
            "dimension",
            match ask.root {
                Some(root) => format!("ou:LEVEL-{};{root}", ask.level),
                None => format!("ou:LEVEL-{}", ask.level),
            },
        ),
        ("includeNumDen", "true".into()),
        ("skipRounding", "true".into()),
        ("includeMetadataDetails", "true".into()),
        ("displayProperty", "NAME".into()),
    ]);
    if let Some(b) = ask.breakdown {
        query.push(("dimension", b.to_string()));
    }
    *requests += 1;
    let body = match api.try_get("analytics.json", &query)? {
        Ok(v) => v,
        Err(e) if too_large(&e) && ask.periods.len() > 1 => {
            let (a, b) = ask.periods.split_at(ask.periods.len() / 2);
            eprintln!(
                "{}: result too large for DHIS2, splitting {} periods in two",
                ask.label(),
                ask.periods.len()
            );
            fetch(api, Ask { periods: a, ..ask }, requests, out)?;
            return fetch(api, Ask { periods: b, ..ask }, requests, out);
        }
        Err(e) if too_large(&e) && ask.indicators.len() > 1 => {
            let (a, b) = ask.indicators.split_at(ask.indicators.len() / 2);
            eprintln!(
                "{}: result too large for DHIS2, splitting {} indicators in two",
                ask.label(),
                ask.indicators.len()
            );
            fetch(
                api,
                Ask {
                    indicators: a,
                    ..ask
                },
                requests,
                out,
            )?;
            return fetch(
                api,
                Ask {
                    indicators: b,
                    ..ask
                },
                requests,
                out,
            );
        }
        Err(e) if too_large(&e) => {
            return Err(PipesError::Usage(format!(
                "indicator {} at level {} for {} is over DHIS2's analytics row limit on its own; ask for shorter periods ({e})",
                ask.indicators[0],
                ask.level,
                ask.when()
            )));
        }
        Err(FetchError::Status { status: 409, body }) => {
            return Err(PipesError::Usage(format!(
                "DHIS2 rejected the analytics query for level {} and period {}: {body}",
                ask.level,
                match ask.range {
                    Some(r) => r.label(),
                    None => ask.periods.join(";"),
                }
            )));
        }
        Err(e) => return Err(api.error("analytics.json", e)),
    };
    let mut table = parse_analytics(&body, ask.breakdown)?;
    if let Some(r) = ask.range {
        let code = r.code();
        for row in &mut table.rows {
            row.pe.clone_from(&code);
        }
        table.periods.insert(code, (r.start.clone(), r.end.clone()));
    }
    out.push(table);
    Ok(())
}

/// Totals that can be counted from the reports themselves, so a resumed run
/// recounts them from what the interrupted run wrote.
#[derive(Debug, Default)]
struct Tally {
    written: usize,
    locations: HashSet<String>,
    periods: HashSet<String>,
    stratified: usize,
    new: usize,
    changed: usize,
    unchanged: usize,
}

impl Tally {
    /// Counts one written report, comparing it with the previous run's.
    fn add(&mut self, r: &Value, previous: Option<&mut Previous>) -> Option<Change> {
        self.written += 1;
        if let Some(s) = r.pointer("/subject/reference").and_then(Value::as_str)
            && !self.locations.contains(s)
        {
            self.locations.insert(s.to_string());
        }
        if let Some(pe) = r
            .pointer("/extension/0/valueString")
            .and_then(Value::as_str)
            && !self.periods.contains(pe)
        {
            self.periods.insert(pe.to_string());
        }
        self.stratified += r
            .pointer("/group/0/stratifier")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let change = previous.map(|p| p.classify(r));
        match change {
            Some(Change::New) => self.new += 1,
            Some(Change::Changed) => self.changed += 1,
            Some(Change::Unchanged) => self.unchanged += 1,
            None => {}
        }
        change
    }
}

/// Turns fetched units into written reports. It runs on one thread and
/// takes units in planned order, so the output and the run report are the
/// same at any `--concurrency`; it keeps running totals, not reports.
/// After each unit the output is synced and the checkpoint saved, so an
/// interrupted run can carry on from there.
struct Writer<'a> {
    out: NdjsonWriter,
    /// `--changed`: the new and changed reports.
    changed: Option<NdjsonWriter>,
    previous: Option<Previous>,
    instance: &'a Instance,
    info: &'a ServerInfo,
    indicators: &'a BTreeMap<String, Indicator>,
    mapping: &'a HashMap<String, Target>,
    /// `--campaign`, added to every report after the period extension.
    campaign: Option<&'a str>,
    /// `analytics` or `data-values`, the source extension's code.
    source: &'static str,
    state: Checkpoint,
    checkpoint: PathBuf,
    tally: Tally,
}

impl<'a> Writer<'a> {
    /// The indicator and Location of a row, or `None` after reporting why
    /// the row is skipped.
    fn target(&mut self, row: &AnalyticsRow) -> Option<(&'a Indicator, &'a Target)> {
        let indicators = self.indicators;
        let mapping = self.mapping;
        let Some(ind) = indicators.get(&row.dx) else {
            self.state.report.add(
                "unexpected_indicator",
                &row.dx,
                "DHIS2 returned an indicator that was not asked for; skipped",
            );
            return None;
        };
        let Some(location) = mapping.get(&row.ou) else {
            if self.state.unmapped.insert(row.ou.clone()) {
                self.state.report.add(
                    "unmapped_org_unit",
                    &row.ou,
                    "no Location carries this org unit; its values are skipped",
                );
            }
            return None;
        };
        Some((ind, location))
    }

    /// With `--previous`, notes the org units and periods a response
    /// covered: where removed values are looked for.
    fn scope(&mut self, table: &Analytics) {
        if self.previous.is_none() {
            return;
        }
        let s = &mut self.state;
        for ou in table
            .org_units
            .iter()
            .chain(table.rows.iter().map(|r| &r.ou))
        {
            if !s.scope_org_units.contains(ou) {
                s.scope_org_units.insert(ou.clone());
            }
        }
        for pe in &table.period_codes {
            s.scope_periods.insert(period_value(pe));
        }
        for row in &table.rows {
            s.scope_periods.insert(period_value(&row.pe));
        }
    }

    /// Writes one unit's reports, sorted by id, with its strata attached to
    /// its totals; returns how many values DHIS2 sent for it.
    fn unit(&mut self, f: Fetched) -> Result<usize> {
        for (kind, key, detail) in &f.issues {
            if kind != "unmapped_org_unit" || self.state.unmapped.insert(key.clone()) {
                self.state.report.add(kind, key, detail);
            }
        }
        let mut rows = 0usize;
        let mut out: BTreeMap<String, Value> = BTreeMap::new();
        for table in &f.totals {
            rows += table.rows.len();
            self.scope(table);
            for row in &table.rows {
                let Some((ind, location)) = self.target(row) else {
                    continue;
                };
                let Some(period) = table.periods.get(&row.pe) else {
                    self.state.report.add(
                        "period_without_dates",
                        &row.pe,
                        "DHIS2 gave no start and end date for this period; row skipped",
                    );
                    continue;
                };
                if let Some(mut r) = measure_report(
                    row,
                    ind,
                    location,
                    period,
                    self.instance,
                    self.info,
                    &mut self.state.report,
                ) {
                    if let Some(ext) = r["extension"].as_array_mut() {
                        if let Some(c) = self.campaign {
                            ext.push(json!({"url": self.instance.urls.campaign_extension, "valueReference": {"reference": c}}));
                        }
                        ext.push(json!({"url": self.instance.urls.source_extension, "valueCode": self.source}));
                    }
                    out.insert(r["id"].as_str().unwrap_or_default().to_string(), r);
                }
            }
        }

        // (report id, breakdown) → (breakdown name, strata)
        let mut strata: BTreeMap<(String, String), (String, Vec<Value>)> = BTreeMap::new();
        for (b, tables) in &f.breakdowns {
            for table in tables {
                rows += table.rows.len();
                for row in &table.rows {
                    let Some((ind, _)) = self.target(row) else {
                        continue;
                    };
                    if let Some(st) = stratum(
                        row,
                        ind,
                        b,
                        &table.names,
                        self.instance,
                        &mut self.state.report,
                    ) {
                        let name = table.names.get(b).cloned().unwrap_or_else(|| b.clone());
                        strata
                            .entry((report_id(self.instance, row), b.clone()))
                            .or_insert_with(|| (name, Vec::new()))
                            .1
                            .push(st);
                    }
                }
            }
        }
        for ((rid, b), (name, mut list)) in strata {
            let Some(r) = out.get_mut(&rid) else {
                self.state.report.add(
                    "stratum_without_total",
                    &rid,
                    &format!("DHIS2 broke a value down by {name} but gave no total for it; breakdown left out"),
                );
                continue;
            };
            list.sort_by(|a, b| {
                a["value"]["text"]
                    .as_str()
                    .cmp(&b["value"]["text"].as_str())
            });
            let stratifier = json!({
                "code": [{"coding": [{"system": self.instance.dimension_system(None), "code": b, "display": name}], "text": name}],
                "stratum": list,
            });
            match r["group"][0]
                .get_mut("stratifier")
                .and_then(Value::as_array_mut)
            {
                Some(existing) => existing.push(stratifier),
                None => r["group"][0]["stratifier"] = json!([stratifier]),
            }
        }

        for r in out.values() {
            let change = self.tally.add(r, self.previous.as_mut());
            self.out.write(r)?;
            if let (Some(w), Some(Change::New | Change::Changed)) = (self.changed.as_mut(), change)
            {
                w.write(r)?;
            }
        }
        self.state.values += rows;
        self.state.requests += f.requests;
        Ok(rows)
    }

    /// Syncs everything written and records that `units_done` units are in
    /// the files. A crash before this leaves the last checkpoint, and a
    /// resumed run cuts the files back to it.
    fn save(&mut self, units_done: usize) -> Result<()> {
        self.state.out_bytes = self.out.sync()?;
        if let Some(w) = &mut self.changed {
            self.state.changed_bytes = Some(w.sync()?);
        }
        self.state.units_done = units_done;
        self.state.write(&self.checkpoint)
    }

    /// After a resume: counts the reports the interrupted run wrote, and
    /// marks their ids as written again for `--removed`.
    fn recount(&mut self) -> Result<()> {
        let path = self.out.tmp().to_path_buf();
        let mut reader = self.out.read_back()?;
        let mut line = String::new();
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .map_err(|e| PipesError::io(&path, e))?
                == 0
            {
                return Ok(());
            }
            let r: Value = serde_json::from_str(&line)?;
            self.tally.add(&r, self.previous.as_mut());
        }
    }
}

/// `1234567` → `1,234,567`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `42s`, `4m12s`, `1h05m`; to the minute only, `4m`, when `rough`.
fn clock(d: Duration, rough: bool) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 if rough => format!("{}m", s / 60),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `[37/412] level 4 · 20260421…20260427 (7 periods) · 10 items: 33,656
/// values · 4m12s elapsed, ~38m left`. The estimate assumes the units left
/// take as long as the ones done since `start` (where a resumed run began).
fn progress(
    done: usize,
    start: usize,
    total: usize,
    unit: &Unit,
    values: usize,
    elapsed: Duration,
) -> String {
    let left = if done < total && done > start {
        let rest = elapsed.mul_f64((total - done) as f64 / (done - start) as f64);
        format!(", ~{} left", clock(rest, true))
    } else {
        String::new()
    };
    format!(
        "[{done}/{total}] {} · {} · {} items: {} values · {} elapsed{left}",
        unit.where_(),
        unit.when(),
        unit.indicators.len(),
        thousands(values),
        clock(elapsed, false)
    )
}

/// Each value once, in the order first given.
fn unique<T: Clone + PartialEq>(list: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for x in list {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

pub fn run_reports(args: &Dhis2ReportsArgs, profile: &crate::profile::Profile) -> Result<()> {
    check_out(&args.out)?;
    if args.chunk == 0 {
        return Err(PipesError::Usage("--chunk must be at least 1".into()));
    }
    if args.max_rows == 0 {
        return Err(PipesError::Usage("--max-rows must be at least 1".into()));
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()));
    }
    if args.levels.contains(&0) {
        return Err(PipesError::Usage(
            "--level counts from 1, the top of the DHIS2 tree".into(),
        ));
    }
    for (flag, path) in [("--changed", &args.changed), ("--removed", &args.removed)] {
        if let Some(p) = path {
            check_out(p)?;
            if p == &args.out || Some(p) == args.previous.as_ref() {
                return Err(PipesError::Usage(format!(
                    "{flag} {}: must be a file of its own, not --out or --previous",
                    p.display()
                )));
            }
        }
    }
    let from_values = args.source == ReportSource::DataValues;
    if let Some(since) = &args.updated_since {
        if !from_values {
            return Err(PipesError::Usage(
                "--updated-since needs --source data-values (analytics has no last-updated filter)"
                    .into(),
            ));
        }
        let shaped = since.len() == 10 || (since.len() >= 16 && since.as_bytes()[10] == b'T');
        if !shaped || super::api::epoch_seconds(since).is_none() {
            return Err(PipesError::Usage(format!(
                "--updated-since {since:?}: expected YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS"
            )));
        }
    }
    if let Some(p) = &args.previous
        && !p.is_file()
    {
        return Err(PipesError::Usage(format!(
            "--previous {}: no such file",
            p.display()
        )));
    }
    let instance = Instance::new(&args.conn.instance, profile)?;
    let ranges = unique(&args.ranges)
        .iter()
        .map(|r| DateRange::parse(r))
        .collect::<Result<Vec<_>>>()?;
    if let Some(c) = &args.campaign {
        check_campaign(c)?;
    }
    if !args.locations.is_file() {
        return Err(PipesError::Usage(format!(
            "--locations {}: no such file",
            args.locations.display()
        )));
    }
    let system = instance.orgunit_system();
    let mapping = load_mapping(&args.locations, &system)?;
    if mapping.is_empty() {
        return Err(PipesError::Usage(format!(
            "no Location in {} carries an identifier under {system}; run `fhir-pipes dhis2 locations --instance {}` and load its output first",
            args.locations.display(),
            instance.name
        )));
    }

    let api = Dhis2::connect(&args.conn)?;
    if !from_values {
        match &api.info.last_analytics {
            Some(t) => eprintln!("DHIS2 analytics tables last built {t}"),
            None => eprintln!("DHIS2 does not say when its analytics tables were last built"),
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        if let Some(warning) = check_analytics_age(&api.info, args.max_analytics_age, now)? {
            eprintln!("{warning}");
        }
    }
    let indicators = fetch_indicators(&api, &args.select)?;
    let ids: Vec<String> = indicators.keys().cloned().collect();
    // A range's code is longer than a period's: check now that every
    // report id still fits, rather than skip every row as invalid_id.
    for range in &ranges {
        for dx in &ids {
            let id = format!(
                "{}-{}-{}-{}",
                instance.name,
                fhir_key(dx),
                "x".repeat(UID_LEN),
                range.code()
            );
            if !is_valid_id(&id) {
                return Err(PipesError::Usage(format!(
                    "--range {}: report ids for {dx} would be {} characters, over FHIR's 64; use a shorter --instance",
                    range.label(),
                    id.len()
                )));
            }
        }
    }

    let mut periods = unique(&args.periods);
    let mut ranges = ranges;
    let mut raw: Option<RawSource> = None;
    let mut levels: Vec<(u32, u64)> = Vec::new();
    let units = if from_values {
        if let Some(p) = periods.iter().find(|p| Span::of_period(p).is_none()) {
            return Err(PipesError::Usage(format!(
                "--period {p}: --source data-values knows the dates of days, ISO weeks, months, bi-months, quarters, six-months and years only (no relative periods); use --range for other spans"
            )));
        }
        let mut src = prepare(&api, &indicators, &args.breakdowns)?;
        let mut by_level = Vec::new();
        for level in unique(&args.levels) {
            let ous = org_unit_ids(&api, level, args.root.as_deref())?;
            eprintln!("level {level}: {} org units", ous.len());
            levels.push((level, ous.len() as u64));
            by_level.push((level, ous));
        }
        let parents = load_parents(&args.locations, &system)?;
        src.place(&parents, &by_level, args.root.as_deref());
        if let Some(since) = &args.updated_since {
            let spans: Vec<Span> = periods
                .iter()
                .filter_map(|p| Span::of_period(p))
                .chain(ranges.iter().filter_map(Span::of_range))
                .collect();
            let changed = src.changed(&api, &src.elements_of(&ids), &spans, since)?;
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
            &ids,
            args.chunk,
            args.max_rows,
            &|chunk| src.elements_of(chunk).len(),
        );
        eprintln!("{} batches, one dataValueSets request each", units.len());
        raw = Some(src);
        units
    } else {
        // Requests are sized from each level's org unit count, so several
        // periods can share one.
        for level in unique(&args.levels) {
            let n = org_unit_count(&api, level, args.root.as_deref())?;
            eprintln!("level {level}: {n} org units");
            levels.push((level, n));
        }
        let units = plan(&levels, &periods, &ranges, &ids, args.chunk, args.max_rows);
        eprintln!(
            "{} batches, {} analytics requests unless DHIS2 calls some too large",
            units.len(),
            units.len() * (1 + args.breakdowns.len())
        );
        units
    };
    let total = units.len();

    // Everything that decides the batches and what goes in them. An
    // interrupted run is continued only under the same plan.
    let fingerprint = json!({
        "dhis2": args.conn.url,
        "instance": instance.name,
        "source": args.source.code(),
        "updated_since": args.updated_since,
        // Data values do not depend on the analytics build.
        "last_analytics": if from_values { None } else { api.info.last_analytics.clone() },
        "indicators": ids,
        "levels": levels,
        "periods": periods,
        "ranges": ranges.iter().map(DateRange::label).collect::<Vec<_>>(),
        "root": args.root,
        "breakdowns": args.breakdowns,
        "chunk": args.chunk,
        "max_rows": args.max_rows,
        "campaign": args.campaign,
        "locations": file_stamp(&args.locations),
        "previous": args.previous.as_deref().map(file_stamp),
        "changed": args.changed.as_ref().map(|p| p.display().to_string()),
    });
    let checkpoint = Checkpoint::path(&args.out);
    let saved = if args.resume {
        let saved =
            Checkpoint::read(&checkpoint)?.filter(|_| NdjsonWriter::tmp_path(&args.out).is_file());
        if saved.is_none() {
            eprintln!(
                "no interrupted run to resume at {}; starting from the beginning",
                args.out.display()
            );
        }
        saved
    } else {
        if checkpoint.is_file() {
            eprintln!(
                "starting over; the interrupted run at {} is discarded (--resume continues it)",
                args.out.display()
            );
        }
        None
    };
    if let Some(s) = &saved {
        let differ = plan_differences(&s.plan, &fingerprint);
        if !differ.is_empty() {
            return Err(PipesError::Usage(format!(
                "--resume: the interrupted run at {} was planned differently ({} changed); run without --resume to start over",
                args.out.display(),
                differ.join(", ")
            )));
        }
    }

    let previous = match &args.previous {
        Some(p) => {
            let prev = Previous::load(p)?;
            eprintln!("{}: {} previous reports", p.display(), prev.len());
            Some(prev)
        }
        None => None,
    };
    let start = saved.as_ref().map_or(0, |s| s.units_done);
    let (out, changed, state) = match saved {
        Some(s) => {
            let out = NdjsonWriter::resume(&args.out, s.out_bytes)?;
            let changed = match (&args.changed, s.changed_bytes) {
                (Some(p), Some(bytes)) => Some(NdjsonWriter::resume(p, bytes)?),
                (Some(p), None) => Some(NdjsonWriter::create(p)?),
                (None, _) => None,
            };
            eprintln!(
                "resuming after batch {start} of {total} ({})",
                NdjsonWriter::tmp_path(&args.out).display()
            );
            (out, changed, s)
        }
        None => (
            NdjsonWriter::create(&args.out)?,
            args.changed
                .as_deref()
                .map(NdjsonWriter::create)
                .transpose()?,
            Checkpoint {
                plan: fingerprint,
                as_of: from_values.then(now_utc),
                ..Checkpoint::default()
            },
        ),
    };
    // From data values, a report's `date` is when the run started.
    let info = match &state.as_of {
        Some(t) if from_values => ServerInfo {
            last_analytics: Some(t.clone()),
            utc: true,
            ..api.info.clone()
        },
        _ => api.info.clone(),
    };
    let values = match &raw {
        Some(src) => Values::DataValues {
            raw: src,
            indicators: &indicators,
        },
        None => Values::Analytics {
            root: args.root.as_deref(),
        },
    };
    let mut w = Writer {
        out: out.keep_partial(),
        changed: changed.map(NdjsonWriter::keep_partial),
        previous,
        instance: &instance,
        info: &info,
        indicators: &indicators,
        mapping: &mapping,
        campaign: args.campaign.as_deref(),
        source: args.source.code(),
        state,
        checkpoint: checkpoint.clone(),
        tally: Tally::default(),
    };
    if start > 0 {
        w.recount()?;
    }
    w.save(start)?;

    // Workers fetch units from a shared counter; this thread writes them in
    // planned order, holding back any that finish early. A worker does not
    // start a unit more than `window` ahead of the last one written, so
    // memory stays at a few units, not the whole export.
    let workers = args.concurrency.min(total - start).max(1);
    let window = workers * 2;
    let next = AtomicUsize::new(start);
    let stop = AtomicBool::new(false);
    let written = (Mutex::new(start), Condvar::new());
    let (tx, rx) = mpsc::sync_channel::<(usize, Result<Fetched>)>(window);
    let started = Instant::now();
    let mut failure: Option<PipesError> = None;
    let mut cursor = start;
    std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (api, units, next, stop, written) = (&api, &units, &next, &stop, &written);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    {
                        let (lock, ready) = written;
                        let mut done = lock.lock().unwrap();
                        while i >= *done + window && !stop.load(Ordering::Relaxed) {
                            done = ready.wait(done).unwrap();
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let fetched = fetch_unit(api, values, &args.breakdowns, &units[i]);
                    if tx.send((i, fetched)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut pending: HashMap<usize, Fetched> = HashMap::new();
        for (i, fetched) in rx.iter() {
            match fetched {
                Ok(f) => {
                    pending.insert(i, f);
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
            while let Some(f) = pending.remove(&cursor) {
                match w
                    .unit(f)
                    .and_then(|values| w.save(cursor + 1).map(|_| values))
                {
                    Ok(values) => eprintln!(
                        "{}",
                        progress(
                            cursor + 1,
                            start,
                            total,
                            &units[cursor],
                            values,
                            started.elapsed()
                        )
                    ),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
                cursor += 1;
                *written.0.lock().unwrap() = cursor;
                written.1.notify_all();
            }
            if failure.is_some() {
                break;
            }
        }
        if failure.is_some() {
            // Set under the lock so a worker about to wait sees it.
            let guard = written.0.lock().unwrap();
            stop.store(true, Ordering::Relaxed);
            drop(guard);
            written.1.notify_all();
            // Let workers still mid-request finish and exit.
            for _ in rx.iter() {}
        }
    });
    if let Some(e) = failure {
        eprintln!(
            "{cursor} of {total} batches written to {}; run again with --resume to continue from there",
            w.out.tmp().display()
        );
        return Err(e);
    }

    // Previous reports this run would have written but DHIS2 no longer has.
    // Read before --out is renamed, as it may be the --previous file. With
    // --updated-since, the previous reports for the periods not recomputed
    // are kept in --out, so it stays the whole baseline.
    let mut kept = 0usize;
    let keep_rest = args.updated_since.is_some();
    let removed = match &w.previous {
        Some(prev) => {
            let mut scope = Scope {
                measures: ids
                    .iter()
                    .map(|dx| instance.measure_url(&fhir_key(dx)))
                    .collect(),
                subjects: w
                    .state
                    .scope_org_units
                    .iter()
                    .filter_map(|ou| mapping.get(ou))
                    .map(|t| format!("Location/{}", t.id))
                    .collect(),
                periods: w.state.scope_periods.iter().cloned().collect(),
            };
            scope
                .periods
                .extend(periods.iter().filter(|p| period_kind(p).is_some()).cloned());
            scope.periods.extend(ranges.iter().map(DateRange::label));
            let out = &mut w.out;
            Some(prev.removed(
                |r| scope.contains(r, &instance.urls.period_extension),
                |r| {
                    if keep_rest {
                        kept += 1;
                        out.write(r)?;
                    }
                    Ok(())
                },
            )?)
        }
        None => None,
    };

    let Writer {
        out,
        changed,
        state,
        tally,
        ..
    } = w;
    if let Some(c) = changed {
        c.finish()?;
    }
    if let (Some(path), Some(ids)) = (&args.removed, &removed) {
        write_lines(path, ids)?;
    }
    out.finish()?;
    std::fs::remove_file(&checkpoint).map_err(|e| PipesError::io(&checkpoint, e))?;

    println!(
        "Wrote {} MeasureReports to {} ({} indicators, {} Locations, {} periods; {} values from {} {} requests)",
        tally.written,
        args.out.display(),
        indicators.len(),
        tally.locations.len(),
        tally.periods.len(),
        state.values,
        state.requests,
        if from_values {
            "dataValueSets"
        } else {
            "analytics"
        }
    );
    if !args.breakdowns.is_empty() {
        println!(
            "{} breakdowns attached ({} dimensions)",
            tally.stratified,
            args.breakdowns.len()
        );
    }
    if let (Some(prev), Some(removed)) = (&args.previous, &removed) {
        println!(
            "Compared with {}: {} new, {} changed, {} unchanged, {} removed",
            prev.display(),
            tally.new,
            tally.changed,
            tally.unchanged,
            removed.len()
        );
        if let Some(p) = &args.changed {
            println!(
                "Wrote {} new or changed MeasureReports to {}",
                tally.new + tally.changed,
                p.display()
            );
        }
        if let Some(p) = &args.removed {
            println!("Wrote {} removed ids to {}", removed.len(), p.display());
        }
        if keep_rest {
            println!(
                "Kept {kept} MeasureReports from {} for periods with no updated data values",
                prev.display()
            );
        }
    }
    println!("{}", state.report.summary());
    if let Some(path) = &args.report {
        write_report(path, &state.report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analytics() -> Value {
        json!({
            "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},
                        {"name":"denominator"},{"name":"factor"},{"name":"multiplier"},{"name":"divisor"}],
            "metaData": {"items": {
                "2025": {"name":"2025","startDate":"2025-01-01T00:00:00.000","endDate":"2025-12-31T00:00:00.000"},
                "O6uvpzGd5pu": {"name":"Bo"}
            }},
            "rows": [["tUIlpyeeX9N","2025","O6uvpzGd5pu","27.9764","62045.0","221781.0","100.0","36500","365"]]
        })
    }

    fn loc() -> Target {
        Target {
            id: "loc".into(),
            name: None,
        }
    }

    fn penta3() -> Indicator {
        Indicator::parse(&json!({"id":"tUIlpyeeX9N","name":"Penta 3 Coverage <1y",
            "indicatorType":{"factor":100},"numerator":"#{a}","denominator":"#{b}"}))
        .unwrap()
    }

    #[test]
    fn analytics_rows_and_period_dates_are_read_by_column_name() {
        let t = parse_analytics(&analytics(), None).unwrap();
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].ou, "O6uvpzGd5pu");
        assert_eq!(t.rows[0].numerator, "62045.0");
        assert_eq!(
            t.periods.get("2025"),
            Some(&("2025-01-01".to_string(), "2025-12-31".to_string()))
        );
        assert!(!t.periods.contains_key("O6uvpzGd5pu"));
        assert!(parse_analytics(&json!({"rows": []}), None).is_err());
    }

    #[test]
    fn a_row_becomes_a_summary_report_with_the_score_as_a_proportion() {
        let t = parse_analytics(&analytics(), None).unwrap();
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let info = ServerInfo {
            system_name: Some("DHIS 2 Demo - Sierra Leone".into()),
            last_analytics: Some("2026-10-12T02:07:00.052".into()),
            utc: true,
            ..Default::default()
        };
        let mut report = Report::default();
        let r = measure_report(
            &t.rows[0],
            &penta3(),
            &Target {
                id: "sl-demo-O6uvpzGd5pu".into(),
                name: Some("Bo".into()),
            },
            &t.periods["2025"],
            &instance,
            &info,
            &mut report,
        )
        .unwrap();
        assert_eq!(r["id"], "sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-2025");
        assert_eq!(
            r["measure"],
            "https://icr.healthcampaigns.org/Measure/sl-demo-tUIlpyeeX9N"
        );
        assert_eq!(r["subject"]["reference"], "Location/sl-demo-O6uvpzGd5pu");
        assert_eq!(r["subject"]["display"], "Bo");
        assert_eq!(r["type"], "summary");
        assert_eq!(r["date"], "2026-10-12T02:07:00.052Z");
        assert_eq!(r["reporter"]["display"], "DHIS 2 Demo - Sierra Leone");
        assert_eq!(r["period"]["start"], "2025-01-01");
        assert_eq!(r["period"]["end"], "2025-12-31");
        assert_eq!(r["extension"][0]["valueString"], "2025");
        let g = &r["group"][0];
        assert_eq!(g["population"][0]["count"], 62045);
        assert_eq!(g["population"][1]["count"], 221781);
        assert_eq!(g["measureScore"]["value"], 0.279764);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn fractional_and_blank_counts() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let row = AnalyticsRow {
            dx: "tUIlpyeeX9N".into(),
            pe: "202501".into(),
            ou: "ou".into(),
            value: "12.5".into(),
            numerator: "10.4".into(),
            denominator: "".into(),
            option: None,
        };
        let mut report = Report::default();
        let r = measure_report(
            &row,
            &penta3(),
            &loc(),
            &("2025-01-01".into(), "2025-01-31".into()),
            &instance,
            &ServerInfo::default(),
            &mut report,
        )
        .unwrap();
        let pops = r["group"][0]["population"].as_array().unwrap();
        assert_eq!(pops.len(), 1);
        assert_eq!(pops[0]["count"], 10);
        assert_eq!(report.count("fractional_count"), 1);
        assert!(r.get("date").is_none());

        let bad = AnalyticsRow {
            value: "n/a".into(),
            ..row
        };
        assert!(
            measure_report(
                &bad,
                &penta3(),
                &loc(),
                &("a".into(), "b".into()),
                &instance,
                &ServerInfo::default(),
                &mut report
            )
            .is_none()
        );
        assert_eq!(report.count("bad_number"), 1);
    }

    #[test]
    fn a_cohort_reports_the_numerator_as_its_population() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let ind = Indicator::parse(&json!({"id":"dy1a1mseGR7","indicatorType":{"factor":1},"numerator":"#{a}","denominator":"1"})).unwrap();
        let row = AnalyticsRow {
            dx: "dy1a1mseGR7".into(),
            pe: "2025".into(),
            ou: "ou".into(),
            value: "1234.0".into(),
            numerator: "1234.0".into(),
            denominator: "1.0".into(),
            option: None,
        };
        let mut report = Report::default();
        let r = measure_report(
            &row,
            &ind,
            &loc(),
            &("2025-01-01".into(), "2025-12-31".into()),
            &instance,
            &ServerInfo::default(),
            &mut report,
        )
        .unwrap();
        let pops = r["group"][0]["population"].as_array().unwrap();
        assert_eq!(pops.len(), 1);
        assert_eq!(pops[0]["code"]["coding"][0]["code"], "initial-population");
        assert_eq!(r["group"][0]["measureScore"]["value"], 1234.0);
    }

    #[test]
    fn a_data_element_row_counts_its_value() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let de =
            Indicator::from_data_element(&json!({"id": "s46m5MS0hxu", "name": "BCG doses given"}))
                .unwrap();
        let row = AnalyticsRow {
            dx: "s46m5MS0hxu".into(),
            pe: "2025".into(),
            ou: "ou".into(),
            value: "88039.0".into(),
            numerator: "".into(),
            denominator: "".into(),
            option: None,
        };
        let mut report = Report::default();
        let r = measure_report(
            &row,
            &de,
            &loc(),
            &("2025-01-01".into(), "2025-12-31".into()),
            &instance,
            &ServerInfo::default(),
            &mut report,
        )
        .unwrap();
        assert_eq!(
            r["group"][0]["population"][0]["code"]["coding"][0]["code"],
            "initial-population"
        );
        assert_eq!(r["group"][0]["population"][0]["count"], 88039);
        assert_eq!(r["group"][0]["measureScore"]["value"], 88039.0);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn a_breakdown_row_is_a_coded_stratum_with_its_own_figures() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let v = json!({
            "headers": [{"name":"dx"},{"name":"fMZEcRHuamy"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {"fMZEcRHuamy": {"name": "Location Fixed/Outreach"}, "qkPbeWaFsnU": {"name": "Fixed"}}},
            "rows": [["tUIlpyeeX9N","qkPbeWaFsnU","2025","O6uvpzGd5pu","25.0","50.0","200.0"]]
        });
        let t = parse_analytics(&v, Some("fMZEcRHuamy")).unwrap();
        assert_eq!(t.rows[0].option.as_deref(), Some("qkPbeWaFsnU"));
        let mut report = Report::default();
        let s = stratum(
            &t.rows[0],
            &penta3(),
            "fMZEcRHuamy",
            &t.names,
            &instance,
            &mut report,
        )
        .unwrap();
        assert_eq!(s["value"]["text"], "Fixed");
        assert_eq!(
            s["value"]["coding"][0]["system"],
            instance.dimension_system(Some("fMZEcRHuamy"))
        );
        assert_eq!(s["population"][0]["count"], 50);
        assert_eq!(s["measureScore"]["value"], 0.25);
        assert!(parse_analytics(&v, Some("J5jldMd8OHv")).is_err());
    }

    #[test]
    fn the_mapping_reads_only_this_instances_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.ndjson");
        std::fs::write(
            &p,
            concat!(
                r#"{"resourceType":"Location","id":"sle-a","identifier":[{"system":"S","value":"a"}]}"#, "\n",
                r#"{"resourceType":"Organization","id":"org-sle-a","identifier":[{"system":"S","value":"a"}]}"#, "\n",
                "\n",
                r#"{"resourceType":"Location","id":"other","identifier":[{"system":"T","value":"b"}]}"#, "\n",
            ),
        )
        .unwrap();
        let m = load_mapping(&p, "S").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m["a"].id, "sle-a");
    }

    fn codes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

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
    fn progress_reads_at_a_glance() {
        let unit = Unit {
            levels: vec![4],
            periods: codes(&["20260421", "20260422", "20260427"]),
            range: None,
            indicators: codes(&["a"; 10]),
        };
        assert_eq!(
            progress(37, 0, 412, &unit, 33_656, Duration::from_secs(252)),
            "[37/412] level 4 · 20260421…20260427 (3 periods) · 10 items: 33,656 values · 4m12s elapsed, ~42m left"
        );
        let one = Unit {
            periods: codes(&["2025"]),
            ..unit
        };
        assert_eq!(
            progress(2, 0, 2, &one, 5, Duration::from_secs(3725)),
            "[2/2] level 4 · 2025 · 10 items: 5 values · 1h02m elapsed"
        );
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
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

    #[test]
    fn a_range_report_carries_the_range_as_its_period() {
        let instance = Instance::new("bgd-epi-2026", &crate::profile::icr_example()).unwrap();
        let row = AnalyticsRow {
            dx: "tUIlpyeeX9N".into(),
            pe: "20260405-20260520".into(),
            ou: "O6uvpzGd5pu".into(),
            value: "50.0".into(),
            numerator: "5.0".into(),
            denominator: "10.0".into(),
            option: None,
        };
        let mut report = Report::default();
        let r = measure_report(
            &row,
            &penta3(),
            &loc(),
            &("2026-04-05".into(), "2026-05-20".into()),
            &instance,
            &ServerInfo::default(),
            &mut report,
        )
        .unwrap();
        assert_eq!(
            r["id"],
            "bgd-epi-2026-tUIlpyeeX9N-O6uvpzGd5pu-20260405-20260520"
        );
        assert!(r["id"].as_str().unwrap().len() <= 64);
        assert_eq!(r["extension"][0]["valueString"], "2026-04-05/2026-05-20");
        assert_eq!(r["period"]["start"], "2026-04-05");
        assert_eq!(r["period"]["end"], "2026-05-20");
    }

    #[test]
    fn a_response_by_date_range_has_no_period_column() {
        let v = json!({
            "headers": [{"name":"dx"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {}},
            "rows": [["tUIlpyeeX9N","O6uvpzGd5pu","50.0","5.0","10.0"]]
        });
        let t = parse_analytics(&v, None).unwrap();
        assert_eq!(t.rows[0].pe, "");
        assert_eq!(t.rows[0].numerator, "5.0");
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

    #[test]
    fn a_campaign_is_a_reference() {
        assert!(check_campaign("CarePlan/bgd-mr-sia-2026").is_ok());
        for bad in [
            "bgd-mr-sia-2026",
            "careplan/x",
            "CarePlan/",
            "CarePlan/a_b",
            "Care Plan/x",
            "CarePlan/x/y",
        ] {
            assert!(check_campaign(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_row_limit_errors_count_as_too_large() {
        let e = |status: u16, body: &str| FetchError::Status {
            status,
            body: body.into(),
        };
        assert!(too_large(&e(
            409,
            r#"{"errorCode":"E7128","message":"Query result set exceeded max limit"}"#
        )));
        assert!(too_large(&e(409, "E7151")));
        assert!(!too_large(&e(409, "E7124")));
        assert!(!too_large(&e(500, "E7128")));
    }
}
