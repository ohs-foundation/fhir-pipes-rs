//! The official HL7 FHIRPath test suite (R4), run the way fhir-pipes runs any
//! FHIRPath: as a ViewDefinition column through the in-memory evaluator.
//!
//! Each case becomes a one-column view whose column is the test expression,
//! marked `collection` so every value comes back; a test with a `context`
//! runs the expression under `forEach: <context>`. The rows are JSON, so
//! results are compared by value: the suite's output types are not checked.
//!
//! fhir-pipes does not pass every case. `tests/fhirpath/known-failures.json`
//! lists the ones it fails today, and this test enforces that list both ways:
//! a failure not on the list fails the test, and so does a listed case that
//! now passes. After a change that moves either way, rewrite the list with
//! `FHIRPATH_UPDATE_KNOWN_FAILURES=1 cargo test -p fhir-pipes-sof --test fhirpath_hl7`
//! and review the diff. Data and provenance: `tests/fhirpath/SOURCE.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fhir_pipes_sof::{ViewDefinition, execute};
use serde_json::{Value, json};

struct Case {
    key: String,
    input: Option<String>,
    expression: String,
    invalid: Option<String>,
    predicate: bool,
    unordered: bool,
    context: Option<String>,
    outputs: Vec<(String, String)>,
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fhirpath")
}

fn load(path: &Path) -> Vec<Case> {
    let text = std::fs::read_to_string(path).unwrap();
    let doc = roxmltree::Document::parse(&text).unwrap();
    let mut cases = Vec::new();
    let mut seen = BTreeMap::<String, usize>::new();
    for group in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name("group"))
    {
        for t in group.children().filter(|n| n.has_tag_name("test")) {
            // The suite repeats a few (group, name) pairs; number the repeats.
            let mut key = format!(
                "{}/{}",
                group.attribute("name").unwrap(),
                t.attribute("name").unwrap()
            );
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            if *n > 1 {
                key = format!("{key}#{n}");
            }
            let expr = t.children().find(|n| n.has_tag_name("expression")).unwrap();
            cases.push(Case {
                key,
                input: t.attribute("inputfile").map(|f| {
                    f.trim_end_matches(".xml")
                        .trim_end_matches(".json")
                        .to_string()
                }),
                expression: expr.text().unwrap_or("").to_string(),
                invalid: expr.attribute("invalid").map(str::to_string),
                predicate: t.attribute("predicate") == Some("true"),
                unordered: t.attribute("ordered") == Some("false"),
                context: t.attribute("context").map(str::to_string),
                outputs: t
                    .children()
                    .filter(|n| n.has_tag_name("output"))
                    .map(|o| {
                        (
                            o.attribute("type").unwrap_or("").to_string(),
                            o.text().unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
            });
        }
    }
    cases
}

/// The expression's values, as fhir-pipes returns them for a collection column.
async fn evaluate(case: &Case, input: &Value) -> Result<Vec<Value>, String> {
    let resource = input["resourceType"].as_str().unwrap();
    let column = json!([{ "name": "r", "path": case.expression, "collection": true }]);
    let select = match &case.context {
        None => json!([{ "column": column }]),
        Some(context) => json!([{ "forEach": context, "column": column }]),
    };
    let view: ViewDefinition = serde_json::from_value(json!({
        "resourceType": "ViewDefinition", "status": "active", "resource": resource, "select": select
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

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The type of an `<output>` the suite leaves untyped, read from its literal:
/// `@T10:30`, `@2014-01`, `1.5 'cm'`, `1.58`, `true`.
fn literal_type(text: &str) -> &'static str {
    if text.starts_with("@T") {
        "time"
    } else if text.starts_with('@') {
        "dateTime"
    } else if text.ends_with('\'') && text.contains(" '") {
        "Quantity"
    } else if text.parse::<f64>().is_ok() {
        "decimal"
    } else if text == "true" || text == "false" {
        "boolean"
    } else {
        "string"
    }
}

/// Whether a value matches one expected `<output type="...">text</output>`.
fn matches(value: &Value, (ty, text): &(String, String)) -> bool {
    let ty = if ty.is_empty() {
        literal_type(text)
    } else {
        ty
    };
    match ty {
        "boolean" => value.as_bool() == Some(text == "true"),
        "integer" | "decimal" => number(value).is_some() && number(value) == text.parse().ok(),
        "date" | "dateTime" | "instant" => value.as_str() == Some(text.trim_start_matches('@')),
        "time" => value.as_str() == Some(text.trim_start_matches("@T")),
        "Quantity" => {
            // `1 'g'`, or a calendar duration such as `1 year`.
            let (num, unit) = text.split_once(' ').unwrap_or((text, ""));
            let unit = unit.trim_matches('\'');
            number(&value["value"]) == num.parse().ok()
                && [&value["code"], &value["unit"]]
                    .iter()
                    .any(|u| u.as_str() == Some(unit))
        }
        "string" | "code" | "id" | "uri" | "url" | "canonical" | "markdown" | "oid" | "uuid" => {
            value.as_str() == Some(text)
        }
        // A complex type: the suite checks only that a value is there.
        _ => true,
    }
}

/// `Ok` if the result is what the case expects, else why not.
fn judge(case: &Case, result: Result<Vec<Value>, String>) -> Result<(), String> {
    if let Some(kind) = &case.invalid {
        return match result {
            Err(_) => Ok(()),
            Ok(values) => Err(format!(
                "expected a {kind} error, got {}",
                Value::from(values)
            )),
        };
    }
    let mut values = result?;
    if case.predicate {
        values = match values.len() {
            0 => vec![],
            1 if values[0].is_boolean() => values,
            1 => vec![json!(true)],
            n => return Err(format!("predicate over {n} values")),
        };
    }
    let expected = || {
        case.outputs
            .iter()
            .map(|(t, v)| format!("{t} {v}"))
            .collect::<Vec<_>>()
    };
    if values.len() != case.outputs.len() {
        return Err(format!(
            "expected {:?}, got {}",
            expected(),
            Value::from(values)
        ));
    }
    let ok = if case.unordered {
        let mut used = vec![false; values.len()];
        case.outputs.iter().all(|out| {
            match (0..values.len()).find(|&i| !used[i] && matches(&values[i], out)) {
                Some(i) => {
                    used[i] = true;
                    true
                }
                None => false,
            }
        })
    } else {
        values
            .iter()
            .zip(&case.outputs)
            .all(|(v, out)| matches(v, out))
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "expected {:?}, got {}",
            expected(),
            Value::from(values)
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hl7_fhirpath_r4() {
    let dir = dir();
    let cases = load(&dir.join("tests-fhir-r4.xml"));
    let no_input = json!({ "resourceType": "Patient", "id": "empty" });

    let mut failures = BTreeMap::new();
    for case in &cases {
        let input = match &case.input {
            Some(name) => serde_json::from_str(
                &std::fs::read_to_string(dir.join(format!("input/{name}.json"))).unwrap(),
            )
            .unwrap(),
            None => no_input.clone(),
        };
        if let Err(reason) = judge(case, evaluate(case, &input).await) {
            let reason: String = reason.chars().take(200).collect();
            failures.insert(case.key.clone(), reason);
        }
    }
    let passed = cases.len() - failures.len();
    eprintln!("HL7 FHIRPath R4: {passed}/{} pass", cases.len());

    let list = dir.join("known-failures.json");
    if std::env::var_os("FHIRPATH_UPDATE_KNOWN_FAILURES").is_some() {
        std::fs::write(
            &list,
            serde_json::to_string_pretty(&failures).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let known: BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&list).unwrap()).unwrap();
    let new: Vec<_> = failures
        .iter()
        .filter(|(k, _)| !known.contains_key(*k))
        .collect();
    let fixed: Vec<_> = known
        .keys()
        .filter(|k| !failures.contains_key(*k))
        .collect();
    assert!(
        new.is_empty() && fixed.is_empty(),
        "known-failures.json is out of date (see this file's header to rewrite it).\n\
         New failures: {new:#?}\nListed but now passing: {fixed:#?}"
    );
}
