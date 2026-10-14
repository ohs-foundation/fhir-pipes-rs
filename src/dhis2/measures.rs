//! `fhir-pipes dhis2 measures`: DHIS2 indicators and data elements as FHIR Measures.
//!
//! An indicator is numerator ÷ denominator × factor. A percent (factor 100)
//! is scored `proportion`, any other factor `ratio`, and a plain number
//! (denominator `1`) `cohort` with a single population. The expressions,
//! the factor and the annualized flag ride along so a reader can see how
//! DHIS2 computes the value; `measureScore` on the reports is the DHIS2
//! value divided by this factor (45.18 % → 0.4518), the ICR convention.
//!
//! A data element (an aggregate count such as "BCG doses given") becomes a
//! `cohort` Measure too: its value is the total over every disaggregation,
//! the same number DHIS2 shows for `#{uid}`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::api::{Dhis2, ServerInfo};
use super::{Instance, check_out, write_ndjson};
use crate::cli::{Dhis2MeasuresArgs, IndicatorSelection};
use crate::error::{PipesError, Result};
use crate::report::Report;
use crate::transform::write_report;

pub const INDICATOR_EXTENSION_URL: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/dhis2-indicator";
pub const MEASURE_SCORING_SYSTEM: &str = "http://terminology.hl7.org/CodeSystem/measure-scoring";
pub const MEASURE_POPULATION_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/measure-population";
/// DHIS2's indicator expression language; not a registered media type.
pub const EXPRESSION_LANGUAGE: &str = "text/x-dhis2-expression";

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

fn groups(v: &Value, key: &str) -> Vec<Group> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| {
            Some(Group {
                id: g.get("id").and_then(Value::as_str).map(str::to_string),
                name: g.get("name")?.as_str()?.to_string(),
                sets: g
                    .get("groupSets")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.get("id")?.as_str().map(str::to_string))
                    .collect(),
            })
        })
        .collect()
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
    fn display(self) -> &'static str {
        match self {
            Scoring::Proportion => "Proportion",
            Scoring::Ratio => "Ratio",
            Scoring::Cohort => "Cohort",
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

fn population(code: &str, display: &str, description: Option<&str>, expression: &str) -> Value {
    let mut p = json!({
        "code": {"coding": [{"system": MEASURE_POPULATION_SYSTEM, "code": code, "display": display}]},
    });
    if let Some(d) = description {
        p["description"] = json!(d);
    }
    p["criteria"] = json!({"language": EXPRESSION_LANGUAGE, "expression": expression});
    p
}

pub fn measure(ind: &Indicator, instance: &Instance, info: &ServerInfo) -> Value {
    let scoring = ind.scoring();
    let mut sub = vec![json!({"url": "factor", "valueDecimal": ind.factor})];
    if let Some(t) = &ind.type_name {
        sub.push(json!({"url": "indicatorType", "valueString": t}));
    }
    sub.push(json!({"url": "annualized", "valueBoolean": ind.annualized}));
    if let Some(d) = ind.decimals {
        sub.push(json!({"url": "decimals", "valueInteger": d}));
    }
    let (system, code_system) = match ind.kind {
        Kind::Indicator => (
            instance.indicator_system(),
            instance.indicator_code_system(),
        ),
        Kind::DataElement => (
            instance.data_element_system(),
            instance.data_element_code_system(),
        ),
        Kind::ReportingRate => (instance.data_set_system(), instance.data_set_code_system()),
    };
    let mut identifier = vec![json!({"use": "official", "system": system, "value": ind.uid()})];
    if let Some(code) = &ind.code {
        identifier.push(json!({"system": code_system, "value": code}));
    }
    let mut m = json!({
        "resourceType": "Measure",
        "id": instance.measure_id(&ind.fhir_key()),
        "meta": {"tag": [instance.tag()]},
        "extension": [{"url": INDICATOR_EXTENSION_URL, "extension": sub}],
        "url": instance.measure_url(&ind.fhir_key()),
        "identifier": identifier,
        "title": ind.name,
        "status": "active",
    });
    if let Some(d) = ind.last_updated.as_deref().and_then(|t| info.timestamp(t)) {
        m["date"] = json!(d);
    }
    if let Some(p) = &info.system_name {
        m["publisher"] = json!(p);
    }
    if let Some(d) = &ind.description {
        m["description"] = json!(d);
    }
    if !ind.groups.is_empty() {
        m["topic"] = Value::Array(topics(ind, instance));
    }
    m["subjectCodeableConcept"] =
        json!({"coding": [{"system": "http://hl7.org/fhir/resource-types", "code": "Location"}]});
    m["scoring"] = json!({"coding": [{"system": MEASURE_SCORING_SYSTEM, "code": scoring.code(), "display": scoring.display()}]});
    let populations = match scoring {
        Scoring::Cohort => vec![population(
            "initial-population",
            "Initial Population",
            ind.numerator_description.as_deref(),
            &ind.numerator,
        )],
        _ => vec![
            population(
                "numerator",
                "Numerator",
                ind.numerator_description.as_deref(),
                &ind.numerator,
            ),
            population(
                "denominator",
                "Denominator",
                ind.denominator_description.as_deref(),
                &ind.denominator,
            ),
        ],
    };
    m["group"] = json!([{"population": populations}]);
    m
}

/// One topic per group, coded so a reader can regroup items the way DHIS2
/// does: by indicator group, by data element group, or by the groups of a
/// data element group set (one topic per set the group is in).
fn topics(ind: &Indicator, instance: &Instance) -> Vec<Value> {
    let mut out = Vec::new();
    for g in &ind.groups {
        let coded = |system: String| match &g.id {
            Some(id) => {
                json!({"coding": [{"system": system, "code": id, "display": g.name}], "text": g.name})
            }
            None => json!({"text": g.name}),
        };
        match ind.kind {
            Kind::DataElement if !g.sets.is_empty() => {
                for set in &g.sets {
                    out.push(coded(
                        instance.group_system(&format!("data-element-group-set/{set}")),
                    ));
                }
            }
            Kind::DataElement => out.push(coded(instance.group_system("data-element-group"))),
            _ => out.push(coded(instance.group_system("indicator-group"))),
        }
    }
    out
}

/// A DHIS2 analytics item (`uid`, `uid.REPORTING_RATE`) as a FHIR id part.
pub fn fhir_key(dx: &str) -> String {
    dx.replace('_', "-")
}

fn in_filter(field: &str, ids: &[String]) -> String {
    format!("{field}:in:[{}]", ids.join(","))
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
        if src.key == "dataElements" {
            // Tracker data elements have no aggregate value to report.
            query.push(("filter", "domainType:eq:AGGREGATE".into()));
        }
        add(&api.get(src.endpoint, &query)?);
    }
    if !ids.is_empty() {
        add(&api.get(
            src.endpoint,
            &[
                ("paging", "false".into()),
                ("fields", src.fields.into()),
                ("filter", in_filter("id", ids)),
            ],
        )?);
        let missing: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|i| !found.contains_key(*i))
            .collect();
        if !missing.is_empty() {
            return Err(PipesError::Usage(format!(
                "DHIS2 has no {} {}",
                src.what,
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
    let api = Dhis2::connect(&args.conn)?;
    let indicators = fetch_indicators(&api, &args.select)?;
    let report = Report::default();
    let measures: Vec<Value> = indicators
        .values()
        .map(|ind| measure(ind, &instance, &api.info))
        .collect();
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
    println!(
        "Wrote {n} Measures to {} ({})",
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
        assert_eq!(m["identifier"][0]["system"], instance.data_element_system());
        assert_eq!(
            m["identifier"][1]["system"],
            instance.data_element_code_system()
        );
        assert_eq!(
            m["group"][0]["population"][0]["criteria"]["expression"],
            "#{s46m5MS0hxu}"
        );
        assert_eq!(m["topic"][0]["text"], "Immunization");
        assert_eq!(m["topic"][0]["coding"][0]["code"], "h9cuJOkOwY2");
        assert_eq!(
            m["topic"][0]["coding"][0]["system"],
            instance.group_system("data-element-group")
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
        assert!(crate::fhir::group::is_valid_id(m["id"].as_str().unwrap()));
        assert_eq!(m["title"], "Reproductive Health - Reporting rate");
        assert_eq!(m["identifier"][0]["system"], instance.data_set_system());
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
            instance.group_system("data-element-group-set/jp826jAJHUc")
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
