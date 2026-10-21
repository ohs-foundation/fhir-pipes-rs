//! MeasureReports from analytics rows: the Location an org unit maps to,
//! the populations and score of a row, and the strata of a breakdown.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::dhis2::Instance;
use crate::dhis2::api::ServerInfo;
use crate::dhis2::measures::{Indicator, MEASURE_POPULATION_SYSTEM, Scoring, fhir_key};
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

    fn penta3() -> Indicator {
        Indicator::parse(&json!({"id":"tUIlpyeeX9N","name":"Penta 3 Coverage <1y",
            "indicatorType":{"factor":100},"numerator":"#{a}","denominator":"#{b}"}))
        .unwrap()
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
}
