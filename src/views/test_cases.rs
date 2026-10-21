//! `views test`: run SQL-on-FHIR test-case files in memory and compare each
//! case's rows with what it expects.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use fhir_pipes_sof::ViewDefinition;

use super::diagnostic;
use crate::error::{Findings, usage};

/// Run the test-case file `manifest`, or every `.json` file in the directory
/// `manifest`, and print a summary; a failed case is a findings error.
pub(super) fn run(manifest: &Path, color: bool) -> Result<()> {
    let mut files = Vec::new();
    if manifest.is_dir() {
        let mut entries = crate::fs::sorted_entries(manifest)
            .with_context(|| format!("reading directory {}", manifest.display()))?;
        entries.retain(|p| p.extension().is_some_and(|x| x == "json"));
        files.extend(entries);
    } else {
        files.push(manifest.to_path_buf());
    }
    let (mut pass, mut total) = (0usize, 0usize);
    for file in &files {
        let (p, t) = run_test_file(file, color)?;
        pass += p;
        total += t;
    }
    let summary = format!("{pass}/{total} passed");
    if pass == total {
        println!("{}", diagnostic::ok(&summary, color));
    } else {
        println!("{summary}");
        return Err(Findings.into());
    }
    Ok(())
}

/// Run one test-case file in memory, printing each case. Returns
/// `(passed, total)`.
fn run_test_file(path: &Path, color: bool) -> Result<(usize, usize)> {
    let doc: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
            .map_err(usage)?,
    )
    .with_context(|| format!("parsing {}", path.display()))
    .map_err(usage)?;

    let resources: Vec<serde_json::Value> = doc
        .get("resources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let empty = Vec::new();
    let tests = doc
        .get("tests")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (mut pass, mut total) = (0usize, 0usize);
    for test in tests {
        total += 1;
        let title = test
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("<untitled>");
        let (ok, detail) = run_one_test(test, &resources);
        let marker = diagnostic::marker(ok, color);
        if ok {
            pass += 1;
            println!("  {marker} [{name}] {title}");
        } else {
            println!("  {marker} [{name}] {title}: {detail}");
        }
    }
    Ok((pass, total))
}

fn run_one_test(test: &serde_json::Value, resources: &[serde_json::Value]) -> (bool, String) {
    use serde_json::Value;
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let view = match ViewDefinition::from_json(&test.get("view").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v,
        Err(e) => return (expect_error, format!("parse error: {e}")),
    };
    let result = match fhir_pipes_sof::execute_blocking(&view, resources) {
        Ok(r) => r,
        Err(e) => return (expect_error, format!("execute error: {e}")),
    };
    if expect_error {
        return (false, "expected an error but execution succeeded".into());
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual = result.to_json_array();
        if multiset_eq(&actual, expect) {
            (true, String::new())
        } else {
            (false, format!("rows differ: got {}", Value::Array(actual)))
        }
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = result.data.len() as u64;
        (got == count, format!("want {count} rows, got {got}"))
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let want: Vec<&str> = cols.iter().filter_map(Value::as_str).collect();
        (got == want, format!("want columns {want:?}, got {got:?}"))
    } else {
        (true, String::new())
    }
}

/// Order-insensitive comparison of two sets of JSON rows.
fn multiset_eq(a: &[serde_json::Value], b: &[serde_json::Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(canonical_json).collect();
    let mut b: Vec<String> = b.iter().map(canonical_json).collect();
    a.sort();
    b.sort();
    a == b
}

/// JSON with object keys sorted, for comparing rows as sets.
fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{k:?}:{}", canonical_json(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}
