//! What the FHIRPath test suites (`fhirpath_hl7.rs`, `fhirpath_js.rs`) share:
//! running an expression the way fhir-pipes runs any FHIRPath, and the list of
//! known failures with the cause of each.

use std::collections::BTreeMap;
use std::path::Path;

use fhir_pipes_sof::{ViewDefinition, execute};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The values of `expression` on `input`, as fhir-pipes returns them for a
/// collection column: a one-column view, under `forEach: <context>` when
/// there is one, with `constants` as the view's `constant` entries.
pub async fn evaluate(
    expression: &str,
    context: Option<&str>,
    constants: Value,
    input: &Value,
) -> Result<Vec<Value>, String> {
    let resource = input["resourceType"].as_str().unwrap();
    let column = json!([{ "name": "r", "path": expression, "collection": true }]);
    let select = match context {
        None => json!([{ "column": column }]),
        Some(context) => json!([{ "forEach": context, "column": column }]),
    };
    let view: ViewDefinition = serde_json::from_value(json!({
        "resourceType": "ViewDefinition", "status": "active", "resource": resource,
        "constant": constants, "select": select
    }))
    .map_err(|e| e.to_string())?;
    let result = execute(&view, std::slice::from_ref(input))
        .await
        .map_err(|e| e.to_string())?;
    Ok(result
        .data
        .into_iter()
        .flat_map(|row| match row.into_iter().next() {
            Some(Value::Array(values)) => values,
            Some(Value::Null) | None => vec![],
            Some(value) => vec![value],
        })
        .collect())
}

/// A number, or a string holding one.
pub fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Why fhir-pipes fails a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// The expression is wrong for the types it works on, which the spec
    /// wants reported before evaluation. octofhir-fhirpath has no type
    /// checker: it evaluates the expression instead.
    TypeChecker,
    /// octofhir-fhirpath gives a wrong result or a wrong error.
    Engine,
    /// octofhir-fhirpath does not have the function, operator or literal.
    NotSupported,
    /// fhir-pipes' own handling of the expression or its result.
    FhirPipes,
    /// The test expects something specific to the implementation it came
    /// from, or something the spec does not say.
    Suite,
    /// Not yet looked at. The test fails while any failure has this cause.
    Unsorted,
}

/// A test fhir-pipes fails: why, and what the last run gave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownFailure {
    pub cause: Cause,
    /// An issue that tracks it, as `owner/repo#n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What the run gave, written by the test.
    pub got: String,
}

/// Compare this run's failures (test key to what it gave) with the list at
/// `path`, both ways. With `FHIRPATH_UPDATE_KNOWN_FAILURES` set, rewrite the
/// list instead: each failure keeps the cause, issue and note it had, and a
/// new one is `unsorted`.
pub fn check_known_failures(
    suite: &str,
    path: &Path,
    total: usize,
    failures: BTreeMap<String, String>,
) {
    eprintln!("{suite}: {}/{total} pass", total - failures.len());
    let known: BTreeMap<String, KnownFailure> = std::fs::read_to_string(path)
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap_or_default();

    if std::env::var_os("FHIRPATH_UPDATE_KNOWN_FAILURES").is_some() {
        let list: BTreeMap<_, _> = failures
            .into_iter()
            .map(|(key, got)| {
                let entry = match known.get(&key) {
                    Some(k) => KnownFailure { got, ..k.clone() },
                    None => KnownFailure {
                        cause: Cause::Unsorted,
                        issue: None,
                        note: None,
                        got,
                    },
                };
                (key, entry)
            })
            .collect();
        std::fs::write(path, serde_json::to_string_pretty(&list).unwrap() + "\n").unwrap();
        return;
    }

    let mut by_cause = BTreeMap::<Cause, usize>::new();
    for k in known.values() {
        *by_cause.entry(k.cause).or_default() += 1;
    }
    eprintln!("  known failures by cause: {by_cause:?}");

    let new: Vec<_> = failures
        .iter()
        .filter(|(k, _)| !known.contains_key(*k))
        .collect();
    let fixed: Vec<_> = known
        .keys()
        .filter(|k| !failures.contains_key(*k))
        .collect();
    let unsorted: Vec<_> = known
        .iter()
        .filter(|(_, k)| k.cause == Cause::Unsorted)
        .map(|(key, _)| key)
        .collect();
    assert!(
        new.is_empty() && fixed.is_empty() && unsorted.is_empty(),
        "{} is out of date (see tests/common/mod.rs to rewrite it).\n\
         New failures: {new:#?}\nListed but now passing: {fixed:#?}\n\
         Listed without a cause: {unsorted:#?}",
        path.display()
    );
}
