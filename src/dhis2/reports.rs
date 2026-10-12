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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{json, Value};

use super::api::{Dhis2, ServerInfo};
use super::measures::{fetch_indicators, fhir_key, Indicator, Scoring, MEASURE_POPULATION_SYSTEM};
use super::{check_out, write_ndjson, Instance};
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

struct Request {
    level: u32,
    period: String,
    indicators: Vec<String>,
    breakdown: Option<String>,
}

pub fn run_reports(args: &Dhis2ReportsArgs) -> Result<()> {
    check_out(&args.out)?;
    if args.chunk == 0 {
        return Err(KilnError::Usage("--chunk must be at least 1".into()));
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

    // Totals first, then each breakdown: a stratum is attached to its total's
    // report at the end.
    let mut queue: VecDeque<Request> = VecDeque::new();
    for breakdown in std::iter::once(None).chain(args.breakdowns.iter().map(|b| Some(b.clone()))) {
        for &level in &args.levels {
            for period in &args.periods {
                for chunk in ids.chunks(args.chunk) {
                    queue.push_back(Request {
                        level,
                        period: period.clone(),
                        indicators: chunk.to_vec(),
                        breakdown: breakdown.clone(),
                    });
                }
            }
        }
    }
    // (report id, breakdown) → (breakdown name, strata)
    let mut strata: BTreeMap<(String, String), (String, Vec<Value>)> = BTreeMap::new();

    let mut report = Report::default();
    let mut unmapped: HashSet<String> = HashSet::new();
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let mut requests = 0usize;
    let mut rows_seen = 0usize;
    while let Some(req) = queue.pop_front() {
        let mut query = vec![
            ("dimension", format!("dx:{}", req.indicators.join(";"))),
            ("dimension", format!("pe:{}", req.period)),
            ("dimension", format!("ou:LEVEL-{}", req.level)),
            ("includeNumDen", "true".into()),
            ("skipRounding", "true".into()),
            ("includeMetadataDetails", "true".into()),
            ("displayProperty", "NAME".into()),
        ];
        if let Some(b) = &req.breakdown {
            query.push(("dimension", b.clone()));
        }
        requests += 1;
        let body = match api.try_get("analytics.json", &query)? {
            Ok(v) => v,
            Err(e) if too_large(&e) && req.indicators.len() > 1 => {
                let half = req.indicators.len() / 2;
                eprintln!(
                    "level {} {}: result too large for DHIS2, splitting {} indicators in two",
                    req.level,
                    req.period,
                    req.indicators.len()
                );
                queue.push_front(Request {
                    level: req.level,
                    period: req.period.clone(),
                    indicators: req.indicators[half..].to_vec(),
                    breakdown: req.breakdown.clone(),
                });
                queue.push_front(Request {
                    level: req.level,
                    period: req.period,
                    indicators: req.indicators[..half].to_vec(),
                    breakdown: req.breakdown,
                });
                continue;
            }
            Err(e) if too_large(&e) => {
                return Err(KilnError::Usage(format!(
                    "indicator {} at level {} for {} is over DHIS2's analytics row limit on its own; ask for shorter periods ({e})",
                    req.indicators[0], req.level, req.period
                )));
            }
            Err(FetchError::Status { status: 409, body }) => {
                return Err(KilnError::Usage(format!(
                    "DHIS2 rejected the analytics query for level {} and period {}: {body}",
                    req.level, req.period
                )));
            }
            Err(e) => return Err(api.error("analytics.json", e)),
        };
        let table = parse_analytics(&body, req.breakdown.as_deref())?;
        eprintln!(
            "level {} {}{}: {} indicators, {} values",
            req.level,
            req.period,
            req.breakdown
                .as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            req.indicators.len(),
            table.rows.len()
        );
        rows_seen += table.rows.len();
        for row in &table.rows {
            let Some(ind) = indicators.get(&row.dx) else {
                report.add(
                    "unexpected_indicator",
                    &row.dx,
                    "DHIS2 returned an indicator that was not asked for; skipped",
                );
                continue;
            };
            let Some(location) = mapping.get(&row.ou) else {
                if unmapped.insert(row.ou.clone()) {
                    report.add(
                        "unmapped_org_unit",
                        &row.ou,
                        "no Location carries this org unit; its values are skipped",
                    );
                }
                continue;
            };
            if let Some(b) = &req.breakdown {
                if let Some(st) = stratum(row, ind, b, &table.names, &instance, &mut report) {
                    let name = table.names.get(b).cloned().unwrap_or_else(|| b.clone());
                    strata
                        .entry((report_id(&instance, row), b.clone()))
                        .or_insert_with(|| (name, Vec::new()))
                        .1
                        .push(st);
                }
                continue;
            }
            let Some(period) = table.periods.get(&row.pe) else {
                report.add(
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
                &instance,
                &api.info,
                &mut report,
            ) {
                out.insert(r["id"].as_str().unwrap_or_default().to_string(), r);
            }
        }
    }

    let mut stratified = 0usize;
    for ((rid, b), (name, mut list)) in strata {
        let Some(r) = out.get_mut(&rid) else {
            report.add(
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
            "code": [{"coding": [{"system": instance.dimension_system(None), "code": b, "display": name}], "text": name}],
            "stratum": list,
        });
        match r["group"][0]
            .get_mut("stratifier")
            .and_then(Value::as_array_mut)
        {
            Some(existing) => existing.push(stratifier),
            None => r["group"][0]["stratifier"] = json!([stratifier]),
        }
        stratified += 1;
    }
    let n = write_ndjson(&args.out, out.values())?;
    let org_units: HashSet<&str> = out
        .values()
        .filter_map(|r| r.pointer("/subject/reference")?.as_str())
        .collect();
    let periods: HashSet<&str> = out
        .values()
        .filter_map(|r| r.pointer("/extension/0/valueString")?.as_str())
        .collect();
    println!(
        "Wrote {n} MeasureReports to {} ({} indicators, {} Locations, {} periods; {rows_seen} values from {requests} analytics requests)",
        args.out.display(),
        indicators.len(),
        org_units.len(),
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
