//! `DHIS2Value` records from analytics rows: the Location an org unit maps
//! to, the counts and score of a row, and the strata of a breakdown.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::cli::ReportSource;
use crate::dhis2::Instance;
use crate::dhis2::api::ServerInfo;
use crate::dhis2::locations::location_id;
use crate::dhis2::maps::{ORG_UNIT_MODEL, VALUE_MODEL, record_type};
use crate::dhis2::measures::{Indicator, Scoring, fhir_key};
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::ndjson::NdjsonReader;
use crate::report::Report;

use super::analytics::AnalyticsRow;
use super::periods::period_value;

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

/// Org unit UID → its Location, and org unit UID → its parent's UID.
pub type OrgUnits = (HashMap<String, Target>, HashMap<String, Option<String>>);

/// The org units of `instance` in the `DHIS2OrgUnit` records at `path`, the
/// file `dhis2 extract locations` writes: org unit UID → the Location its
/// record becomes, and org unit UID → its parent's UID.
///
/// The Location is the one the record's map writes: id `<idPrefix>-<uid>`,
/// named as the org unit. These are the same facts `load_mapping` and
/// `load_parents` read from the Locations, so a `DHIS2Value` record comes
/// out the same from either. As there:
///
/// - a record whose Location id would not be a valid FHIR id is left out
///   (`dhis2 extract locations` writes none, nor a Location);
/// - records of another instance are left out (their Locations carry the
///   UID under another instance's system);
/// - a parent is kept only when its own record is in the file (a Location's
///   `partOf` is followed only to a Location in the file).
pub fn load_org_unit_records(path: &Path, instance: &str) -> Result<OrgUnits> {
    let record_type = record_type(ORG_UNIT_MODEL);
    let mut targets = HashMap::new();
    let mut parent_of: Vec<(String, Option<String>)> = Vec::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let v: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        let field = |k: &str| v.get(k).and_then(Value::as_str);
        if field("resourceType") != Some(record_type) || field("instance") != Some(instance) {
            continue;
        }
        let (Some(uid), Some(prefix)) = (field("uid"), field("idPrefix")) else {
            continue;
        };
        let id = location_id(prefix, uid);
        if !is_valid_id(&id) {
            continue;
        }
        targets.insert(
            uid.to_string(),
            Target {
                id,
                name: field("name").map(str::to_string),
            },
        );
        parent_of.push((uid.to_string(), field("parentUid").map(str::to_string)));
    }
    let parents = parent_of
        .into_iter()
        .map(|(uid, parent)| (uid, parent.filter(|p| targets.contains_key(p))))
        .collect();
    Ok((targets, parents))
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

pub fn report_id(instance: &Instance, row: &AnalyticsRow) -> String {
    format!(
        "{}-{}-{}-{}",
        instance.name,
        fhir_key(&row.dx),
        row.ou,
        row.pe
    )
}

/// The row as a `DHIS2Value` record (examples/import/icr-dhis2/models), the
/// input of the package's map or the line `dhis2 extract` writes, or `None` after
/// reporting why there is none. Breakdowns are added to its `stratifier`
/// afterwards.
#[allow(clippy::too_many_arguments)]
pub fn value_record(
    row: &AnalyticsRow,
    ind: &Indicator,
    location: &Target,
    period: &(String, String),
    instance: &Instance,
    info: &ServerInfo,
    campaign: Option<&str>,
    source: ReportSource,
    report: &mut Report,
) -> Option<Value> {
    let id = valid_report_id(instance, row, report)?;
    let figures = figures(row, ind, &id, report)?;
    let mut r = json!({
        "resourceType": record_type(VALUE_MODEL),
        "instance": instance.name,
        "id": id,
        "indicatorKey": ind.fhir_key(),
        "locationId": location.id,
    });
    if let Some(name) = &location.name {
        r["locationName"] = json!(name);
    }
    r["period"] = json!(period_value(&row.pe));
    r["periodStart"] = json!(period.0);
    r["periodEnd"] = json!(period.1);
    if let Some(d) = info
        .last_analytics
        .as_deref()
        .and_then(|t| info.timestamp(t))
    {
        r["date"] = json!(d);
    }
    if let Some(name) = &info.system_name {
        r["reporter"] = json!(name);
    }
    if let Some(c) = campaign {
        r["campaign"] = json!(c);
    }
    r["source"] = json!(source.coding().0);
    figures.write(&mut r);
    Some(r)
}

/// The report id of a row, or `None`, reported, when it is not a valid FHIR id.
fn valid_report_id(instance: &Instance, row: &AnalyticsRow, report: &mut Report) -> Option<String> {
    let id = report_id(instance, row);
    if !is_valid_id(&id) {
        report.add("invalid_id", &id, "not a valid FHIR id; row skipped");
        return None;
    }
    Some(id)
}

/// The counts and score of one analytics row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figures {
    /// The count of a plain number (a cohort).
    pub initial: Option<i64>,
    pub numerator: Option<i64>,
    pub denominator: Option<i64>,
    /// The DHIS2 value divided by the indicator's factor.
    pub score: f64,
}

impl Figures {
    /// The counts and score as fields of a `DHIS2Value` record or stratum.
    fn write(&self, record: &mut Value) {
        if let Some(n) = self.initial {
            record["initialPopulation"] = json!(n);
        }
        if let Some(n) = self.numerator {
            record["numerator"] = json!(n);
        }
        if let Some(n) = self.denominator {
            record["denominator"] = json!(n);
        }
        record["score"] = json!(self.score);
    }
}

/// The populations and score of one analytics row (a whole report, or one
/// stratum of it); `None` after reporting why there are none. `key` names
/// the report in the run report.
pub fn figures(
    row: &AnalyticsRow,
    ind: &Indicator,
    id: &str,
    report: &mut Report,
) -> Option<Figures> {
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
    let mut figures = Figures {
        initial: None,
        numerator: None,
        denominator: None,
        score,
    };
    match ind.scoring() {
        // A data element row has no numerator column: its value is the count.
        Scoring::Cohort => {
            figures.initial = count(
                if row.numerator.trim().is_empty() {
                    &row.value
                } else {
                    &row.numerator
                },
                id,
                "count",
                report,
            );
        }
        _ => {
            figures.numerator = count(&row.numerator, id, "numerator", report);
            figures.denominator = count(&row.denominator, id, "denominator", report);
        }
    }
    Some(figures)
}

/// One stratum of a `DHIS2Value` record: a breakdown option with its own
/// counts and score.
pub fn stratum_record(
    row: &AnalyticsRow,
    ind: &Indicator,
    breakdown: &str,
    names: &HashMap<String, String>,
    instance: &Instance,
    report: &mut Report,
) -> Option<Value> {
    let option = row.option.as_deref()?;
    let key = format!("{}/{breakdown}:{option}", report_id(instance, row));
    let figures = figures(row, ind, &key, report)?;
    let name = names.get(option).map(String::as_str).unwrap_or(option);
    let mut s = json!({"option": option, "name": name});
    figures.write(&mut s);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhis2::reports::analytics::{parse_analytics, tests::analytics};

    fn loc() -> Target {
        Target {
            id: "loc".into(),
            name: None,
        }
    }

    /// The MeasureReport the example package writes for one row, or `None`
    /// when the row gives no record.
    #[allow(clippy::too_many_arguments)]
    fn measure_report(
        row: &AnalyticsRow,
        ind: &Indicator,
        location: &Target,
        period: &(String, String),
        instance: &Instance,
        info: &ServerInfo,
        campaign: Option<&str>,
        source: ReportSource,
        report: &mut Report,
    ) -> Option<Value> {
        let record = value_record(
            row, ind, location, period, instance, info, campaign, source, report,
        )?;
        Some(map(&record, report))
    }

    fn map(record: &Value, report: &mut Report) -> Value {
        let maps = crate::dhis2::maps::icr_package(VALUE_MODEL);
        let mut out = maps
            .run(record, record["id"].as_str().unwrap(), report)
            .unwrap();
        assert_eq!(out.len(), 1);
        out.remove(0)
    }

    /// The stratum the example package writes for a breakdown row, attached
    /// to a total from the same row.
    fn stratum(
        row: &AnalyticsRow,
        ind: &Indicator,
        breakdown: &str,
        names: &HashMap<String, String>,
        instance: &Instance,
        report: &mut Report,
    ) -> Option<Value> {
        let st = stratum_record(row, ind, breakdown, names, instance, report)?;
        let mut total = value_record(
            row,
            ind,
            &loc(),
            &("2025-01-01".into(), "2025-12-31".into()),
            instance,
            &ServerInfo::default(),
            None,
            ReportSource::Analytics,
            report,
        )?;
        let name = names
            .get(breakdown)
            .cloned()
            .unwrap_or(breakdown.to_string());
        total["stratifier"] = json!([{"dimension": breakdown, "name": name, "stratum": [st]}]);
        let r = map(&total, report);
        Some(r["group"][0]["stratifier"][0]["stratum"][0].clone())
    }

    fn penta3() -> Indicator {
        Indicator::parse(&json!({"id":"tUIlpyeeX9N","name":"Penta 3 Coverage <1y",
            "indicatorType":{"factor":100},"numerator":"#{a}","denominator":"#{b}"}))
        .unwrap()
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
            None,
            ReportSource::Analytics,
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
            None,
            ReportSource::Analytics,
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
                None,
                ReportSource::Analytics,
                &mut report
            )
            .is_none()
        );
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
            None,
            ReportSource::Analytics,
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
            None,
            ReportSource::Analytics,
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
            "https://icr.healthcampaigns.org/CodeSystem/dhis2/sl-demo/dimension/fMZEcRHuamy"
        );
        assert_eq!(s["population"][0]["count"], 50);
        assert_eq!(s["measureScore"]["value"], 0.25);
        assert!(parse_analytics(&v, Some("J5jldMd8OHv")).is_err());
    }

    #[test]
    /// The records are the package's test case values, and the package maps
    /// them to the case's expected MeasureReports: what the built-in mapping
    /// wrote before it was removed.
    fn the_records_are_the_package_case_values() {
        let maps = crate::dhis2::maps::icr_package(VALUE_MODEL);
        let instance = Instance::new("sl-demo").unwrap();
        let info = ServerInfo {
            system_name: Some("DHIS 2 Demo - Sierra Leone".into()),
            last_analytics: Some("2026-10-12T02:07:00.052".into()),
            utc: true,
            ..Default::default()
        };
        let bare = ServerInfo::default();
        let row = |value: &str, numerator: &str, denominator: &str| AnalyticsRow {
            dx: "tUIlpyeeX9N".into(),
            pe: "2025Q1".into(),
            ou: "O6uvpzGd5pu".into(),
            value: value.into(),
            numerator: numerator.into(),
            denominator: denominator.into(),
            option: None,
        };
        let named = Target {
            id: "sle-O6uvpzGd5pu".into(),
            name: Some("Bo".into()),
        };
        let period = ("2025-01-01".to_string(), "2025-03-31".to_string());
        let count = Indicator::parse(
            &json!({"id":"tUIlpyeeX9N","indicatorType":{"factor":1},"numerator":"#{a}","denominator":"1"}),
        )
        .unwrap();
        let element =
            Indicator::from_data_element(&json!({"id": "tUIlpyeeX9N", "name": "BCG doses given"}))
                .unwrap();
        let rate = Indicator::from_data_set(&json!({"id": "tUIlpyeeX9N", "name": "RCH"})).unwrap();
        let analytics = ReportSource::Analytics;
        let values = ReportSource::DataValues;
        // (row, item, Location, server, campaign, source)
        let cases = [
            // A percent with both counts, a named Location and a campaign.
            (
                row("45.18", "4518", "10000"),
                penta3(),
                &named,
                &info,
                Some("CarePlan/c1"),
                analytics,
            ),
            // Counts that are not whole numbers, and one DHIS2 left blank.
            (
                row("12.5", "1.4", ""),
                penta3(),
                &loc(),
                &bare,
                None,
                values,
            ),
            // A plain number: its numerator is the count.
            (
                row("88039.0", "88039.0", "1.0"),
                count,
                &named,
                &info,
                None,
                analytics,
            ),
            // A data element: no numerator column, the value is the count.
            (row("7", "", ""), element, &loc(), &info, None, values),
            // A reporting rate: a key with a hyphen in the id and the Measure.
            (
                row("93.3", "28", "30"),
                rate,
                &named,
                &info,
                None,
                analytics,
            ),
        ];
        let mut all_records = Vec::new();
        let mut all_mapped = Vec::new();
        for (row, ind, location, info, campaign, source) in &cases {
            let mut row = row.clone();
            row.dx = ind.id.clone();
            let mut mapped_report = Report::default();
            let mut record = value_record(
                &row,
                ind,
                location,
                &period,
                &instance,
                info,
                *campaign,
                *source,
                &mut mapped_report,
            )
            .unwrap();

            // Two breakdowns, the first with two options, attached as the writer does.
            let names: HashMap<String, String> = [
                ("fMZEcRHuamy", "Location Fixed/Outreach"),
                ("qkPbeWaFsnU", "Fixed"),
                ("wbrDrL2aYEc", "Outreach"),
                ("J5jldMd8OHv", "Facility Type"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
            // The option of the second breakdown has no name: its UID stands in.
            let breakdowns = [
                (
                    "fMZEcRHuamy",
                    vec![
                        ("qkPbeWaFsnU", "25.0", "50.0", "200.0"),
                        ("wbrDrL2aYEc", "10", "", "3"),
                    ],
                ),
                ("J5jldMd8OHv", vec![("CXw2yu5fodb", "1.5", "3", "2")]),
            ];
            for (dimension, options) in &breakdowns {
                let mut records = Vec::new();
                for (option, value, numerator, denominator) in options {
                    let mut r = row.clone();
                    r.option = Some(option.to_string());
                    r.value = value.to_string();
                    r.numerator = numerator.to_string();
                    r.denominator = denominator.to_string();
                    records.extend(stratum_record(
                        &r,
                        ind,
                        dimension,
                        &names,
                        &instance,
                        &mut mapped_report,
                    ));
                }
                let name = &names[*dimension];
                let stratifier = json!({"dimension": dimension, "name": name, "stratum": records});
                match record.get_mut("stratifier").and_then(Value::as_array_mut) {
                    Some(list) => list.push(stratifier),
                    None => record["stratifier"] = json!([stratifier]),
                }
            }

            let id = record["id"].as_str().unwrap().to_string();
            let mut mapped = maps.run(&record, &id, &mut mapped_report).unwrap();
            assert_eq!(mapped.len(), 1, "{id}");
            all_records.push(record);
            all_mapped.push(mapped.remove(0));
        }
        crate::dhis2::maps::case::records("values", &all_records);
        crate::dhis2::maps::case::expected("values", &all_mapped);
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
    fn org_unit_records_give_the_locations_and_parents_the_locations_would() {
        let dir = tempfile::tempdir().unwrap();
        let text = concat!(
            r#"{"resourceType":"DHIS2OrgUnit","instance":"sl-demo","idPrefix":"sle","uid":"a","name":"A"}"#,
            "\n",
            r#"{"resourceType":"DHIS2OrgUnit","instance":"sl-demo","idPrefix":"sle","uid":"b","parentUid":"a"}"#,
            "\n",
            // Its parent has no record: no parent, as no partOf is followed.
            r#"{"resourceType":"DHIS2OrgUnit","instance":"sl-demo","idPrefix":"sle","uid":"c","parentUid":"gone"}"#,
            "\n",
            // Another instance, and an id that is not a valid FHIR id.
            r#"{"resourceType":"DHIS2OrgUnit","instance":"other","idPrefix":"o","uid":"d"}"#,
            "\n",
            r#"{"resourceType":"DHIS2OrgUnit","instance":"sl-demo","idPrefix":"sle","uid":"e_f"}"#,
            "\n",
            r#"{"resourceType":"Location","id":"sle-g","identifier":[{"system":"S","value":"g"}]}"#,
            "\n",
        );
        let plain = dir.path().join("DHIS2OrgUnit.ndjson");
        std::fs::write(&plain, text).unwrap();
        let gz = dir.path().join("DHIS2OrgUnit.ndjson.gz");
        let mut e = flate2::write::GzEncoder::new(
            std::fs::File::create(&gz).unwrap(),
            flate2::Compression::default(),
        );
        std::io::Write::write_all(&mut e, text.as_bytes()).unwrap();
        e.finish().unwrap();
        for path in [&plain, &gz] {
            let (targets, parents) = load_org_unit_records(path, "sl-demo").unwrap();
            let mut uids: Vec<&str> = targets.keys().map(String::as_str).collect();
            uids.sort();
            assert_eq!(uids, ["a", "b", "c"]);
            assert_eq!(
                targets["a"],
                Target {
                    id: "sle-a".into(),
                    name: Some("A".into())
                }
            );
            assert_eq!(targets["b"].name, None);
            assert_eq!(parents["a"], None);
            assert_eq!(parents["b"].as_deref(), Some("a"));
            assert_eq!(parents["c"], None);
        }
    }

    #[test]
    fn a_range_report_carries_the_range_as_its_period() {
        let instance = Instance::new("bgd-epi-2026").unwrap();
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
            None,
            ReportSource::Analytics,
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
    fn the_campaign_and_the_source_follow_the_period_extension() {
        let instance = Instance::new("sl-demo").unwrap();
        let row = AnalyticsRow {
            dx: "tUIlpyeeX9N".into(),
            pe: "2025".into(),
            ou: "ou".into(),
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
            &("2025-01-01".into(), "2025-12-31".into()),
            &instance,
            &ServerInfo::default(),
            Some("CarePlan/bgd-mr-sia-2026"),
            ReportSource::DataValues,
            &mut report,
        )
        .unwrap();
        assert_eq!(
            r["extension"],
            json!([
                {"url": "https://icr.healthcampaigns.org/StructureDefinition/dhis2-period",
                 "valueString": "2025"},
                {"url": "https://icr.healthcampaigns.org/StructureDefinition/campaign",
                 "valueReference": {"reference": "CarePlan/bgd-mr-sia-2026"}},
                {"url": "https://icr.healthcampaigns.org/StructureDefinition/report-source",
                 "valueCodeableConcept": {"coding": [{
                     "system": "https://icr.healthcampaigns.org/CodeSystem/icr-report-source-cs",
                     "code": "dhis2-data-values", "display": "DHIS2 data values"}]}}
            ])
        );
        // The keys keep the order they were written in before.
        let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "resourceType",
                "id",
                "meta",
                "extension",
                "status",
                "type",
                "measure",
                "subject",
                "period",
                "group"
            ]
        );
    }
}
