//! `fhir-pipes dhis2 measures` and `dhis2 extract measures`: DHIS2
//! indicators, data elements and data set reporting rates as
//! `DHIS2Indicator` records, and with a package as Measures.
//!
//! An indicator is numerator ÷ denominator × factor. A percent (factor 100)
//! is scored `proportion`, any other factor `ratio`, and a plain number
//! (denominator `1`) `cohort` with a single population. The record carries
//! the expressions, the factor and the annualized flag; the score of a
//! `DHIS2Value` is the DHIS2 value divided by this factor (45.18 % → 0.4518).
//!
//! A data element (an aggregate count such as "BCG doses given") is scored
//! `cohort` too: its value is the total over every disaggregation, the same
//! number DHIS2 shows for `#{uid}`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::Instance;
use super::api::{Dhis2, ServerInfo, in_filter};
use super::maps::{INDICATOR_MODEL, RecordMap, Writes, record_type};
use crate::cli::{Dhis2MeasuresArgs, IndicatorSelection};
use fhir_pipes::error::{PipesError, Result};
use fhir_pipes::fhir::ndjson::write_ndjson;
use fhir_pipes::fs::check_out;
use fhir_pipes::report::{Report, write_report};

const INDICATOR_FIELDS: &str = "id,code,name,description,numerator,numeratorDescription,\
denominator,denominatorDescription,decimals,annualized,lastUpdated,\
indicatorType[name,factor,number],indicatorGroups[id,name]";
const DATA_ELEMENT_FIELDS: &str =
    "id,code,name,description,aggregationType,lastUpdated,dataElementGroups[id,name,groupSets[id]]";
const DATA_SET_FIELDS: &str = "id,code,name,description,periodType,lastUpdated";

/// What a Measure was made from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Indicator,
    DataElement,
    /// A data set's reporting rate: actual ÷ expected reports, in percent.
    ReportingRate,
}

/// An indicator group or data element group the item belongs to, with the
/// group sets that group is in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Group {
    pub id: Option<String>,
    pub name: String,
    pub sets: Vec<String>,
}

/// The groups of an indicator or data element, sorted by id then name, and
/// each group's sets sorted: DHIS2 returns both in an order that can change
/// between requests, and the order here is the order of `Measure.topic`.
fn groups(v: &Value, key: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = v
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let mut sets: Vec<String> = g
                .get("groupSets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.get("id")?.as_str().map(str::to_string))
                .collect();
            sets.sort();
            Some(Group {
                id: g.get("id").and_then(Value::as_str).map(str::to_string),
                name: g.get("name")?.as_str()?.to_string(),
                sets,
            })
        })
        .collect();
    groups.sort_by(|a, b| (&a.id, &a.name).cmp(&(&b.id, &b.name)));
    groups
}

#[derive(Debug, Clone, Default)]
pub struct Indicator {
    pub id: String,
    pub code: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub numerator: String,
    pub numerator_description: Option<String>,
    pub denominator: String,
    pub denominator_description: Option<String>,
    pub decimals: Option<i64>,
    pub annualized: bool,
    pub factor: f64,
    pub type_name: Option<String>,
    pub groups: Vec<Group>,
    pub last_updated: Option<String>,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scoring {
    Proportion,
    Ratio,
    Cohort,
}

impl Scoring {
    pub fn code(self) -> &'static str {
        match self {
            Scoring::Proportion => "proportion",
            Scoring::Ratio => "ratio",
            Scoring::Cohort => "cohort",
        }
    }
}

impl Indicator {
    pub fn parse(v: &Value) -> Option<Indicator> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let id = s("id")?;
        Some(Indicator {
            name: s("name").unwrap_or_else(|| id.clone()),
            code: s("code"),
            description: s("description"),
            numerator: s("numerator").unwrap_or_default(),
            numerator_description: s("numeratorDescription"),
            denominator: s("denominator").unwrap_or_else(|| "1".into()),
            denominator_description: s("denominatorDescription"),
            decimals: v.get("decimals").and_then(Value::as_i64),
            annualized: v
                .get("annualized")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            factor: v
                .pointer("/indicatorType/factor")
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite() && *f != 0.0)
                .unwrap_or(1.0),
            type_name: v
                .pointer("/indicatorType/name")
                .and_then(Value::as_str)
                .map(str::to_string),
            groups: groups(v, "indicatorGroups"),
            last_updated: s("lastUpdated"),
            kind: Kind::Indicator,
            id,
        })
    }

    /// A data element as a count: `#{uid}` over 1, factor 1.
    pub fn from_data_element(v: &Value) -> Option<Indicator> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let id = s("id")?;
        Some(Indicator {
            name: s("name").unwrap_or_else(|| id.clone()),
            code: s("code"),
            description: s("description"),
            numerator: format!("#{{{id}}}"),
            denominator: "1".into(),
            factor: 1.0,
            type_name: Some(match s("aggregationType") {
                Some(a) => format!("Data element ({a})"),
                None => "Data element".into(),
            }),
            groups: groups(v, "dataElementGroups"),
            last_updated: s("lastUpdated"),
            kind: Kind::DataElement,
            id,
            ..Default::default()
        })
    }

    /// A data set's reporting rate, as DHIS2 names and computes it: actual ÷
    /// expected reports × 100. Its analytics item is `<uid>.REPORTING_RATE`.
    pub fn from_data_set(v: &Value) -> Option<Indicator> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let uid = s("id")?;
        Some(Indicator {
            name: format!(
                "{} - Reporting rate",
                s("name").unwrap_or_else(|| uid.clone())
            ),
            code: s("code"),
            description: s("description"),
            numerator: format!("R{{{uid}.ACTUAL_REPORTS}}"),
            numerator_description: Some("Actual reports".into()),
            denominator: format!("R{{{uid}.EXPECTED_REPORTS}}"),
            denominator_description: Some("Expected reports".into()),
            factor: 100.0,
            type_name: Some(match s("periodType") {
                Some(p) => format!("Reporting rate ({p})"),
                None => "Reporting rate".into(),
            }),
            last_updated: s("lastUpdated"),
            kind: Kind::ReportingRate,
            id: format!("{uid}.REPORTING_RATE"),
            ..Default::default()
        })
    }

    /// The item as it appears in FHIR ids and canonical URLs. DHIS2 analytics
    /// items can hold `_` (`<uid>.REPORTING_RATE`), which a FHIR id cannot.
    pub fn fhir_key(&self) -> String {
        fhir_key(&self.id)
    }

    /// The DHIS2 uid the item is identified by (a reporting rate's data set).
    pub fn uid(&self) -> &str {
        self.id.split('.').next().unwrap_or(&self.id)
    }

    pub fn scoring(&self) -> Scoring {
        if self.denominator.trim() == "1" {
            Scoring::Cohort
        } else if self.factor == 100.0 {
            Scoring::Proportion
        } else {
            Scoring::Ratio
        }
    }
}

/// The item as a `DHIS2Indicator` record (examples/import/icr-dhis2/models):
/// the input of the package's map, or the line `dhis2 extract` writes.
pub fn record(ind: &Indicator, instance: &Instance, info: &ServerInfo) -> Value {
    let mut r = json!({
        "resourceType": record_type(INDICATOR_MODEL),
        "instance": instance.name,
        "kind": match ind.kind {
            Kind::Indicator => "indicator",
            Kind::DataElement => "data-element",
            Kind::ReportingRate => "reporting-rate",
        },
        "uid": ind.uid(),
        "key": ind.fhir_key(),
    });
    let mut set = |key: &str, value: Option<Value>| {
        if let Some(v) = value {
            r[key] = v;
        }
    };
    set("code", ind.code.as_deref().map(Value::from));
    set("name", Some(json!(ind.name)));
    set("description", ind.description.as_deref().map(Value::from));
    set("numerator", Some(json!(ind.numerator)));
    set(
        "numeratorDescription",
        ind.numerator_description.as_deref().map(Value::from),
    );
    set("denominator", Some(json!(ind.denominator)));
    set(
        "denominatorDescription",
        ind.denominator_description.as_deref().map(Value::from),
    );
    set("factor", Some(json!(ind.factor)));
    set("typeName", ind.type_name.as_deref().map(Value::from));
    set("annualized", Some(json!(ind.annualized)));
    set("decimals", ind.decimals.map(Value::from));
    set(
        "date",
        ind.last_updated
            .as_deref()
            .and_then(|t| info.timestamp(t))
            .map(Value::from),
    );
    set("publisher", info.system_name.as_deref().map(Value::from));
    if !ind.groups.is_empty() {
        let groups: Vec<Value> = ind
            .groups
            .iter()
            .map(|g| {
                let mut group = json!({});
                if let Some(id) = &g.id {
                    group["uid"] = json!(id);
                }
                group["name"] = json!(g.name);
                if !g.sets.is_empty() {
                    group["set"] = json!(g.sets);
                }
                group
            })
            .collect();
        set("group", Some(Value::Array(groups)));
    }
    r
}

/// A DHIS2 analytics item (`uid`, `uid.REPORTING_RATE`) as a FHIR id part.
pub fn fhir_key(dx: &str) -> String {
    dx.replace('_', "-")
}

/// One kind of DHIS2 metadata a Measure can be made from, and where to find it.
struct Source {
    what: &'static str,
    endpoint: &'static str,
    key: &'static str,
    group_endpoint: &'static str,
    group_key: &'static str,
    group_field: &'static str,
    fields: &'static str,
    parse: fn(&Value) -> Option<Indicator>,
}

const INDICATORS: Source = Source {
    what: "indicator",
    endpoint: "indicators.json",
    key: "indicators",
    group_endpoint: "indicatorGroups.json",
    group_key: "indicatorGroups",
    group_field: "indicatorGroups.id",
    fields: INDICATOR_FIELDS,
    parse: Indicator::parse,
};

const DATA_ELEMENTS: Source = Source {
    what: "data element",
    endpoint: "dataElements.json",
    key: "dataElements",
    group_endpoint: "dataElementGroups.json",
    group_key: "dataElementGroups",
    group_field: "dataElementGroups.id",
    fields: DATA_ELEMENT_FIELDS,
    parse: Indicator::from_data_element,
};

/// For data elements, the filter that keeps aggregate ones: a tracker data
/// element has no aggregate value to report.
fn aggregate_only(src: &Source) -> Option<(&'static str, String)> {
    (src.key == "dataElements").then(|| ("filter", "domainType:eq:AGGREGATE".to_string()))
}

fn fetch_source(
    api: &Dhis2,
    src: &Source,
    groups: &[String],
    ids: &[String],
    found: &mut BTreeMap<String, Indicator>,
) -> Result<()> {
    let mut add = |items: &Value| {
        for v in items
            .get(src.key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(ind) = (src.parse)(v) {
                found.insert(ind.id.clone(), ind);
            }
        }
    };
    if !groups.is_empty() {
        let listed = api.get(
            src.group_endpoint,
            &[
                ("paging", "false".into()),
                ("fields", "id".into()),
                ("filter", in_filter("id", groups)),
            ],
        )?;
        let have: BTreeSet<&str> = listed
            .get(src.group_key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|g| g.get("id")?.as_str())
            .collect();
        let missing: Vec<&str> = groups
            .iter()
            .map(String::as_str)
            .filter(|g| !have.contains(g))
            .collect();
        if !missing.is_empty() {
            return Err(PipesError::Usage(format!(
                "DHIS2 has no {} group {}",
                src.what,
                missing.join(", ")
            )));
        }
        let mut query = vec![
            ("paging", "false".to_string()),
            ("fields", src.fields.to_string()),
            ("filter", in_filter(src.group_field, groups)),
        ];
        query.extend(aggregate_only(src));
        add(&api.get(src.endpoint, &query)?);
    }
    if !ids.is_empty() {
        let mut query = vec![
            ("paging", "false".to_string()),
            ("fields", src.fields.to_string()),
            ("filter", in_filter("id", ids)),
        ];
        query.extend(aggregate_only(src));
        add(&api.get(src.endpoint, &query)?);
        let missing: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|i| !found.contains_key(*i))
            .collect();
        if !missing.is_empty() {
            let what = if aggregate_only(src).is_some() {
                format!("aggregate {}", src.what)
            } else {
                src.what.to_string()
            };
            return Err(PipesError::Usage(format!(
                "DHIS2 has no {what} {}",
                missing.join(", ")
            )));
        }
    }
    Ok(())
}

/// The selected indicators and data elements, by id. Asking for a group or
/// an item DHIS2 does not have is a usage error rather than a silently
/// smaller run.
pub fn fetch_indicators(
    api: &Dhis2,
    select: &IndicatorSelection,
) -> Result<BTreeMap<String, Indicator>> {
    let mut found: BTreeMap<String, Indicator> = BTreeMap::new();
    fetch_source(
        api,
        &INDICATORS,
        &select.groups,
        &select.indicators,
        &mut found,
    )?;
    fetch_source(
        api,
        &DATA_ELEMENTS,
        &select.data_element_groups,
        &select.data_elements,
        &mut found,
    )?;
    if !select.data_element_group_sets.is_empty() {
        let sets = api.get(
            "dataElementGroupSets.json",
            &[
                ("paging", "false".into()),
                ("fields", "id,dataElementGroups[id]".into()),
                ("filter", in_filter("id", &select.data_element_group_sets)),
            ],
        )?;
        let sets = sets
            .get("dataElementGroupSets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let missing: Vec<&str> = select
            .data_element_group_sets
            .iter()
            .map(String::as_str)
            .filter(|u| {
                !sets
                    .iter()
                    .any(|s| s.get("id").and_then(Value::as_str) == Some(*u))
            })
            .collect();
        if !missing.is_empty() {
            return Err(PipesError::Usage(format!(
                "DHIS2 has no data element group set {}",
                missing.join(", ")
            )));
        }
        let groups: Vec<String> = sets
            .iter()
            .flat_map(|s| {
                s.get("dataElementGroups")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|g| g.get("id")?.as_str().map(str::to_string))
            .collect();
        if !groups.is_empty() {
            fetch_source(api, &DATA_ELEMENTS, &groups, &[], &mut found)?;
        }
    }
    if !select.data_sets.is_empty() {
        let listed = api.get(
            "dataSets.json",
            &[
                ("paging", "false".into()),
                ("fields", DATA_SET_FIELDS.into()),
                ("filter", in_filter("id", &select.data_sets)),
            ],
        )?;
        let mut have = BTreeSet::new();
        for v in listed
            .get("dataSets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(rate) = Indicator::from_data_set(v) {
                have.insert(rate.uid().to_string());
                found.insert(rate.id.clone(), rate);
            }
        }
        let missing: Vec<&str> = select
            .data_sets
            .iter()
            .map(String::as_str)
            .filter(|d| !have.contains(*d))
            .collect();
        if !missing.is_empty() {
            return Err(PipesError::Usage(format!(
                "DHIS2 has no data set {}",
                missing.join(", ")
            )));
        }
    }
    if found.is_empty() {
        return Err(PipesError::Usage(
            "the selection has no indicators, aggregate data elements or data sets".into(),
        ));
    }
    Ok(found)
}

pub fn run_measures(args: &Dhis2MeasuresArgs) -> Result<()> {
    check_out(&args.out)?;
    let instance = Instance::new(&args.conn.instance)?;
    let map = if args.records {
        None
    } else {
        Some(super::package_map(
            "measures",
            &args.package,
            INDICATOR_MODEL,
        )?)
    };
    let writes = Writes::new(map.as_ref());
    let api = Dhis2::connect(&args.conn)?;
    let indicators = fetch_indicators(&api, &args.select)?;
    let mut report = Report::default();
    let mut measures: Vec<Value> = Vec::with_capacity(indicators.len());
    match writes {
        Writes::Records => measures.extend(
            indicators
                .values()
                .map(|ind| record(ind, &instance, &api.info)),
        ),
        Writes::Maps(map) => {
            let records: Vec<Value> = indicators
                .values()
                .map(|ind| record(ind, &instance, &api.info))
                .collect();
            let records: Vec<&Value> = records.iter().collect();
            for (ind, output) in indicators.values().zip(map.transform_all(&records)) {
                measures.extend(RecordMap::take(output, &ind.id, &mut report)?);
            }
        }
    }
    let n = write_ndjson(&args.out, &measures)?;
    let mut by_scoring: BTreeMap<&str, usize> = BTreeMap::new();
    for ind in indicators.values() {
        let key = match ind.kind {
            Kind::DataElement => "data element counts",
            Kind::ReportingRate => "reporting rates",
            Kind::Indicator => ind.scoring().code(),
        };
        *by_scoring.entry(key).or_default() += 1;
    }
    let scoring: Vec<String> = by_scoring.iter().map(|(k, v)| format!("{v} {k}")).collect();
    let what = if args.records {
        format!("{} records", record_type(INDICATOR_MODEL))
    } else {
        "Measures".to_string()
    };
    println!(
        "Wrote {n} {what} to {} ({})",
        args.out.display(),
        scoring.join(", ")
    );
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anc1() -> Value {
        json!({"id":"Uvn6LCg7dVU","code":"IN_52486","name":"ANC 1 Coverage","annualized":true,
               "lastUpdated":"2024-01-02T03:04:05.000",
               "indicatorType":{"name":"Per cent","factor":100,"number":false},
               "numerator":"#{fbfJHSPpUQD}","numeratorDescription":"ANC 1st visit",
               "denominator":"#{h0xKKjijTdI}","denominatorDescription":"Expected pregnancies",
               "indicatorGroups":[{"id":"oehv9EO3vP7","name":"ANC"}]})
    }

    const IDS: &str = "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo";
    const CODES: &str = "https://icr.healthcampaigns.org/CodeSystem/dhis2/sl-demo";

    /// The Measure the example package writes for `ind`.
    fn measure(ind: &Indicator, instance: &Instance, info: &ServerInfo) -> Value {
        let map = crate::maps::icr_package(INDICATOR_MODEL);
        let mut report = Report::default();
        let mut out = map
            .run(&record(ind, instance, info), &ind.id, &mut report)
            .unwrap();
        assert_eq!(out.len(), 1, "{}", ind.id);
        assert_eq!(report.summary(), Report::default().summary(), "{}", ind.id);
        out.remove(0)
    }

    #[test]
    /// The records are the package's test case indicators, and the package
    /// maps them to the case's expected Measures: what the built-in mapping
    /// wrote before it was removed.
    fn the_records_are_the_package_case_indicators() {
        let instance = Instance::new("sl-demo").unwrap();
        let info = ServerInfo {
            system_name: Some("DHIS 2 Demo - Sierra Leone".into()),
            utc: true,
            ..Default::default()
        };
        let mut ratio = Indicator::parse(&anc1()).unwrap();
        ratio.factor = 1000.0;
        ratio.decimals = Some(1);
        ratio.description = Some("Per thousand".into());
        ratio.code = None;
        let mut no_group_id = Indicator::parse(&anc1()).unwrap();
        no_group_id.groups[0].id = None;
        let items = [
            // A percent: proportion, with both populations.
            Indicator::parse(&anc1()).unwrap(),
            ratio,
            no_group_id,
            // A plain number: cohort, with one population.
            Indicator::parse(&json!({"id":"dy1a1mseGR7","name":"ANC visits total",
                "indicatorType":{"name":"Number","factor":1},"numerator":"#{a}+#{b}","denominator":" 1 "}))
            .unwrap(),
            // Nothing but an id.
            Indicator::parse(&json!({"id": "x"})).unwrap(),
            // A data element in a group of two sets, and in a group of none.
            Indicator::from_data_element(&json!({"id":"s46m5MS0hxu","code":"DE_359706","name":"BCG doses given",
                "aggregationType":"SUM","lastUpdated":"2024-01-02T03:04:05.000",
                "dataElementGroups":[{"id":"h9cuJOkOwY2","name":"Immunisation","groupSets":[{"id":"XY1vwCQskjX"},{"id":"jp826jAJHUc"}]},
                                     {"id":"SriP0jBXMr6","name":"All"}]}))
            .unwrap(),
            Indicator::from_data_set(
                &json!({"id":"QX4ZTUbOt3a","code":"DS_1","name":"Reproductive Health","periodType":"Monthly"}),
            )
            .unwrap(),
        ];
        // Each item its own id, so the case's Measures differ by id.
        let mut items = items;
        items[1].id = "Uvn6LCg7dV1".into();
        items[2].id = "Uvn6LCg7dV2".into();
        let records: Vec<Value> = items
            .iter()
            .map(|ind| record(ind, &instance, &info))
            .collect();
        crate::maps::case::records("indicators", &records);
        let mapped: Vec<Value> = items
            .iter()
            .map(|ind| measure(ind, &instance, &info))
            .collect();
        crate::maps::case::expected("indicators", &mapped);
    }

    #[test]
    fn scoring_follows_the_indicator_type() {
        let mut ind = Indicator::parse(&anc1()).unwrap();
        assert_eq!(ind.scoring(), Scoring::Proportion);
        ind.factor = 1000.0;
        assert_eq!(ind.scoring(), Scoring::Ratio);
        ind.denominator = " 1 ".into();
        assert_eq!(ind.scoring(), Scoring::Cohort);
    }

    #[test]
    fn a_missing_factor_is_one() {
        let ind = Indicator::parse(&json!({"id": "x", "numerator": "#{a}", "denominator": "#{b}"}))
            .unwrap();
        assert_eq!(ind.factor, 1.0);
        assert_eq!(ind.scoring(), Scoring::Ratio);
    }

    #[test]
    fn a_proportion_measure_carries_both_expressions_and_the_factor() {
        let instance = Instance::new("sl-demo").unwrap();
        let info = ServerInfo {
            system_name: Some("DHIS 2 Demo - Sierra Leone".into()),
            utc: true,
            ..Default::default()
        };
        let m = measure(&Indicator::parse(&anc1()).unwrap(), &instance, &info);
        assert_eq!(m["id"], "sl-demo-Uvn6LCg7dVU");
        assert_eq!(
            m["url"],
            "https://icr.healthcampaigns.org/Measure/sl-demo-Uvn6LCg7dVU"
        );
        assert_eq!(m["identifier"][0]["value"], "Uvn6LCg7dVU");
        assert_eq!(m["identifier"][1]["value"], "IN_52486");
        assert_eq!(m["title"], "ANC 1 Coverage");
        assert_eq!(m["date"], "2024-01-02T03:04:05.000Z");
        assert_eq!(m["publisher"], "DHIS 2 Demo - Sierra Leone");
        assert_eq!(m["topic"][0]["text"], "ANC");
        assert_eq!(m["scoring"]["coding"][0]["code"], "proportion");
        let pops = m["group"][0]["population"].as_array().unwrap();
        assert_eq!(pops.len(), 2);
        assert_eq!(pops[0]["code"]["coding"][0]["code"], "numerator");
        assert_eq!(pops[0]["criteria"]["expression"], "#{fbfJHSPpUQD}");
        assert_eq!(pops[1]["description"], "Expected pregnancies");
        let ext = m["extension"][0]["extension"].as_array().unwrap();
        assert!(ext.contains(&json!({"url": "factor", "valueDecimal": 100.0})));
        assert!(ext.contains(&json!({"url": "annualized", "valueBoolean": true})));
    }

    #[test]
    fn a_data_element_is_a_count_under_its_own_identifier_system() {
        let instance = Instance::new("sl-demo").unwrap();
        let ind = Indicator::from_data_element(&json!({"id":"s46m5MS0hxu","code":"DE_359706","name":"BCG doses given",
            "aggregationType":"SUM","dataElementGroups":[{"id":"h9cuJOkOwY2","name":"Immunization"}]}))
        .unwrap();
        assert_eq!(ind.kind, Kind::DataElement);
        assert_eq!(ind.scoring(), Scoring::Cohort);
        let m = measure(&ind, &instance, &ServerInfo::default());
        assert_eq!(m["id"], "sl-demo-s46m5MS0hxu");
        assert_eq!(m["identifier"][0]["system"], format!("{IDS}/data-element"));
        assert_eq!(
            m["identifier"][1]["system"],
            format!("{IDS}/data-element-code")
        );
        assert_eq!(
            m["group"][0]["population"][0]["criteria"]["expression"],
            "#{s46m5MS0hxu}"
        );
        assert_eq!(m["topic"][0]["text"], "Immunization");
        assert_eq!(m["topic"][0]["coding"][0]["code"], "h9cuJOkOwY2");
        assert_eq!(
            m["topic"][0]["coding"][0]["system"],
            format!("{CODES}/data-element-group")
        );
        let ext = m["extension"][0]["extension"].as_array().unwrap();
        assert!(ext.contains(&json!({"url": "factor", "valueDecimal": 1.0})));
        assert!(
            ext.contains(&json!({"url": "indicatorType", "valueString": "Data element (SUM)"}))
        );
    }

    #[test]
    fn a_data_set_becomes_its_reporting_rate_under_a_valid_fhir_id() {
        let instance = Instance::new("sl-demo").unwrap();
        let rate = Indicator::from_data_set(
            &json!({"id":"QX4ZTUbOt3a","name":"Reproductive Health","periodType":"Monthly"}),
        )
        .unwrap();
        assert_eq!(rate.id, "QX4ZTUbOt3a.REPORTING_RATE");
        assert_eq!(rate.uid(), "QX4ZTUbOt3a");
        assert_eq!(rate.scoring(), Scoring::Proportion);
        let m = measure(&rate, &instance, &ServerInfo::default());
        assert_eq!(m["id"], "sl-demo-QX4ZTUbOt3a.REPORTING-RATE");
        assert!(fhir_pipes::fhir::group::is_valid_id(
            m["id"].as_str().unwrap()
        ));
        assert_eq!(m["title"], "Reproductive Health - Reporting rate");
        assert_eq!(m["identifier"][0]["system"], format!("{IDS}/data-set"));
        assert_eq!(m["identifier"][0]["value"], "QX4ZTUbOt3a");
        let pops = m["group"][0]["population"].as_array().unwrap();
        assert_eq!(
            pops[0]["criteria"]["expression"],
            "R{QX4ZTUbOt3a.ACTUAL_REPORTS}"
        );
        assert_eq!(pops[1]["description"], "Expected reports");
    }

    #[test]
    fn data_element_groups_in_a_group_set_are_coded_by_that_set() {
        let instance = Instance::new("sl-demo").unwrap();
        let de = Indicator::from_data_element(&json!({"id":"a","name":"Malaria new",
            "dataElementGroups":[{"id":"TzwKbcw1nUK","name":"Malaria","groupSets":[{"id":"jp826jAJHUc"}]},
                                 {"id":"weRMUzBs8T7","name":"New cases","groupSets":[{"id":"lv8UXn17ZOm"}]}]})).unwrap();
        let m = measure(&de, &instance, &ServerInfo::default());
        let topics = m["topic"].as_array().unwrap();
        assert_eq!(topics.len(), 2);
        assert_eq!(
            topics[0]["coding"][0]["system"],
            format!("{CODES}/data-element-group-set/jp826jAJHUc")
        );
        assert_eq!(topics[0]["coding"][0]["code"], "TzwKbcw1nUK");
        assert_eq!(topics[1]["coding"][0]["display"], "New cases");
    }

    #[test]
    fn a_number_indicator_is_a_cohort_with_one_population() {
        let instance = Instance::new("sl-demo").unwrap();
        let ind = Indicator::parse(&json!({"id":"dy1a1mseGR7","name":"ANC visits total",
            "indicatorType":{"name":"Number (Factor 1)","factor":1,"number":true},
            "numerator":"#{a}+#{b}","denominator":"1"}))
        .unwrap();
        let m = measure(&ind, &instance, &ServerInfo::default());
        assert_eq!(m["scoring"]["coding"][0]["code"], "cohort");
        let pops = m["group"][0]["population"].as_array().unwrap();
        assert_eq!(pops.len(), 1);
        assert_eq!(pops[0]["code"]["coding"][0]["code"], "initial-population");
        assert!(m.get("date").is_none());
    }
}
