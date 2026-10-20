//! fhirpath.js's FHIRPath tests, run the way fhir-pipes runs any FHIRPath: as
//! a ViewDefinition column through the in-memory evaluator (see
//! `common::evaluate`). They cover the spec section by section, which the
//! HL7 suite (`fhirpath_hl7.rs`) does more thinly, and add the HL7 R5 tests.
//!
//! A test runs against its `inputfile`, or else the file's `subject`; a
//! subject with no `resourceType` is given one (`Subject`) so a view can
//! name it. Its `variables` become view constants. Results are compared by
//! value, as fhirpath.js writes them: a Quantity as `3 'min'`, a date or time
//! without its `@`, a Long as a string. Not run: tests fhirpath.js disables, tests whose subject
//! is not an object or whose input file it does not ship, and tests whose
//! `context` or `variables` a view cannot give (see `skip_reason`).
//!
//! `tests/fhirpath-js/known-failures.json` lists the tests fhir-pipes fails
//! today, each with its cause, enforced both ways as for the HL7 suite:
//! `FHIRPATH_UPDATE_KNOWN_FAILURES=1 cargo test -p fhir-pipes-sof --test fhirpath_js`
//! rewrites it. Data and provenance: `tests/fhirpath-js/SOURCE.md`.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{check_known_failures, number};
use serde_json::{Value, json};

struct Case {
    key: String,
    expression: String,
    input: Result<Value, &'static str>,
    variables: Option<Value>,
    context: bool,
    error: bool,
    result: Vec<Value>,
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fhirpath-js")
}

/// The tests in one case file, groups flattened, disabled tests left out.
fn load(file: &str, doc: &Value, seen: &mut BTreeMap<String, usize>) -> Vec<Case> {
    let subject = match &doc["subject"] {
        Value::Null => Ok(json!({ "resourceType": "Patient", "id": "empty" })),
        Value::Object(o) if o.contains_key("resourceType") => Ok(doc["subject"].clone()),
        Value::Object(o) => {
            let mut o = o.clone();
            o.insert("resourceType".into(), json!("Subject"));
            Ok(Value::Object(o))
        }
        _ => Err("the subject is not an object"),
    };
    let mut cases = Vec::new();
    walk(file, &doc["tests"], &subject, seen, &mut cases);
    cases
}

fn walk(
    file: &str,
    tests: &Value,
    subject: &Result<Value, &'static str>,
    seen: &mut BTreeMap<String, usize>,
    cases: &mut Vec<Case>,
) {
    for t in tests.as_array().into_iter().flatten() {
        if t["disable"] == json!(true) {
            continue;
        }
        if let Some(group) = t
            .as_object()
            .and_then(|o| o.iter().find(|(k, _)| k.starts_with("group")))
        {
            walk(file, group.1, subject, seen, cases);
            continue;
        }
        let expressions = match &t["expression"] {
            Value::String(e) => vec![e.clone()],
            Value::Array(es) => es
                .iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect(),
            _ => continue, // a heading
        };
        let input = match t["inputfile"].as_str() {
            Some(name) => {
                let model = t["model"].as_str().unwrap_or("r4");
                std::fs::read_to_string(dir().join(format!("resources/{model}/{name}")))
                    .map(|text| serde_json::from_str(&text).unwrap())
                    .map_err(|_| "fhirpath.js does not ship the input file")
            }
            None => subject.clone(),
        };
        for expression in expressions {
            // fhirpath.js repeats a few expressions; number the repeats.
            let mut key = format!("{file}/{expression}");
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            if *n > 1 {
                key = format!("{key}#{n}");
            }
            cases.push(Case {
                key,
                expression,
                input: input.clone(),
                variables: t.get("variables").cloned(),
                context: t.get("context").is_some(),
                error: t["error"] == json!(true),
                result: t["result"].as_array().cloned().unwrap_or_default(),
            });
        }
    }
}

/// Why a test cannot run, if it cannot.
fn skip_reason(case: &Case) -> Option<&'static str> {
    if let Err(reason) = &case.input {
        return Some(reason);
    }
    if case.context {
        return Some("it sets %context, which a view cannot");
    }
    let simple = |(name, value): (&String, &Value)| {
        name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && (value.is_string() || value.is_number() || value.is_boolean())
    };
    match &case.variables {
        Some(Value::Object(vars)) if !vars.iter().all(simple) => Some(
            "a variable is not a constant a view can give (a name, and a string, number or boolean)",
        ),
        _ => None,
    }
}

/// The view constants for a test's variables.
fn constants(case: &Case) -> Value {
    let Some(Value::Object(vars)) = &case.variables else {
        return json!([]);
    };
    vars.iter()
        .map(|(name, value)| match value {
            Value::String(s) => json!({ "name": name, "valueString": s }),
            Value::Bool(b) => json!({ "name": name, "valueBoolean": b }),
            Value::Number(n) if n.is_i64() => json!({ "name": name, "valueInteger": n }),
            n => json!({ "name": name, "valueDecimal": n }),
        })
        .collect()
}

/// Whether an expression has a Long literal (`5L`) or makes a Long.
fn has_long(expression: &str) -> bool {
    let b = expression.as_bytes();
    expression.contains("Long")
        || (1..b.len()).any(|i| {
            b[i] == b'L'
                && b[i - 1].is_ascii_digit()
                && b.get(i + 1).is_none_or(|c| !c.is_ascii_alphanumeric())
        })
}

/// Whether a value fhir-pipes returned is the one fhirpath.js expects. `long`:
/// the expression makes Long values, which fhirpath.js writes as strings.
fn same(value: &Value, expected: &Value, long: bool) -> bool {
    match expected {
        Value::Number(n) => number(value) == n.as_f64(),
        Value::String(text) if long && value.is_i64() => text.parse().ok() == value.as_i64(),
        Value::String(text) if value.is_object() => {
            // A Quantity: `3 'min'`, or a calendar duration such as `1 year`.
            let (num, unit) = text.split_once(' ').unwrap_or((text, ""));
            let unit = unit.trim_matches('\'');
            number(&value["value"]).is_some()
                && number(&value["value"]) == num.parse().ok()
                && [&value["code"], &value["unit"]]
                    .iter()
                    .any(|u| u.as_str() == Some(unit))
        }
        _ => value == expected,
    }
}

/// `Ok` if the result is what the case expects, else why not.
fn judge(case: &Case, result: Result<Vec<Value>, String>) -> Result<(), String> {
    if case.error {
        return match result {
            Err(_) => Ok(()),
            Ok(values) => Err(format!("expected an error, got {}", Value::from(values))),
        };
    }
    // fhirpath.js leaves out a value that is only an id or extensions.
    let values: Vec<Value> = result?.into_iter().filter(|v| !v.is_null()).collect();
    let long = has_long(&case.expression);
    let ok = values.len() == case.result.len()
        && values
            .iter()
            .zip(&case.result)
            .all(|(v, e)| same(v, e, long));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "expected {}, got {}",
            Value::from(case.result.clone()),
            Value::from(values)
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fhirpath_js_cases() {
    let dir = dir();
    let mut files: Vec<_> = std::fs::read_dir(dir.join("cases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    let mut cases = Vec::new();
    let mut seen = BTreeMap::new();
    for path in &files {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let file = path.file_stem().unwrap().to_str().unwrap();
        cases.extend(load(file, &doc, &mut seen));
    }

    let mut skipped = BTreeMap::<&str, usize>::new();
    let mut failures = BTreeMap::new();
    let mut run = 0;
    for case in &cases {
        if let Some(reason) = skip_reason(case) {
            *skipped.entry(reason).or_default() += 1;
            continue;
        }
        run += 1;
        let input = case.input.as_ref().unwrap();
        let result = common::evaluate(&case.expression, None, constants(case), input).await;
        if let Err(reason) = judge(case, result) {
            let reason: String = reason.chars().take(200).collect();
            failures.insert(case.key.clone(), reason);
        }
    }
    eprintln!("fhirpath.js: not run: {skipped:#?}");
    check_known_failures(
        "fhirpath.js",
        &dir.join("known-failures.json"),
        run,
        failures,
    );
}
