//! Binary-level tests for `fhir-pipes views`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

fn views() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_DATABASE_URL").arg("views");
    c
}

#[test]
fn run_writes_csv_to_stdout() {
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .arg("--input")
        .arg(fixtures().join("raw/Observation.ndjson"))
        .assert()
        .success()
        .stdout(contains("id,location_id,value,issued"))
        .stdout(contains("o2,loc-kono,5,2026-02-03T04:05:06Z"));
}

#[test]
fn run_streams_ndjson() {
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .arg("--input")
        .arg(fixtures().join("raw/Observation.ndjson"))
        .args(["--output", "ndjson"])
        .assert()
        .success()
        .stdout(contains(r#""id":"o3""#))
        .stderr(contains("3 rows"));
}

#[test]
fn run_a_directory_writes_one_parquet_file_per_view() {
    let out = tempfile::tempdir().unwrap();
    // The directory also holds a JSON file that is not a view; it is skipped.
    views()
        .arg("run")
        .arg(fixtures().join("views"))
        .arg("--input")
        .arg(fixtures().join("raw"))
        .args([
            "--output",
            "parquet",
            "--parquet-temporal",
            "native",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success()
        .stderr(contains("IcrObs"))
        .stderr(contains("(3 rows)"));
    assert!(out.path().join("IcrObs.parquet").exists());
    assert!(out.path().join("IcrPatient.parquet").exists());
}

#[test]
fn a_directory_needs_out() {
    views()
        .arg("run")
        .arg(fixtures().join("views"))
        .arg("--input")
        .arg(fixtures().join("raw"))
        .assert()
        .failure()
        .stderr(contains("--out <dir>"));
}

#[test]
fn sql_emits_ddl() {
    views()
        .arg("sql")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .args(["--ddl", "--dialect", "duckdb", "--table", "obs"])
        .assert()
        .success()
        .stdout(contains("CREATE TABLE"))
        .stdout(contains("obs"));
}

#[test]
fn validate_passes_a_good_view_and_fails_a_bad_one() {
    views()
        .arg("validate")
        .arg(fixtures().join("views/Binary-IcrPatient.json"))
        .assert()
        .success()
        .stdout(contains("valid"));

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    fs::write(
        &bad,
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    views()
        .args(["--no-color", "validate", "--json"])
        .arg(&bad)
        .assert()
        .failure()
        .stdout(contains("FH08"));
}

#[test]
fn lint_shareable_runs_offline() {
    views()
        .args(["lint", "--shareable"])
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .assert()
        .success();
}

#[test]
fn test_runs_the_official_suite() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/sof/tests/spec");
    views()
        .args(["--no-color", "test"])
        .arg(spec)
        .assert()
        .success()
        .stdout(contains("144/144 passed"));
}
