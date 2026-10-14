//! SQL-on-FHIR v2 content-test conformance harness for the DuckDB backend.
//!
//! Mirrors `tests/conformance.rs` (PostgreSQL) but generates SQL with
//! [`Dialect::DuckDb`] and executes it through the local `duckdb` CLI against an
//! in-memory database. The harness is opt-in: it runs only when a `duckdb`
//! binary is on `PATH`, so the default `cargo test` (and CI without DuckDB)
//! skips it cleanly.
//!
//! Each test file's resources are loaded into a table per (lower-cased)
//! resourceType with columns `(id VARCHAR, resource JSON, resource_type
//! VARCHAR, status VARCHAR)` and `status = 'created'`, matching the shape the
//! generated SQL expects. The generated SELECT is wrapped in
//! `SELECT coalesce(json_group_array(t), '[]') FROM (<sql>) t` and the produced
//! rows (as JSON) are compared with the expected output using the same
//! multiset/canonical comparison the PostgreSQL harness uses.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use octofhir_sof::{Dialect, SqlGenerator, ViewDefinition};
use serde_json::Value;

struct Outcome {
    file: String,
    title: String,
    passed: bool,
    detail: String,
}

/// Cases that legitimately cannot pass on the DuckDB backend, keyed by
/// `(file, title)`, with a reason. Kept empty unless a feature genuinely cannot
/// map; populated entries are printed in the report and excluded from the gate.
const SKIP: &[(&str, &str, &str)] = &[];

fn duckdb_available() -> bool {
    Command::new("duckdb")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn sql_on_fhir_conformance_duckdb() {
    if !duckdb_available() {
        eprintln!("duckdb binary not found on PATH — skipping DuckDB conformance suite");
        return;
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(test_cases_dir())
        .expect("read tests/spec")
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
        run_file(file, &mut outcomes);
    }
    // The extra (non-official) cases, e.g. contained-resource keying.
    let extra = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/extra");
    if extra.is_dir() {
        let mut efiles: Vec<PathBuf> = std::fs::read_dir(&extra)
            .expect("read tests/extra")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        efiles.sort();
        for file in &efiles {
            run_file(file, &mut outcomes);
        }
    }

    report(&outcomes);

    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} DuckDB conformance cases failed");
}

fn test_cases_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SOF_TEST_CASES_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/spec")
}

fn run_file(path: &PathBuf, outcomes: &mut Vec<Outcome>) {
    let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("parse {file_name}: {e}"));

    let resources = doc
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let setup = load_sql(&resources);

    let empty = Vec::new();
    let tests = doc.get("tests").and_then(Value::as_array).unwrap_or(&empty);
    for test in tests {
        let title = test
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>")
            .to_string();
        if let Some((_, _, reason)) = SKIP.iter().find(|(f, t, _)| *f == file_name && *t == title) {
            outcomes.push(Outcome {
                file: file_name.clone(),
                title,
                passed: true,
                detail: format!("SKIPPED: {reason}"),
            });
            continue;
        }
        let outcome = run_test(&file_name, &title, test, &setup);
        outcomes.push(outcome);
    }
}

/// Build the `CREATE TABLE` + `INSERT` statements that materialise every
/// resource into a table named after its (lower-cased) resourceType.
fn load_sql(resources: &[Value]) -> String {
    let mut created: BTreeSet<String> = BTreeSet::new();
    let mut sql = String::new();
    for res in resources {
        let Some(rt) = res.get("resourceType").and_then(Value::as_str) else {
            continue;
        };
        let table = rt.to_lowercase();
        if created.insert(table.clone()) {
            sql.push_str(&format!(
                "CREATE TABLE \"{table}\" (id VARCHAR, resource JSON, resource_type VARCHAR, status VARCHAR);\n"
            ));
        }
        let id = res.get("id").and_then(Value::as_str).unwrap_or("");
        let json = serde_json::to_string(res).unwrap();
        sql.push_str(&format!(
            "INSERT INTO \"{table}\" VALUES ('{}', '{}'::JSON, '{}', 'created');\n",
            sql_quote(id),
            sql_quote(&json),
            sql_quote(rt),
        ));
    }
    sql
}

fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn run_test(file: &str, title: &str, test: &Value, setup: &str) -> Outcome {
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mk = |passed: bool, detail: String| Outcome {
        file: file.to_string(),
        title: title.to_string(),
        passed,
        detail,
    };

    let view_json = test.get("view").cloned().unwrap_or(Value::Null);
    let view = match ViewDefinition::from_json(&view_json) {
        Ok(v) => v,
        Err(e) => return mk(expect_error, format!("parse error: {e}")),
    };

    let sql = match SqlGenerator::new()
        .with_dialect(Dialect::DuckDb)
        .generate(&view)
    {
        Ok(g) => g.sql,
        Err(e) => return mk(expect_error, format!("generate error: {e}")),
    };

    let wrapped = format!("SELECT coalesce(json_group_array(t), '[]') FROM ({sql}) t;");
    let script = format!("{setup}.mode json\n{wrapped}\n");

    let actual = match run_duckdb(&script) {
        Ok(v) => v,
        Err(e) => return mk(expect_error, format!("exec error: {e}")),
    };

    if expect_error {
        return mk(
            false,
            "expected error but generation+execution succeeded".into(),
        );
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual_arr = actual.as_array().cloned().unwrap_or_default();
        if multiset_eq(&actual_arr, expect) {
            mk(true, String::new())
        } else {
            mk(
                false,
                format!(
                    "rows differ: expected {} got {}",
                    Value::Array(expect.clone()),
                    actual
                ),
            )
        }
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: BTreeSet<String> = actual
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let want: BTreeSet<String> = cols
            .iter()
            .filter_map(|c| c.as_str().map(String::from))
            .collect();
        mk(got == want, format!("columns: want {want:?} got {got:?}"))
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = actual.as_array().map(|a| a.len() as u64).unwrap_or(0);
        mk(got == count, format!("want {count} rows, got {got}"))
    } else {
        mk(true, "no expectation".into())
    }
}

/// Run a SQL script through `duckdb :memory:` and parse the JSON-mode result of
/// the final (wrapped) query, returning the inner JSON array value.
fn run_duckdb(script: &str) -> Result<Value, String> {
    let mut child = Command::new("duckdb")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn duckdb: {e}"))?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .map_err(|e| format!("write stdin: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait duckdb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "duckdb exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // `.mode json` emits an array of row objects. The wrapped query yields one
    // row with a single column whose value is the result JSON array.
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!("empty duckdb output (stderr: {})", stderr.trim()));
    }
    let rows: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("parse duckdb json: {e}: {trimmed}"))?;
    let inner = rows
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_object)
        .and_then(|o| o.values().next())
        .cloned()
        .ok_or_else(|| format!("unexpected duckdb output shape: {trimmed}"))?;
    Ok(inner)
}

/// Order-insensitive comparison of two row sets.
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

/// Canonical JSON string with sorted object keys, for set comparison.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{:?}:{}", k, canonical(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        // DuckDB's DECIMAL arithmetic serialises integral results as `5.0`
        // where PostgreSQL's jsonb yields `5`; normalise numbers so an integral
        // value compares equal regardless of representation.
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.fract() == 0.0 && f.is_finite() {
                format!("{}", f as i128)
            } else {
                format!("{f}")
            }
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
    println!("\n=== SQL-on-FHIR conformance (DuckDB) ===");
    for (file, (p, t)) in &per_file {
        println!("  {file:28} {p:>3}/{t}");
        pass += p;
        total += t;
    }
    println!("  {:-<34}", "");
    println!("  {:28} {pass:>3}/{total}", "TOTAL");

    let skipped: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.detail.starts_with("SKIPPED"))
        .collect();
    if !skipped.is_empty() {
        println!("\n--- skipped ---");
        for o in &skipped {
            println!("  [{}] {}: {}", o.file, o.title, o.detail);
        }
    }

    println!("\n--- failures ---");
    for o in outcomes.iter().filter(|o| !o.passed) {
        let d: String = o.detail.chars().take(200).collect();
        println!("  [{}] {}: {}", o.file, o.title, d);
    }
}
