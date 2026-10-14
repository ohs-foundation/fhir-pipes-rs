//! SQL-on-FHIR v2 content-test conformance harness.
//!
//! Loads the official `sql-on-fhir-v2` content tests (the `tests/*.json` files),
//! materialises each test's resources into a PostgreSQL table shaped like the one
//! [`crate::ViewRunner`] expects, runs every ViewDefinition through
//! [`SqlGenerator`], executes the generated SQL and compares the produced rows
//! (as JSON) against the expected output.
//!
//! The harness is opt-in: it runs only when `SOF_CONFORMANCE_DB` points at a
//! reachable PostgreSQL instance, so the default `cargo test` (and CI without a
//! database) skips it cleanly. The test-case directory defaults to the sibling
//! `fhir-test-cases/sql-on-fhir` checkout and can be overridden with
//! `SOF_TEST_CASES_DIR`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use octofhir_sof::{SqlGenerator, ViewDefinition};
use serde_json::Value;
use sqlx_core::connection::Connection;
use sqlx_core::error::Error as SqlxError;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::PgConnection;

const SETUP_SQL: &str = r#"
CREATE OR REPLACE FUNCTION fhir_ref_id(reference TEXT) RETURNS TEXT
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE
    WHEN reference IS NULL OR reference = '' THEN NULL
    WHEN reference LIKE '#%' OR reference LIKE 'urn:%' OR reference LIKE '%://%' THEN NULL
    WHEN array_length(string_to_array(ltrim(reference, '/'), '/'), 1) < 2 THEN NULL
    ELSE (string_to_array(ltrim(reference, '/'), '/'))[2]
  END;
$$;

CREATE OR REPLACE FUNCTION fhir_ref_type(reference TEXT) RETURNS TEXT
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE
    WHEN reference IS NULL OR reference = '' THEN NULL
    WHEN reference LIKE '#%' OR reference LIKE 'urn:%' OR reference LIKE '%://%' THEN NULL
    WHEN array_length(string_to_array(ltrim(reference, '/'), '/'), 1) < 2 THEN NULL
    ELSE (string_to_array(ltrim(reference, '/'), '/'))[1]
  END;
$$;
"#;

struct Outcome {
    file: String,
    title: String,
    passed: bool,
    detail: String,
}

/// Both DB-backed tests share the `conf` schema and the helper functions, so
/// they must not run concurrently (`CREATE OR REPLACE FUNCTION` races with
/// "tuple concurrently updated"). Serialize them.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn sql_on_fhir_conformance() {
    let Ok(db_url) = std::env::var("SOF_CONFORMANCE_DB") else {
        eprintln!("SOF_CONFORMANCE_DB not set — skipping SQL-on-FHIR conformance suite");
        return;
    };
    let _guard = DB_LOCK.lock().await;

    let dir = test_cases_dir();
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

    let mut conn = PgConnection::connect(&db_url)
        .await
        .expect("connect to SOF_CONFORMANCE_DB");
    sqlx_core::raw_sql::raw_sql(SETUP_SQL)
        .execute(&mut conn)
        .await
        .expect("install helper functions");

    let mut outcomes = Vec::new();
    for file in &files {
        run_file(&mut conn, file, &mut outcomes).await;
    }

    report(&outcomes);

    // The PostgreSQL generator passes the full vendored suite; gate on it when a
    // database is available (the suite is skipped entirely without one).
    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} PostgreSQL conformance cases failed");
}

/// Extra cases not in the official suite (e.g. contained-resource keying),
/// run against PostgreSQL through the same generator + helper path. Gated on
/// `SOF_CONFORMANCE_DB` exactly like the main suite.
#[tokio::test]
async fn extra_cases() {
    let Ok(db_url) = std::env::var("SOF_CONFORMANCE_DB") else {
        eprintln!("SOF_CONFORMANCE_DB not set — skipping extra SQL cases");
        return;
    };
    let _guard = DB_LOCK.lock().await;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/extra");
    if !dir.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let mut conn = PgConnection::connect(&db_url)
        .await
        .expect("connect to SOF_CONFORMANCE_DB");
    sqlx_core::raw_sql::raw_sql(SETUP_SQL)
        .execute(&mut conn)
        .await
        .expect("install helper functions");

    let mut outcomes = Vec::new();
    for file in &files {
        run_file(&mut conn, file, &mut outcomes).await;
    }
    report(&outcomes);

    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} extra PostgreSQL cases failed");
}

fn test_cases_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SOF_TEST_CASES_DIR") {
        return PathBuf::from(d);
    }
    // Prefer the vendored official reference suite (see tests/spec/SOURCE.md).
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/spec");
    if vendored.is_dir() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fhir-test-cases/sql-on-fhir")
        .canonicalize()
        .expect("default test-cases dir not found; set SOF_TEST_CASES_DIR")
}

async fn run_file(conn: &mut PgConnection, path: &PathBuf, outcomes: &mut Vec<Outcome>) {
    let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("parse {file_name}: {e}"));

    let resources = doc.get("resources").and_then(Value::as_array).cloned();
    if let Err(e) = load_resources(conn, resources.as_deref().unwrap_or(&[])).await {
        // Whole file unusable; record one failure and move on.
        outcomes.push(Outcome {
            file: file_name.clone(),
            title: "<setup>".into(),
            passed: false,
            detail: format!("resource load failed: {e}"),
        });
        return;
    }

    let empty = Vec::new();
    let tests = doc.get("tests").and_then(Value::as_array).unwrap_or(&empty);
    for test in tests {
        let title = test
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>")
            .to_string();
        let outcome = run_test(conn, &file_name, &title, test).await;
        outcomes.push(outcome);
    }
}

/// (Re)create the per-file schema and insert every resource into a table named
/// after its (lower-cased) resourceType, matching `ViewRunner`'s expectations.
async fn load_resources(conn: &mut PgConnection, resources: &[Value]) -> Result<(), SqlxError> {
    sqlx_core::raw_sql::raw_sql(
        "DROP SCHEMA IF EXISTS conf CASCADE; CREATE SCHEMA conf; SET search_path TO conf, public;",
    )
    .execute(&mut *conn)
    .await?;

    let mut created: BTreeSet<String> = BTreeSet::new();
    for res in resources {
        let Some(rt) = res.get("resourceType").and_then(Value::as_str) else {
            continue;
        };
        let table = rt.to_lowercase();
        if created.insert(table.clone()) {
            let ddl = format!(
                "CREATE TABLE conf.\"{table}\" (id text, resource jsonb, resource_type text, status text)"
            );
            sqlx_core::raw_sql::raw_sql(AssertSqlSafe(ddl))
                .execute(&mut *conn)
                .await?;
        }
        let id = res
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        sqlx_core::query::query(AssertSqlSafe(format!(
            "INSERT INTO conf.\"{table}\" (id, resource, resource_type, status) VALUES ($1, $2, $3, 'created')"
        )))
        .bind(id)
        .bind(res)
        .bind(rt)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn run_test(conn: &mut PgConnection, file: &str, title: &str, test: &Value) -> Outcome {
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
        Err(e) => {
            return mk(expect_error, format!("parse error: {e}"));
        }
    };

    let sql = match SqlGenerator::new().generate(&view) {
        Ok(g) => g.sql,
        Err(e) => {
            return mk(expect_error, format!("generate error: {e}"));
        }
    };

    let wrapped = format!("SELECT coalesce(jsonb_agg(t), '[]'::jsonb) FROM ({sql}) t");
    let rows: Result<(Value,), SqlxError> = sqlx_core::query_as::query_as(AssertSqlSafe(wrapped))
        .fetch_one(&mut *conn)
        .await;

    let actual = match rows {
        Ok((v,)) => v,
        Err(e) => {
            return mk(expect_error, format!("exec error: {e}"));
        }
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
    } else {
        mk(true, "no expectation".into())
    }
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

/// Canonical JSON string with sorted object keys (serde_json::Value sorts when
/// using a BTreeMap; here we re-serialise objects key-sorted).
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
    println!("\n=== SQL-on-FHIR conformance ===");
    for (file, (p, t)) in &per_file {
        println!("  {file:28} {p:>3}/{t}");
        pass += p;
        total += t;
    }
    println!("  {:-<34}", "");
    println!("  {:28} {pass:>3}/{total}", "TOTAL");

    println!("\n--- failures ---");
    for o in outcomes.iter().filter(|o| !o.passed) {
        let d: String = o.detail.chars().take(160).collect();
        println!("  [{}] {}: {}", o.file, o.title, d);
    }
}
