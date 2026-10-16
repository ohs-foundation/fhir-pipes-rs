//! Exit codes of `refresh` and `views`, by the rule the other commands
//! follow: 2 for an error the operator can fix (flags, config, views,
//! input), 1 for a failure while running (network, server, tools, I/O).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains as has;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

/// `refresh` with `config` (written to a fresh directory) and no
/// FHIR_PIPES_* settings from the environment.
fn refresh(config: &str) -> (tempfile::TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fhir-pipes.toml");
    fs::write(&path, config).unwrap();
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.current_dir(dir.path());
    for (var, _) in std::env::vars() {
        if var.starts_with("FHIR_PIPES_") {
            c.env_remove(var);
        }
    }
    c.arg("refresh").arg("--config").arg(&path);
    (dir, c)
}

fn views() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_DATABASE_URL").arg("views");
    c
}

/// A data directory and the fixture views, read from files.
fn files_config() -> String {
    format!(
        "data = \"data\"\nviews = '{}'\n",
        fixtures().join("views").display()
    )
}

#[test]
fn refresh_config_errors_exit_2() {
    // An unknown key.
    let (_dir, mut c) = refresh("data = \"data\"\nnot_a_key = 1\n");
    c.assert().code(2).stderr(has("not_a_key"));

    // A hook after a step that is not one.
    let (_dir, mut c) =
        refresh("data = \"data\"\n[[hooks]]\nname = \"h\"\nafter = \"vews\"\nrun = [\"true\"]\n");
    c.assert()
        .code(2)
        .stderr(has("a hook runs after one of locations, views"));

    // A config file that is not there.
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.args(["refresh", "--config", "/no/such/fhir-pipes.toml"])
        .assert()
        .code(2)
        .stderr(has("reading /no/such/fhir-pipes.toml"));
}

#[test]
fn refresh_flag_errors_exit_2() {
    let (_dir, mut c) = refresh("data = \"data\"\n");
    c.args(["--only", "catalog"])
        .assert()
        .code(2)
        .stderr(has("--only catalog needs a [catalog] section"));

    let (_dir, mut c) = refresh(&files_config());
    c.args(["--only", "views", "--input", "/no/such/input"])
        .assert()
        .code(2)
        .stderr(has("input /no/such/input not found"));
}

#[test]
fn refresh_invalid_view_exits_2() {
    let (dir, mut c) = refresh(&files_config());
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("bad.json"),
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    c.args(["--only", "views", "--views"])
        .arg(&views)
        .arg("--input")
        .arg(fixtures().join("raw"))
        .assert()
        .code(2)
        .stderr(has("bad.json"));
}

#[test]
fn refresh_unknown_profile_key_exits_2() {
    let (dir, mut c) = refresh("data = \"data\"\nprofile = 'bad.toml'\n");
    fs::write(dir.path().join("bad.toml"), "nonsense_key = 1\n").unwrap();
    let input = dir.path().join("in");
    fs::create_dir_all(&input).unwrap();
    fs::write(
        input.join("Location.ndjson"),
        r#"{"resourceType":"Location","id":"l1","name":"A","position":{"longitude":1,"latitude":2}}"#,
    )
    .unwrap();
    c.args(["--only", "locations", "--input"])
        .arg(&input)
        .assert()
        .code(2)
        .stderr(has("nonsense_key"));
}

#[test]
fn refresh_runtime_failures_exit_1() {
    // A server that does not answer.
    let (_dir, mut c) = refresh(&format!(
        "{}[server]\nbase = \"http://127.0.0.1:9/fhir\"\n",
        files_config()
    ));
    c.args(["--only", "views"]).assert().code(1);

    // A tool that is not there.
    let (_dir, mut c) = refresh(&format!(
        "{}[tools]\nduckdb = \"/no/such/duckdb\"\n",
        files_config()
    ));
    c.args(["--only", "views,tiles"])
        .assert()
        .code(1)
        .stderr(has("not found: /no/such/duckdb"));
}

#[test]
fn views_usage_errors_exit_2() {
    // A view file that is not there.
    views()
        .args(["run", "/no/such/view.json", "--input"])
        .arg(fixtures().join("raw/Observation.ndjson"))
        .assert()
        .code(2)
        .stderr(has("reading ViewDefinition /no/such/view.json"));

    // A file that is not a ViewDefinition.
    views()
        .arg("validate")
        .arg(fixtures().join("views/Patient-notaview.json"))
        .assert()
        .code(2);

    // Neither --input nor --db.
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .assert()
        .code(2)
        .stderr(has("pass --input"));

    // An --input that is not there.
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .args(["--input", "/no/such/input.json"])
        .assert()
        .code(2);

    // An unknown SQL dialect.
    views()
        .arg("sql")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .args(["--dialect", "nope"])
        .assert()
        .code(2);
}

#[test]
fn views_runtime_failures_exit_1() {
    // The output cannot be written.
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .arg("--input")
        .arg(fixtures().join("raw/Observation.ndjson"))
        .args(["--output", "csv", "--out", "/no/such/dir/out.csv"])
        .assert()
        .code(1)
        .stderr(has("creating /no/such/dir/out.csv"));
}

#[test]
fn views_findings_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let view = dir.path().join("bad.json");
    fs::write(
        &view,
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    views().arg("validate").arg(&view).assert().code(1);
}
