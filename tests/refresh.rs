//! End-to-end runs of `fhir-pipes refresh` against fixtures, with a fake FHIR
//! server serving the fixture NDJSON.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use predicates::str::contains as has;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

fn read_ndjson(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn bundle(resources: Vec<Value>) -> String {
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Serve each fixture type, once, as a single search page.
fn serve_types(server: &Server, types: &[&str]) {
    for t in types {
        let resources = read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")));
        server.expect(
            Expectation::matching(request::method_path("GET", format!("/fhir/{t}")))
                .times(1)
                .respond_with(status_code(200).body(bundle(resources))),
        );
    }
}

/// A warehouse directory with the fixture locations table and a config file
/// pointing at `base`. Returns (tempdir, config path).
fn setup(base: &str, extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let loc = root.join("data/parquet/locations/country=SLE");
    fs::create_dir_all(&loc).unwrap();
    fs::copy(
        fixtures().join("parquet/locations/country=SLE/part-0.parquet"),
        loc.join("part-0.parquet"),
    )
    .unwrap();
    let config = root.join("fhir-pipes.toml");
    fs::write(
        &config,
        format!(
            r#"# Literal strings: a Windows path has backslashes.
data = "data"
views = '{views}'

[server]
base = "{base}"
name = "test"

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

/// A server address nothing listens on, for runs that must not reach a server.
const NOWHERE: &str = "http://127.0.0.1:9/fhir";

fn cmd(config: &Path) -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.current_dir(config.parent().unwrap());
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_VIEWS",
        "FHIR_PIPES_SERVER",
        "FHIR_PIPES_SERVER_NAME",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
    ] {
        c.env_remove(var);
    }
    c.arg("refresh").arg("--config").arg(config);
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
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("export Observation, Patient"))
        .stdout(has("IcrObs (Observation) → parquet/obs (3 rows)"))
        .stdout(has("IcrPatient (Patient) → parquet/patient (2 rows)"));
    let root = dir.path();
    assert_eq!(
        read_ndjson(&root.join("data/raw/test/Observation.ndjson")).len(),
        3
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
    let saved: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/views/IcrObs.json")).unwrap())
            .unwrap();
    assert_eq!(saved["resourceType"], "ViewDefinition");
    let manifest: Value =
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
    let (dir, config) = setup(NOWHERE, "");
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
        .success()
        .stdout(has("parquet/obs (3 rows)"));
}

#[test]
fn locations_are_extracted_with_client_credentials() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("POST", "/token"))
            .times(1)
            .respond_with(json_encoded(
                json!({"access_token": "tok", "expires_in": 3600}),
            )),
    );
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot");
    let locations: Vec<Value> = read_ndjson(&snapshot.join("locations.ndjson"))
        .into_iter()
        .map(|mut l| {
            l["meta"] = json!({"lastUpdated": "2026-01-01T00:00:00Z"});
            l
        })
        .collect();
    let n = locations.len();
    let bearer = || request::headers(contains(("authorization", "Bearer tok")));
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Location"),
            bearer()
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(locations))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            bearer()
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![]))),
    );
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    let token_url = server.url_str("/token");
    cmd(&config)
        .args(["--only", "locations", "--full", "--token-url", &token_url])
        .env("FHIR_PIPES_CLIENT_ID", "warehouse")
        .env("FHIR_PIPES_CLIENT_SECRET", "s3cret")
        .assert()
        .success();
    let root = dir.path();
    let snap = read_ndjson(&root.join("data/raw/test/locations/locations.ndjson"));
    assert_eq!(snap.len(), n);
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["extract"]["count"], n);
    assert_eq!(
        manifest["extract"]["fhir_pipes_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 0);
}

#[test]
fn flags_override_the_config() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--only", "views", "--server-name", "other", "--server"])
        .arg(server.url_str("/fhir"))
        .assert()
        .success();
    assert!(
        dir.path()
            .join("data/raw/other/Observation.ndjson")
            .exists()
    );
}

#[test]
// The hooks run through sh.
#[cfg(unix)]
fn hooks_run_after_their_step_only() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
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
    let (dir, config) = setup(NOWHERE, "\n[tools]\nduckdb = \"/no/such/duckdb\"\n");
    cmd(&config)
        .args(["--only", "views,tiles"])
        .assert()
        .failure()
        .stderr(has("not found: /no/such/duckdb (for tiles)"));
    assert!(!dir.path().join("data/parquet/obs").exists());
}

#[test]
fn push_needs_a_push_section() {
    let (_dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--only", "manifest", "--push"])
        .assert()
        .failure()
        .stderr(has("[push]"));
}

#[test]
fn a_broken_view_stops_the_run() {
    let (dir, config) = setup(NOWHERE, "");
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
        .stderr(has("bad.json"));
}

/// The fixture resources of `t`, each with a meta.lastUpdated.
fn stamped(t: &str, at: &str) -> Vec<Value> {
    read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")))
        .into_iter()
        .map(|mut r| {
            r["meta"] = json!({"lastUpdated": at});
            r
        })
        .collect()
}

fn full_search(
    t: &str,
) -> impl httptest::matchers::Matcher<httptest::http::Request<httptest::bytes::Bytes>> + use<> {
    all_of![
        request::method_path("GET", format!("/fhir/{t}")),
        request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        request::query(url_decoded(not(contains(key("_summary"))))),
    ]
}

#[test]
fn a_second_refresh_downloads_only_what_changed() {
    let mut server = Server::run();
    for t in ["Observation", "Patient"] {
        server.expect(
            Expectation::matching(full_search(t))
                .times(1)
                .respond_with(status_code(200).body(bundle(stamped(t, "2026-10-01T00:00:00Z")))),
        );
    }
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    cmd(&config).args(["--only", "views"]).assert().success();
    server.verify_and_clear();

    // Second run: each type is searched from its watermark. One Observation
    // changed; the counts still match, so nothing is downloaded in full.
    let mut changed = stamped("Observation", "2026-10-02T00:00:00Z").remove(0);
    changed["valueInteger"] = json!(30);
    let since = ("_lastUpdated", "ge2026-10-01T00:00:00Z");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Observation"),
            request::query(url_decoded(contains(since))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![changed]))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Patient"),
            request::query(url_decoded(contains(since))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![]))),
    );
    for (t, n) in [("Observation", 3), ("Patient", 2)] {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", format!("/fhir/{t}")),
                request::query(url_decoded(contains(("_summary", "count")))),
            ])
            .times(1)
            .respond_with(status_code(200).body(
                json!({"resourceType": "Bundle", "type": "searchset", "total": n}).to_string(),
            )),
        );
    }
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("Observation: 0 new, 1 updated or fetched again"))
        .stdout(has("IcrObs (Observation) → parquet/obs (3 rows)"));
    let sle = rows(
        &dir.path()
            .join("data/parquet/obs/country=SLE/part-0.parquet"),
    );
    assert!(sle.iter().any(|r| get(r, "value") == "30"));
    server.verify_and_clear();

    // --full downloads everything again.
    for t in ["Observation", "Patient"] {
        server.expect(
            Expectation::matching(full_search(t))
                .times(1)
                .respond_with(status_code(200).body(bundle(stamped(t, "2026-10-03T00:00:00Z")))),
        );
    }
    cmd(&config)
        .args(["--only", "views", "--full"])
        .assert()
        .success();
}
