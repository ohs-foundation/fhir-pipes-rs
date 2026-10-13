//! `kiln dhis2 reports`: indicator values from DHIS2 analytics as
//! MeasureReports, one per indicator, org unit and period.
//!
//! DHIS2 computes every value itself: it adds up the numerator and the
//! denominator for the org unit and period, then divides, so values are
//! requested at each level and never summed here. `includeNumDen` returns
//! the numerator and denominator beside the value; `skipRounding` returns
//! the value unrounded. Period start and end dates come from the response's
//! own metadata, so every DHIS2 period type works without kiln knowing its
//! calendar rules.
//!
//! Org units are mapped to Locations through the identifier that
//! `kiln dhis2 locations` writes. A row whose org unit has no Location is
//! reported and skipped.
//!
//! An export can run to millions of values, so the work is planned in
//! units (one level, one chunk of indicators, one batch of periods of the
//! same kind, sized from the level's org unit count to stay under
//! `--max-rows`). Workers fetch units in parallel; each unit's totals and
//! breakdowns come back together, and its reports are written as soon as
//! every unit before it has been, so memory holds a few units at a time and
//! the file is the same at any `--concurrency`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::api::{Dhis2, ServerInfo};
use super::measures::{fetch_indicators, fhir_key, Indicator, Scoring, MEASURE_POPULATION_SYSTEM};
use super::{check_out, Instance, NdjsonWriter};
use crate::cli::Dhis2ReportsArgs;
use crate::error::{KilnError, Result};
use crate::extract::client::FetchError;
use crate::fhir::group::is_valid_id;
use crate::report::Report;
use crate::transform::write_report;

pub const PERIOD_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/dhis2-period";
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
    let file = std::fs::File::open(path).map_err(|e| KilnError::io(path, e))?;
    let mut out = HashMap::new();
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| KilnError::io(path, e))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)
            .map_err(|e| KilnError::Usage(format!("{}: line {}: {e}", path.display(), n + 1)))?;
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
            if ident.get("system").and_then(Value::as_str) == Some(system) {
                if let Some(uid) = ident.get("value").and_then(Value::as_str) {
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

pub fn parse_analytics(v: &Value, breakdown: Option<&str>) -> Result<Analytics> {
    let headers: Vec<&str> = v
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| KilnError::Environment("analytics response has no headers".into()))?
        .iter()
        .map(|h| h.get("name").and_then(Value::as_str).unwrap_or(""))
        .collect();
    let col = |name: &str| headers.iter().position(|h| *h == name);
    let need = |name: &str| {
        col(name).ok_or_else(|| {
            KilnError::Environment(format!("analytics response has no {name} column"))
        })
    };
    let (dx, pe, ou, value) = (need("dx")?, need("pe")?, need("ou")?, need("value")?);
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
            pe: cell(r, Some(pe)),
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
    Ok(Analytics {
        rows,
        periods,
        names,
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
    let mut r = json!({
        "resourceType": "MeasureReport",
        "id": id,
        "meta": {"tag": [instance.tag()]},
        "extension": [{"url": PERIOD_EXTENSION_URL, "valueString": row.pe}],
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

/// One unit of work: a chunk of indicators for a batch of periods at one
/// level. Its totals and every breakdown are fetched together, so its
/// reports are complete, and written, as soon as it is done.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub level: u32,
    pub periods: Vec<String>,
    pub indicators: Vec<String>,
}

/// Every unit of work, in the order its reports are written: level by
/// level, then indicator chunk, then period batch. `levels` pairs each level
/// with its org unit count. A batch holds as many periods of one kind as
/// keep indicators × periods × org units within `max_rows`, and at least
/// one. Breakdowns are not counted: they come back as separate requests.
pub fn plan(
    levels: &[(u32, u64)],
    periods: &[String],
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
                        level,
                        periods: batch.to_vec(),
                        indicators: indicators.to_vec(),
                    });
                }
            }
        }
    }
    units
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
            KilnError::Environment(format!(
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
    indicators: &'a [String],
    breakdown: Option<&'a str>,
}

impl Ask<'_> {
    fn label(&self) -> String {
        format!(
            "level {} {}{}",
            self.level,
            periods_label(self.periods),
            self.breakdown
                .map(|b| format!(" by {b}"))
                .unwrap_or_default()
        )
    }
}

/// A unit's analytics responses: the totals, then each breakdown's.
#[derive(Debug, Default)]
struct Fetched {
    totals: Vec<Analytics>,
    breakdowns: Vec<(String, Vec<Analytics>)>,
    requests: usize,
}

fn fetch_unit(
    api: &Dhis2,
    root: Option<&str>,
    breakdowns: &[String],
    unit: &Unit,
) -> Result<Fetched> {
    let mut f = Fetched::default();
    let ask = Ask {
        level: unit.level,
        root,
        periods: &unit.periods,
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
fn fetch(api: &Dhis2, ask: Ask, requests: &mut usize, out: &mut Vec<Analytics>) -> Result<()> {
    let mut query = vec![
        ("dimension", format!("dx:{}", ask.indicators.join(";"))),
        ("dimension", format!("pe:{}", ask.periods.join(";"))),
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
    ];
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
            return Err(KilnError::Usage(format!(
                "indicator {} at level {} for {} is over DHIS2's analytics row limit on its own; ask for shorter periods ({e})",
                ask.indicators[0], ask.level, ask.periods[0]
            )));
        }
        Err(FetchError::Status { status: 409, body }) => {
            return Err(KilnError::Usage(format!(
                "DHIS2 rejected the analytics query for level {} and period {}: {body}",
                ask.level,
                ask.periods.join(";")
            )));
        }
        Err(e) => return Err(api.error("analytics.json", e)),
    };
    out.push(parse_analytics(&body, ask.breakdown)?);
    Ok(())
}

/// Turns fetched units into written reports. It runs on one thread and
/// takes units in planned order, so the output and the run report are the
/// same at any `--concurrency`; it keeps running totals, not reports.
struct Writer<'a> {
    out: NdjsonWriter,
    instance: &'a Instance,
    info: &'a ServerInfo,
    indicators: &'a BTreeMap<String, Indicator>,
    mapping: &'a HashMap<String, Target>,
    report: Report,
    unmapped: HashSet<String>,
    locations: HashSet<String>,
    periods: HashSet<String>,
    values: usize,
    requests: usize,
    stratified: usize,
}

impl<'a> Writer<'a> {
    /// The indicator and Location of a row, or `None` after reporting why
    /// the row is skipped.
    fn target(&mut self, row: &AnalyticsRow) -> Option<(&'a Indicator, &'a Target)> {
        let indicators = self.indicators;
        let mapping = self.mapping;
        let Some(ind) = indicators.get(&row.dx) else {
            self.report.add(
                "unexpected_indicator",
                &row.dx,
                "DHIS2 returned an indicator that was not asked for; skipped",
            );
            return None;
        };
        let Some(location) = mapping.get(&row.ou) else {
            if self.unmapped.insert(row.ou.clone()) {
                self.report.add(
                    "unmapped_org_unit",
                    &row.ou,
                    "no Location carries this org unit; its values are skipped",
                );
            }
            return None;
        };
        Some((ind, location))
    }

    /// Writes one unit's reports, sorted by id, with its strata attached to
    /// its totals; returns how many values DHIS2 sent for it.
    fn unit(&mut self, f: Fetched) -> Result<usize> {
        let mut rows = 0usize;
        let mut out: BTreeMap<String, Value> = BTreeMap::new();
        for table in &f.totals {
            rows += table.rows.len();
            for row in &table.rows {
                let Some((ind, location)) = self.target(row) else {
                    continue;
                };
                let Some(period) = table.periods.get(&row.pe) else {
                    self.report.add(
                        "period_without_dates",
                        &row.pe,
                        "DHIS2 gave no start and end date for this period; row skipped",
                    );
                    continue;
                };
                if let Some(r) = measure_report(
                    row,
                    ind,
                    location,
                    period,
                    self.instance,
                    self.info,
                    &mut self.report,
                ) {
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
                    if let Some(st) =
                        stratum(row, ind, b, &table.names, self.instance, &mut self.report)
                    {
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
                self.report.add(
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
            self.stratified += 1;
        }

        for r in out.values() {
            if let Some(s) = r.pointer("/subject/reference").and_then(Value::as_str) {
                if !self.locations.contains(s) {
                    self.locations.insert(s.to_string());
                }
            }
            if let Some(pe) = r
                .pointer("/extension/0/valueString")
                .and_then(Value::as_str)
            {
                if !self.periods.contains(pe) {
                    self.periods.insert(pe.to_string());
                }
            }
            self.out.write(r)?;
        }
        self.values += rows;
        self.requests += f.requests;
        Ok(rows)
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
/// take as long as the ones done.
fn progress(done: usize, total: usize, unit: &Unit, values: usize, elapsed: Duration) -> String {
    let left = if done < total {
        let rest = elapsed.mul_f64((total - done) as f64 / done as f64);
        format!(", ~{} left", clock(rest, true))
    } else {
        String::new()
    };
    format!(
        "[{done}/{total}] level {} · {} · {} items: {} values · {} elapsed{left}",
        unit.level,
        periods_label(&unit.periods),
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

pub fn run_reports(args: &Dhis2ReportsArgs) -> Result<()> {
    check_out(&args.out)?;
    if args.chunk == 0 {
        return Err(KilnError::Usage("--chunk must be at least 1".into()));
    }
    if args.max_rows == 0 {
        return Err(KilnError::Usage("--max-rows must be at least 1".into()));
    }
    if args.concurrency == 0 {
        return Err(KilnError::Usage("--concurrency must be at least 1".into()));
    }
    if args.levels.contains(&0) {
        return Err(KilnError::Usage(
            "--level counts from 1, the top of the DHIS2 tree".into(),
        ));
    }
    let instance = Instance::new(&args.conn.instance)?;
    if !args.locations.is_file() {
        return Err(KilnError::Usage(format!(
            "--locations {}: no such file",
            args.locations.display()
        )));
    }
    let system = instance.orgunit_system();
    let mapping = load_mapping(&args.locations, &system)?;
    if mapping.is_empty() {
        return Err(KilnError::Usage(format!(
            "no Location in {} carries an identifier under {system}; run `kiln dhis2 locations --instance {}` and load its output first",
            args.locations.display(),
            instance.name
        )));
    }

    let api = Dhis2::connect(&args.conn)?;
    match &api.info.last_analytics {
        Some(t) => eprintln!("DHIS2 analytics tables last built {t}"),
        None => eprintln!("DHIS2 does not say when its analytics tables were last built"),
    }
    let indicators = fetch_indicators(&api, &args.select)?;
    let ids: Vec<String> = indicators.keys().cloned().collect();

    // Requests are sized from each level's org unit count, so several
    // periods can share one.
    let mut levels = Vec::new();
    for level in unique(&args.levels) {
        let n = org_unit_count(&api, level, args.root.as_deref())?;
        eprintln!("level {level}: {n} org units");
        levels.push((level, n));
    }
    let units = plan(
        &levels,
        &unique(&args.periods),
        &ids,
        args.chunk,
        args.max_rows,
    );
    let total = units.len();
    eprintln!(
        "{total} batches, {} analytics requests unless DHIS2 calls some too large",
        total * (1 + args.breakdowns.len())
    );

    let mut w = Writer {
        out: NdjsonWriter::create(&args.out)?,
        instance: &instance,
        info: &api.info,
        indicators: &indicators,
        mapping: &mapping,
        report: Report::default(),
        unmapped: HashSet::new(),
        locations: HashSet::new(),
        periods: HashSet::new(),
        values: 0,
        requests: 0,
        stratified: 0,
    };

    // Workers fetch units from a shared counter; this thread writes them in
    // planned order, holding back any that finish early. A worker does not
    // start a unit more than `window` ahead of the last one written, so
    // memory stays at a few units, not the whole export.
    let workers = args.concurrency.min(total).max(1);
    let window = workers * 2;
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let written = (Mutex::new(0usize), Condvar::new());
    let (tx, rx) = mpsc::sync_channel::<(usize, Result<Fetched>)>(window);
    let started = Instant::now();
    let mut failure: Option<KilnError> = None;
    std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (api, units, next, stop, written) = (&api, &units, &next, &stop, &written);
            s.spawn(move || loop {
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
                let fetched = fetch_unit(api, args.root.as_deref(), &args.breakdowns, &units[i]);
                if tx.send((i, fetched)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut pending: HashMap<usize, Fetched> = HashMap::new();
        let mut cursor = 0usize;
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
                match w.unit(f) {
                    Ok(values) => eprintln!(
                        "{}",
                        progress(cursor + 1, total, &units[cursor], values, started.elapsed())
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
        return Err(e);
    }

    let Writer {
        out,
        report,
        locations,
        periods,
        values,
        requests,
        stratified,
        ..
    } = w;
    let n = out.finish()?;
    println!(
        "Wrote {n} MeasureReports to {} ({} indicators, {} Locations, {} periods; {values} values from {requests} analytics requests)",
        args.out.display(),
        indicators.len(),
        locations.len(),
        periods.len()
    );
    if !args.breakdowns.is_empty() {
        println!(
            "{stratified} breakdowns attached ({} dimensions)",
            args.breakdowns.len()
        );
    }
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        write_report(path, &report)?;
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
        let instance = Instance::new("sl-demo").unwrap();
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
        let instance = Instance::new("sl-demo").unwrap();
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
        assert!(measure_report(
            &bad,
            &penta3(),
            &loc(),
            &("a".into(), "b".into()),
            &instance,
            &ServerInfo::default(),
            &mut report
        )
        .is_none());
        assert_eq!(report.count("bad_number"), 1);
    }

    #[test]
    fn a_cohort_reports_the_numerator_as_its_population() {
        let instance = Instance::new("sl-demo").unwrap();
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
        let instance = Instance::new("sl-demo").unwrap();
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
        let instance = Instance::new("sl-demo").unwrap();
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
        let units = plan(&[(2, 3)], &periods, &ids, 2, 12);
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
        let units = plan(&[(4, 100_000)], &periods[..2], &ids, 10, 20_000);
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
            &ids,
            10,
            20_000,
        );
        let per_level = |l: u32| units.iter().filter(|u| u.level == l).count();
        assert_eq!(
            [per_level(1), per_level(2), per_level(3), per_level(4)],
            [18, 18, 36, 258]
        );
        assert_eq!(units.len(), 330);
    }

    #[test]
    fn progress_reads_at_a_glance() {
        let unit = Unit {
            level: 4,
            periods: codes(&["20260421", "20260422", "20260427"]),
            indicators: codes(&["a"; 10]),
        };
        assert_eq!(
            progress(37, 412, &unit, 33_656, Duration::from_secs(252)),
            "[37/412] level 4 · 20260421…20260427 (3 periods) · 10 items: 33,656 values · 4m12s elapsed, ~42m left"
        );
        let one = Unit {
            periods: codes(&["2025"]),
            ..unit
        };
        assert_eq!(
            progress(2, 2, &one, 5, Duration::from_secs(3725)),
            "[2/2] level 4 · 2025 · 10 items: 5 values · 1h02m elapsed"
        );
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
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
