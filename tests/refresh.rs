//! End-to-end runs of the binary against fixtures, with a stub kiln that logs
//! its arguments and copies fixture NDJSON in place of a FHIR server.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use predicates::str::contains;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A warehouse directory with the fixture locations table, a stub kiln and a
/// config file using them. Returns (tempdir, config path).
fn setup(extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let loc = root.join("data/parquet/locations/country=SLE");
    fs::create_dir_all(&loc).unwrap();
    fs::copy(
        fixtures().join("parquet/locations/country=SLE/part-0.parquet"),
        loc.join("part-0.parquet"),
    )
    .unwrap();
    let kiln = root.join("kiln");
    fs::write(
        &kiln,
        format!(
            r#"#!/bin/sh
echo "$@" >> "{log}"
env | grep '^KILN_' | sort >> "{log}"
if [ "$1" = export ]; then
  out=""; types=""
  while [ $# -gt 0 ]; do
    case "$1" in --out) out="$2"; shift ;; --type) types="$types $2"; shift ;; esac
    shift
  done
  mkdir -p "$out"
  for t in $types; do cp "{raw}/$t.ndjson" "$out/$t.ndjson"; done
fi
"#,
            log = root.join("kiln.log").display(),
            raw = fixtures().join("raw").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&kiln, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = root.join("fhir-warehouse.toml");
    fs::write(
        &config,
        format!(
            r#"data = "data"
views = "{views}"

[server]
base = "http://fhir.test/fhir"
name = "test"

[tools]
kiln = "./kiln"

[tables]
strip_prefix = ["icr_"]

[[partition.lookup]]
key = "location_id"
table = "locations"
table_key = "id"
{extra_config}"#,
            views = fixtures().join("views").display()
        ),
    )
    .unwrap();
    (dir, config)
}

fn cmd(config: &Path) -> Command {
    let mut c = Command::cargo_bin("fhir-warehouse").unwrap();
    c.current_dir(config.parent().unwrap())
        .env_remove("DATA")
        .env_remove("VIEWS")
        .env_remove("FHIR_BASE")
        .env_remove("SERVER_NAME")
        .env_remove("KILN")
        .env_remove("REFRESH_RAW")
        .env_remove("FHIR_CLIENT_ID")
        .env_remove("FHIR_CLIENT_SECRET")
        .env_remove("FHIR_TOKEN_URL")
        .env_remove("FHIR_WAREHOUSE_CONFIG")
        .arg("refresh")
        .arg("--config")
        .arg(config);
    c
}

/// Rows of a parquet file as (column → display string) maps.
fn rows(path: &Path) -> Vec<Vec<(String, String)>> {
    let reader = SerializedFileReader::new(fs::File::open(path).unwrap()).unwrap();
    reader
        .get_row_iter(None)
        .unwrap()
        .map(|r| {
            r.unwrap()
                .get_column_iter()
                .map(|(name, field)| {
                    let v = match field {
                        Field::Str(s) => s.clone(),
                        Field::Null => "null".into(),
                        other => other.to_string(),
                    };
                    (name.clone(), v)
                })
                .collect()
        })
        .collect()
}

fn get<'a>(row: &'a [(String, String)], col: &str) -> &'a str {
    &row.iter().find(|(n, _)| n == col).unwrap().1
}

#[test]
fn views_are_downloaded_run_and_partitioned() {
    let (dir, config) = setup("");
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(contains("IcrObs (Observation) → parquet/obs (3 rows)"))
        .stdout(contains("IcrPatient (Patient) → parquet/patient (2 rows)"));
    let root = dir.path();

    // One kiln export for both types.
    let log = fs::read_to_string(root.join("kiln.log")).unwrap();
    assert!(
        log.contains(
            "export --server http://fhir.test/fhir --type Observation --type Patient --out"
        ),
        "{log}"
    );

    // Observations partitioned through the locations table; the unknown location
    // goes to the null partition.
    let obs = root.join("data/parquet/obs");
    let sle = rows(&obs.join("country=SLE/part-0.parquet"));
    assert_eq!(sle.len(), 2);
    assert!(sle.iter().all(|r| get(r, "country") == "SLE"));
    let none = rows(&obs.join("country=__HIVE_DEFAULT_PARTITION__/part-0.parquet"));
    assert_eq!(none.len(), 1);
    assert_eq!(get(&none[0], "id"), "o3");
    assert_eq!(get(&none[0], "country"), "null");

    // Native temporal types: a date column, partial dates floored.
    let patients =
        rows(&root.join("data/parquet/patient/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet"));
    let born: Vec<&str> = patients.iter().map(|r| get(r, "born")).collect();
    assert_eq!(born, ["2020-05-01", "2021-01-01"]);

    // The views are saved as plain ViewDefinitions; the manifest counts the tables.
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/views/IcrObs.json")).unwrap())
            .unwrap();
    assert_eq!(saved["resourceType"], "ViewDefinition");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
    assert_eq!(manifest["tables"]["patient"]["rows"], 2);
    assert_eq!(manifest["tables"]["locations"]["rows"], 2);
    assert_eq!(manifest["server_name"], "test");
    assert_eq!(manifest["views"]["IcrObs"]["resource"], "Observation");
    assert!(root.join("data/raw/test/last-refresh.json").exists());
}

#[test]
fn reuse_raw_skips_the_download() {
    let (dir, config) = setup("");
    let raw = dir.path().join("data/raw/test");
    fs::create_dir_all(&raw).unwrap();
    for t in ["Observation", "Patient"] {
        fs::copy(
            fixtures().join(format!("raw/{t}.ndjson")),
            raw.join(format!("{t}.ndjson")),
        )
        .unwrap();
    }
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    assert!(!dir.path().join("kiln.log").exists());
}

#[test]
fn locations_runs_kiln_with_credentials() {
    let (dir, config) = setup("");
    cmd(&config)
        .args([
            "--only",
            "locations",
            "--full",
            "--token-url",
            "https://auth.test/token",
        ])
        .env("FHIR_CLIENT_ID", "warehouse")
        .env("FHIR_CLIENT_SECRET", "s3cret")
        .assert()
        .success();
    let log = fs::read_to_string(dir.path().join("kiln.log")).unwrap();
    assert!(
        log.contains("run --server http://fhir.test/fhir --snapshot"),
        "{log}"
    );
    assert!(log.contains("data/raw/test/locations --out"), "{log}");
    assert!(log.contains("--full"), "{log}");
    assert!(log.contains("KILN_CLIENT_ID=warehouse"), "{log}");
    assert!(
        log.contains("KILN_TOKEN_URL=https://auth.test/token"),
        "{log}"
    );
}

#[test]
fn flags_override_the_config() {
    let (dir, config) = setup("");
    cmd(&config)
        .args([
            "--only",
            "views",
            "--server",
            "http://other.test/fhir",
            "--server-name",
            "other",
        ])
        .assert()
        .success();
    let log = fs::read_to_string(dir.path().join("kiln.log")).unwrap();
    assert!(log.contains("--server http://other.test/fhir"), "{log}");
    assert!(
        dir.path()
            .join("data/raw/other/Observation.ndjson")
            .exists()
    );
}

#[test]
fn hooks_run_after_their_step_only() {
    let (dir, config) = setup(
        r#"
[[hooks]]
name = "always"
run = ["sh", "-c", "echo \"$DATA $SERVER_NAME $REFRESHED_AT\" > always.txt"]

[[hooks]]
name = "after-locations"
after = "locations"
run = ["sh", "-c", "touch after-locations.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks"])
        .assert()
        .success();
    let always = fs::read_to_string(dir.path().join("always.txt")).unwrap();
    assert!(always.contains("data test 20"), "{always}");
    assert!(!dir.path().join("after-locations.txt").exists());
}

#[test]
fn missing_tools_fail_before_anything_runs() {
    let (dir, config) = setup("");
    cmd(&config)
        .args(["--only", "views", "--kiln", "/no/such/kiln"])
        .assert()
        .failure()
        .stderr(contains(
            "not found: /no/such/kiln (for locations and downloads)",
        ));
    assert!(!dir.path().join("data/parquet/obs").exists());
}

#[test]
fn push_needs_a_push_section() {
    let (_dir, config) = setup("");
    cmd(&config)
        .args(["--only", "manifest", "--push"])
        .assert()
        .failure()
        .stderr(contains("[push]"));
}

#[test]
fn a_broken_view_stops_the_run() {
    let (dir, config) = setup("");
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("bad.json"),
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--views"])
        .arg(&views)
        .assert()
        .failure()
        .stderr(contains("bad.json"));
}
