//! In-memory SQL-on-FHIR v2 conformance harness.
//!
//! Runs the official `sql-on-fhir-v2` content tests through the database-free
//! [`octofhir_sof::execute`] evaluator and compares the produced rows against
//! each test's expectation. Unlike `tests/conformance.rs` this needs no
//! PostgreSQL — it runs whenever the test-case checkout is present (the sibling
//! `../fhir-test-cases/sql-on-fhir`, overridable with `SOF_TEST_CASES_DIR`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use octofhir_sof::{ViewDefinition, execute, execute_parallel};
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn sql_on_fhir_conformance_in_memory() {
    let Some(dir) = test_cases_dir() else {
        eprintln!("test-cases dir not found — skipping in-memory conformance suite");
        return;
    };

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n != "manifest.json" && n != "tests.schema.json")
        })
        .collect();
    files.sort();

    let mut outcomes = Vec::new();
    for file in &files {
        run_file(file, &mut outcomes).await;
    }
    report(&outcomes);

    // Emit a result file in the format the official sql-on-fhir.org registry
    // expects (see write_result_json) when SOF_RESULT_JSON points at a path.
    if let Ok(path) = std::env::var("SOF_RESULT_JSON") {
        write_result_json(&outcomes, &path);
        eprintln!("wrote conformance result JSON to {path}");
    }

    // The in-memory evaluator passes the full vendored v2.1.0-pre suite.
    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} in-memory conformance cases failed");
}

/// Extra cases not in the official suite (e.g. contained-resource keying),
/// kept in `tests/extra/` so the vendored `tests/spec/` suite stays a clean
/// mirror of upstream.
#[tokio::test(flavor = "multi_thread")]
async fn extra_cases_in_memory() {
    // `tests/extra` holds extras that round-trip through both the SQL and the
    // in-memory paths; `tests/extra_memory` holds cases that only the in-memory
    // path supports (full-FHIRPath functions the SQL subset cannot lower), so
    // the SQL harnesses must not pick them up.
    let mut files: Vec<PathBuf> = Vec::new();
    for sub in ["tests/extra", "tests/extra_memory"] {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub);
        if !dir.is_dir() {
            continue;
        }
        files.extend(
            std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json")),
        );
    }
    if files.is_empty() {
        return;
    }
    files.sort();

    let mut outcomes = Vec::new();
    for file in &files {
        run_file(file, &mut outcomes).await;
    }
    report(&outcomes);

    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} extra in-memory cases failed");
}

struct Outcome {
    file: String,
    title: String,
    passed: bool,
    detail: String,
}

async fn run_file(path: &PathBuf, outcomes: &mut Vec<Outcome>) {
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("parse {file}: {e}"));

    let resources: Vec<Value> = doc
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let empty = Vec::new();
    let tests = doc.get("tests").and_then(Value::as_array).unwrap_or(&empty);
    for test in tests {
        let title = test
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>")
            .to_string();
        outcomes.push(run_test(&file, title, test, &resources).await);
    }
}

async fn run_test(file: &str, title: String, test: &Value, resources: &[Value]) -> Outcome {
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mk = |passed: bool, detail: String| Outcome {
        file: file.to_string(),
        title: title.clone(),
        passed,
        detail,
    };

    let view = match ViewDefinition::from_json(&test.get("view").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v,
        Err(e) => return mk(expect_error, format!("parse error: {e}")),
    };

    // The threaded evaluator must agree with the serial one exactly: the same
    // rows in the same order, or an error for both. Three threads split even
    // these small resource sets across several workers.
    let parallel = tokio::task::block_in_place(|| execute_parallel(&view, resources, 3));
    let result = match execute(&view, resources).await {
        Ok(r) => r,
        Err(e) => {
            if parallel.is_ok() {
                return mk(
                    false,
                    format!("execute error {e}, but execute_parallel succeeded"),
                );
            }
            return mk(expect_error, format!("execute error: {e}"));
        }
    };
    match &parallel {
        Ok(p) if p.data == result.data && p.columns.len() == result.columns.len() => {}
        Ok(_) => return mk(false, "execute_parallel rows differ from execute".into()),
        Err(e) => return mk(false, format!("execute_parallel failed: {e}")),
    }

    if expect_error {
        return mk(false, "expected error but execution succeeded".into());
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual = result.to_json_array();
        if multiset_eq(&actual, expect) {
            mk(true, String::new())
        } else {
            mk(
                false,
                format!(
                    "rows differ: expected {} got {}",
                    Value::Array(expect.clone()),
                    Value::Array(actual)
                ),
            )
        }
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = result.row_count as u64;
        mk(got == count, format!("count: want {count} got {got}"))
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let want: Vec<&str> = cols.iter().filter_map(Value::as_str).collect();
        mk(got == want, format!("columns: want {want:?} got {got:?}"))
    } else {
        mk(true, "no expectation".into())
    }
}

/// Serialize outcomes into the JSON shape the official sql-on-fhir.org test
/// registry consumes. The report is an object keyed by test-file name, each
/// holding a `tests` array of `{ name, result: { passed, reason? } }`. This
/// matches the per-implementation `testResultsUrl` documents linked from
/// `test_report/public/implementations.json` in FHIR/sql-on-fhir.js (e.g. the
/// Medplum / Safhire reports). Submit our entry by hosting the produced file
/// and adding a row to that `implementations.json` (see README).
fn write_result_json(outcomes: &[Outcome], path: &str) {
    use serde_json::{Map, Value, json};

    let mut report: Map<String, Value> = Map::new();
    for o in outcomes {
        let mut result = json!({ "passed": o.passed });
        if !o.passed && !o.detail.is_empty() {
            result["reason"] = Value::String(o.detail.clone());
        }
        let entry = report
            .entry(o.file.clone())
            .or_insert_with(|| json!({ "tests": [] }));
        entry["tests"]
            .as_array_mut()
            .expect("tests array")
            .push(json!({ "name": o.title, "result": result }));
    }

    let text = serde_json::to_string_pretty(&Value::Object(report)).expect("serialize report");
    std::fs::write(path, text).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

fn test_cases_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("SOF_TEST_CASES_DIR") {
        return Some(PathBuf::from(d));
    }
    // Prefer the vendored official reference suite (see tests/spec/SOURCE.md).
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/spec");
    if vendored.is_dir() {
        return Some(vendored);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fhir-test-cases/sql-on-fhir")
        .canonicalize()
        .ok()
}

fn multiset_eq(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(canonical).collect();
    let mut b: Vec<String> = b.iter().map(canonical).collect();
    a.sort();
    b.sort();
    a == b
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{k:?}:{}", canonical(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn report(outcomes: &[Outcome]) {
    use std::collections::BTreeMap;
    let mut per_file: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for o in outcomes {
        let e = per_file.entry(o.file.as_str()).or_insert((0, 0));
        e.1 += 1;
        if o.passed {
            e.0 += 1;
        }
    }
    let (mut pass, mut total) = (0usize, 0usize);
    println!("\n=== SQL-on-FHIR conformance (in-memory) ===");
    for (file, (p, t)) in &per_file {
        println!("  {file:28} {p:>3}/{t}");
        pass += p;
        total += t;
    }
    println!("  {:-<34}", "");
    println!("  {:28} {pass:>3}/{total}", "TOTAL");

    let failures: Vec<&Outcome> = outcomes.iter().filter(|o| !o.passed).collect();
    if !failures.is_empty() {
        println!("\n--- failures ---");
        for o in &failures {
            let d: String = o.detail.chars().take(160).collect();
            println!("  [{}] {}: {}", o.file, o.title, d);
        }
    }
    let _ = BTreeSet::<()>::new();
}
