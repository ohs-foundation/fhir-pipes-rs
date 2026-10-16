//! End-to-end runs of `fhir-pipes refresh` against fixtures, with a fake FHIR
//! server serving the fixture NDJSON.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use predicates::prelude::*;
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
profile = '{profile}'

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
            views = fixtures().join("views").display(),
            profile = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/profiles/icr.toml")
                .display()
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
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
        "FHIR_PIPES_GOOGLE_AUTH",
        "FHIR_PIPES_PROFILE",
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

/// A Bulk Data export behind `/status/<job>`: the kick-off for Observation
/// and Patient (with `_since` when given), a status URL that is done at
/// once, each of `files` as one output, `deleted` as one Bundle, and the
/// DELETE after the files are read.
fn serve_bulk(
    server: &Server,
    job: &str,
    since: Option<&str>,
    transaction_time: &str,
    files: &[(&str, Vec<Value>)],
    deleted: &[&str],
) {
    let kick_off = all_of![
        request::method_path("GET", "/fhir/$export"),
        request::query(url_decoded(contains(("_type", "Observation,Patient")))),
        request::headers(contains(("prefer", "respond-async"))),
    ];
    let kick_off = match since {
        Some(s) => Expectation::matching(all_of![
            kick_off,
            request::query(url_decoded(contains(("_since", s.to_string())))),
        ]),
        None => Expectation::matching(all_of![
            kick_off,
            request::query(url_decoded(not(contains(key("_since"))))),
        ]),
    };
    let status = format!("/status/{job}");
    server.expect(
        kick_off.times(1).respond_with(
            status_code(202).insert_header("Content-Location", server.url_str(&status)),
        ),
    );
    let output: Vec<Value> = files
        .iter()
        .map(
            |(t, _)| json!({"type": t, "url": server.url_str(&format!("/files/{job}/{t}.ndjson"))}),
        )
        .collect();
    let mut manifest = json!({
        "transactionTime": transaction_time,
        "request": server.url_str("/fhir/$export"),
        "requiresAccessToken": false,
        "output": output,
        "error": [],
    });
    if !deleted.is_empty() {
        let path = format!("/files/{job}/deleted.ndjson");
        manifest["deleted"] = json!([{"type": "Bundle", "url": server.url_str(&path)}]);
        let bundle = json!({"resourceType": "Bundle", "type": "transaction",
            "entry": deleted.iter().map(|r| json!({"request": {"method": "DELETE", "url": r}})).collect::<Vec<_>>()});
        server.expect(
            Expectation::matching(request::method_path("GET", path))
                .times(1)
                .respond_with(status_code(200).body(format!("{bundle}\n"))),
        );
    }
    server.expect(
        Expectation::matching(request::method_path("GET", status.clone()))
            .times(1)
            .respond_with(status_code(200).body(manifest.to_string())),
    );
    for (t, resources) in files {
        let body: String = resources.iter().map(|r| format!("{r}\n")).collect();
        server.expect(
            Expectation::matching(request::method_path(
                "GET",
                format!("/files/{job}/{t}.ndjson"),
            ))
            .times(1)
            .respond_with(status_code(200).body(body)),
        );
    }
    server.expect(
        Expectation::matching(request::method_path("DELETE", status))
            .times(1)
            .respond_with(status_code(202)),
    );
}

#[test]
fn views_read_a_bulk_export_as_they_read_a_search() {
    let mut server = Server::run();
    let fixture = |t: &str| read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")));
    serve_bulk(
        &server,
        "1",
        None,
        "2026-10-01T00:00:00Z",
        &[
            ("Observation", fixture("Observation")),
            ("Patient", fixture("Patient")),
        ],
        &[],
    );
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("name = \"test\"", "name = \"test\"\nfetch = \"bulk\"");
    fs::write(&config, text).unwrap();
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("export Observation, Patient ($export)"))
        .stdout(has("IcrObs (Observation) → parquet/obs (3 rows)"))
        .stdout(has("IcrPatient (Patient) → parquet/patient (2 rows)"));
    // The same tables as from a search (views_are_downloaded_run_and_partitioned).
    let obs = dir.path().join("data/parquet/obs");
    assert_eq!(rows(&obs.join("country=SLE/part-0.parquet")).len(), 2);
    let none = rows(&obs.join("country=__HIVE_DEFAULT_PARTITION__/part-0.parquet"));
    assert_eq!(none.len(), 1);
    assert_eq!(get(&none[0], "id"), "o3");
    server.verify_and_clear();

    // Second run: changes since the first export's transactionTime. One
    // Observation changed and Patient p2 was deleted.
    let mut changed = fixture("Observation").remove(0);
    changed["valueInteger"] = json!(30);
    serve_bulk(
        &server,
        "2",
        Some("2026-10-01T00:00:00Z"),
        "2026-10-02T00:00:00Z",
        &[("Observation", vec![changed])],
        &["Patient/p2"],
    );
    for (t, n) in [("Observation", 3), ("Patient", 1)] {
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
        .stdout(has("Patient: 0 new, 0 updated or fetched again since 2026-10-01T00:00:00Z, 1 deleted, 1 in all"))
        .stdout(has("IcrObs (Observation) → parquet/obs (3 rows)"))
        .stdout(has("IcrPatient (Patient) → parquet/patient (1 rows)"));
    let sle = rows(&obs.join("country=SLE/part-0.parquet"));
    assert!(sle.iter().any(|r| get(r, "value") == "30"));
}

/// Whether the duckdb command is installed; checks that need it are skipped
/// without it.
fn have_duckdb() -> bool {
    std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// One value from a duckdb query, run in `dir`.
fn duckdb_value(dir: &Path, db: Option<&Path>, sql: &str) -> String {
    let mut c = std::process::Command::new("duckdb");
    if let Some(db) = db {
        c.arg(db);
    }
    let mut child = c
        .args(["-noheader", "-list"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), sql.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_catalog_has_a_view_per_table_and_the_extra_views() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
        "\n[catalog]\nextra = \"extra.sql\"\n",
    );
    fs::write(
        dir.path().join("extra.sql"),
        "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n",
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("catalog.sql: 3 table view(s)"));
    let data = dir.path().join("data");
    let sql = fs::read_to_string(data.join("catalog.sql")).unwrap();
    for table in ["locations", "obs", "patient"] {
        assert!(
            sql.contains(&format!(
                "CREATE OR REPLACE VIEW \"{table}\" AS\n  SELECT * FROM read_parquet('parquet/{table}/country=*/**/*.parquet'"
            )),
            "{sql}"
        );
    }
    assert!(sql.contains("CREATE OR REPLACE VIEW obs_sle"), "{sql}");
    if have_duckdb() {
        let n = duckdb_value(
            &data,
            None,
            ".read catalog.sql\nSELECT (SELECT count(*) FROM obs) || ',' || (SELECT count(*) FROM obs_sle);",
        );
        assert_eq!(n, "3,2");
    }
}

#[test]
fn without_a_catalog_section_there_is_no_catalog() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    cmd(&config).args(["--only", "views"]).assert().success();
    assert!(!dir.path().join("data/catalog.sql").exists());
    cmd(&config)
        .args(["--only", "catalog"])
        .assert()
        .failure()
        .stderr(has("--only catalog needs a [catalog] section"));
}

#[test]
fn a_local_catalog_duckdb_is_built_once_the_tables_are_in_place() {
    if !have_duckdb() {
        return;
    }
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
        "\n[catalog]\nduckdb_file = true\n",
    );
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("   catalog.duckdb"));
    let data = dir.path().join("data");
    assert!(!data.join("catalog.duckdb.tmp").exists());
    // Relative paths: read from the warehouse directory.
    assert_eq!(
        duckdb_value(
            &data,
            Some(Path::new("catalog.duckdb")),
            "SELECT count(*) FROM patient;"
        ),
        "2"
    );
}

/// A fake rclone that copies into $MIRROR as if it were the bucket, and logs
/// each call to $LOG.
#[cfg(unix)]
fn fake_rclone(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rclone");
    fs::write(
        &path,
        r#"#!/bin/sh
echo "$1 $(eval echo \${$#})" >> "$LOG"
if [ "$1" = lsd ]; then echo "          -1 2026-01-01 00:00:00        -1 bkt"; exit 0; fi
for a; do src=$dst; dst=$a; done
rest=${dst#*:}
rest=${rest#*/}
case "$1" in
  sync) mkdir -p "$MIRROR/$rest" && cp -R "$src/." "$MIRROR/$rest/" ;;
  copyto) mkdir -p "$(dirname "$MIRROR/$rest")" && cp "$src" "$MIRROR/$rest" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// With a base_url, catalog.duckdb is built during the push, after the tables
/// are uploaded, and its views read the uploaded copy.
#[test]
#[cfg(unix)]
fn the_pushed_catalog_reads_the_uploaded_tables() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let duckdb_file = have_duckdb();
    let (root, config) = setup(
        &server.url_str("/fhir"),
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"manifest.json\", \"catalog.sql\", \"catalog.duckdb\"]\n\n[catalog]\nbase_url = '{}'\nduckdb_file = {duckdb_file}\n",
            rclone.display(),
            mirror.display()
        ),
    );
    let log = dir.path().join("rclone.log");
    cmd(&config)
        .args(["--only", "views", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", &log)
        .assert()
        .success();
    let calls: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let at = |what: &str| {
        calls
            .iter()
            .position(|c| c.ends_with(what))
            .unwrap_or_else(|| panic!("{what} not pushed: {calls:#?}"))
    };
    assert!(at("r:bkt/parquet") < at("r:bkt/catalog.sql"), "{calls:#?}");
    let sql = fs::read_to_string(mirror.join("catalog.sql")).unwrap();
    assert!(
        sql.contains(&format!(
            "read_parquet('{}/parquet/obs/country=*/**/*.parquet'",
            mirror.display()
        )),
        "{sql}"
    );
    if duckdb_file {
        assert!(
            at("r:bkt/catalog.sql") < at("r:bkt/catalog.duckdb"),
            "{calls:#?}"
        );
        // The local tables are gone: the views read the mirror.
        fs::remove_dir_all(root.path().join("data/parquet")).unwrap();
        assert_eq!(
            duckdb_value(
                dir.path(),
                Some(&mirror.join("catalog.duckdb")),
                "SELECT count(*) FROM obs;"
            ),
            "3"
        );
    }
}

/// A relative --data, and a table named after a SQL keyword.
#[test]
fn the_catalog_takes_a_relative_data_dir_and_keyword_table_names() {
    if !have_duckdb() {
        return;
    }
    let (dir, config) = setup(NOWHERE, "\n[catalog]\nduckdb_file = true\n");
    let data = dir.path().join("data");
    let group = data.join("parquet/group/country=SLE");
    fs::create_dir_all(&group).unwrap();
    fs::copy(
        data.join("parquet/locations/country=SLE/part-0.parquet"),
        group.join("part-0.parquet"),
    )
    .unwrap();
    // cmd runs in the config's directory, where data/ is.
    cmd(&config)
        .args(["--data", "data", "--only", "catalog"])
        .assert()
        .success()
        .stdout(has("   catalog.duckdb"));
    assert_eq!(
        duckdb_value(
            &data,
            Some(Path::new("catalog.duckdb")),
            "SELECT (SELECT count(*) FROM \"group\") || ',' || (SELECT count(*) FROM locations);"
        ),
        "2,2"
    );
}

/// Without duckdb_file, a catalog.duckdb listed in push.files (built by a
/// hook, say) is uploaded as any other listed file.
#[test]
#[cfg(unix)]
fn a_listed_catalog_duckdb_is_pushed_when_fhir_pipes_does_not_build_it() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let (root, config) = setup(
        NOWHERE,
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"catalog.duckdb\"]\n\n[catalog]\n",
            rclone.display()
        ),
    );
    fs::write(root.path().join("data/catalog.duckdb"), "built by a hook").unwrap();
    cmd(&config)
        .args(["--only", "catalog", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", dir.path().join("rclone.log"))
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(mirror.join("catalog.duckdb")).unwrap(),
        "built by a hook"
    );
    assert!(mirror.join("catalog.sql").is_file());
}

/// A warehouse with the fixture downloads in raw/, so `--reuse-raw` runs the
/// views without a server.
fn setup_with_raw(extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = setup(NOWHERE, extra_config);
    let raw = dir.path().join("data/raw/test");
    fs::create_dir_all(&raw).unwrap();
    for t in ["Observation", "Patient"] {
        fs::copy(
            fixtures().join(format!("raw/{t}.ndjson")),
            raw.join(format!("{t}.ndjson")),
        )
        .unwrap();
    }
    (dir, config)
}

/// The fixture views plus Patient views named `names` (each with an `id`
/// column), and, if `empty`, a Patient view that selects nothing.
fn views_with(dir: &Path, names: &[&str], empty: bool) -> PathBuf {
    let views = dir.join("more-views");
    fs::create_dir_all(&views).unwrap();
    for f in fs::read_dir(fixtures().join("views")).unwrap() {
        let f = f.unwrap().path();
        fs::copy(&f, views.join(f.file_name().unwrap())).unwrap();
    }
    let view = |name: &str, filter: &str| {
        format!(
            r#"{{"resourceType": "ViewDefinition", "name": "{name}", "status": "active", "resource": "Patient",{filter}
                "select": [{{"column": [{{"name": "id", "path": "getResourceKey()", "type": "id"}},
                                         {{"name": "born", "path": "birthDate", "type": "date"}}]}}]}}"#
        )
    };
    for n in names {
        fs::write(views.join(format!("{n}.json")), view(n, "")).unwrap();
    }
    if empty {
        fs::write(
            views.join("IcrEmpty.json"),
            view("IcrEmpty", r#" "where": [{"path": "false"}],"#),
        )
        .unwrap();
    }
    views
}

/// A view that selects nothing still writes its columns, as one empty file.
#[test]
fn an_empty_view_writes_an_empty_file() {
    let (dir, config) = setup_with_raw("\n[catalog]\n");
    let views = views_with(dir.path(), &[], true);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&views)
        .assert()
        .success()
        .stdout(has("IcrEmpty (Patient) → parquet/empty (0 rows)"));
    let file = dir
        .path()
        .join("data/parquet/empty/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet");
    let reader = SerializedFileReader::new(fs::File::open(&file).unwrap()).unwrap();
    assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
    let fields: Vec<String> = reader
        .metadata()
        .file_metadata()
        .schema()
        .get_fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(fields, ["id", "born", "country"]);
    // The catalog has a view for it.
    let sql = fs::read_to_string(dir.path().join("data/catalog.sql")).unwrap();
    assert!(sql.contains("CREATE OR REPLACE VIEW \"empty\""), "{sql}");
}

/// With spark = true, catalog.spark.sql has a table for every table, typed
/// from its files, empty ones and keyword names included.
#[test]
fn the_spark_catalog_has_a_table_per_table() {
    let (dir, config) =
        setup_with_raw("\n[catalog]\nspark = true\nspark_extra = \"spark-extra.sql\"\n");
    fs::write(
        dir.path().join("spark-extra.sql"),
        "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n",
    )
    .unwrap();
    let views = views_with(dir.path(), &["IcrGroup"], true);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&views)
        .assert()
        .success()
        .stdout(has("catalog.spark.sql: 5 table(s) in database fhir"));
    let data = dir.path().join("data");
    let sql = fs::read_to_string(data.join("catalog.spark.sql")).unwrap();
    assert!(
        sql.contains("CREATE DATABASE IF NOT EXISTS `fhir`;"),
        "{sql}"
    );
    for table in ["empty", "group", "locations", "obs", "patient"] {
        assert!(
            sql.contains(&format!(
                "DROP TABLE IF EXISTS `fhir`.`{table}`;\nCREATE TABLE `fhir`.`{table}` ("
            )),
            "{sql}"
        );
    }
    // The empty view's one empty file still gives its columns.
    assert!(
        sql.contains(
            "CREATE TABLE `fhir`.`empty` (\n  `id` STRING,\n  `born` DATE,\n  `country` STRING\n)"
        ),
        "{sql}"
    );
    assert!(sql.contains("  `issued` TIMESTAMP,"), "{sql}");
    // The default location is the warehouse directory (the config's
    // directory resolved, as `data` is relative to it).
    #[cfg(unix)]
    assert!(
        sql.contains(&format!(
            "LOCATION 'file://{}/parquet/obs';",
            data.canonicalize().unwrap().display()
        )),
        "{sql}"
    );
    assert!(
        sql.ends_with(
            "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n"
        ),
        "{sql}"
    );
}

/// catalog.spark.sql is pushed with the other catalog files, after the
/// tables, once even when push.files lists it.
#[test]
#[cfg(unix)]
fn the_spark_catalog_is_pushed_after_the_tables() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let (_root, config) = setup(
        NOWHERE,
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"manifest.json\", \"catalog.spark.sql\"]\n\n[catalog]\nspark = true\nspark_location = \"s3a://bkt\"\nspark_database = \"analytics\"\n",
            rclone.display()
        ),
    );
    let log = dir.path().join("rclone.log");
    cmd(&config)
        .args(["--only", "catalog", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", &log)
        .assert()
        .success()
        .stdout(has("catalog.spark.sql: 1 table(s) in database analytics"));
    let calls: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let pushed: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ends_with("r:bkt/catalog.spark.sql"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(pushed.len(), 1, "{calls:#?}");
    let at = |what: &str| calls.iter().position(|c| c.ends_with(what)).unwrap();
    assert!(at("r:bkt/parquet") < pushed[0], "{calls:#?}");
    assert!(at("r:bkt/catalog.sql") < pushed[0], "{calls:#?}");
    let sql = fs::read_to_string(mirror.join("catalog.spark.sql")).unwrap();
    assert!(
        sql.contains("LOCATION 's3a://bkt/parquet/locations';"),
        "{sql}"
    );
}

#[test]
#[cfg(not(feature = "postgres"))]
fn the_database_step_needs_the_postgres_feature() {
    let (_dir, config) = setup_with_raw("\n[database]\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stderr(has("--features postgres"));
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--skip", "database"])
        .assert()
        .success();
}

/// Runs against the PostgreSQL in $FHIR_PIPES_TEST_DATABASE_URL, each test in
/// a schema of its own; skipped without it.
#[cfg(feature = "postgres")]
mod database {
    use super::*;
    use sqlx_core::connection::Connection;
    use sqlx_core::row::Row;
    use sqlx_core::sql_str::AssertSqlSafe;

    const URL: &str = "FHIR_PIPES_TEST_DATABASE_URL";

    fn url() -> Option<String> {
        std::env::var(URL).ok().filter(|u| !u.is_empty())
    }

    /// A schema name no other test uses.
    fn schema(test: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("t_{test}_{}_{nanos}", std::process::id())
    }

    /// Run SQL statements; the rows of the last one, every value as text.
    fn sql(statements: &[&str]) -> Vec<Vec<Option<String>>> {
        let url = url().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut conn = sqlx_postgres::PgConnection::connect(&url).await.unwrap();
                let mut rows = Vec::new();
                for s in statements {
                    rows = sqlx_core::query::query(AssertSqlSafe(s.to_string()))
                        .fetch_all(&mut conn)
                        .await
                        .unwrap_or_else(|e| panic!("{s}: {e}"));
                }
                rows.iter()
                    .map(|r| {
                        (0..r.len())
                            .map(|i| r.try_get::<Option<String>, _>(i).unwrap())
                            .collect()
                    })
                    .collect()
            })
    }

    fn one(statement: &str) -> String {
        sql(&[statement])[0][0].clone().unwrap_or_default()
    }

    #[test]
    fn view_tables_are_loaded_and_replaced() {
        if url().is_none() {
            return;
        }
        let s = schema("load");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("obs → {s}.obs (3 rows)")))
            .stdout(has(format!("patient → {s}.patient (2 rows)")));
        // Only view tables: not locations.
        assert_eq!(
            one(&format!(
                "SELECT string_agg(table_name::text, ',' ORDER BY table_name) FROM information_schema.tables WHERE table_schema = '{s}'"
            )),
            "obs,patient"
        );
        // Types from the Parquet files, values as written.
        assert_eq!(
            one(&format!(
                "SELECT data_type::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'patient' AND column_name = 'born'"
            )),
            "date"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || '=' || born::text || '@' || coalesce(country, 'null'), ',' ORDER BY id) FROM \"{s}\".patient"
            )),
            "p1=2020-05-01@null,p2=2021-01-01@null"
        );

        // A view someone built on a table survives the next run, which
        // replaces the rows.
        sql(&[&format!(
            "CREATE VIEW \"{s}\".patient_ids AS SELECT id FROM \"{s}\".patient"
        )]);
        let raw = dir.path().join("data/raw/test/Patient.ndjson");
        let first = fs::read_to_string(&raw)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        fs::write(&raw, format!("{first}\n")).unwrap();
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("patient → {s}.patient (1 rows)")));
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',') FROM \"{s}\".patient_ids"
            )),
            "p1"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn a_table_with_other_columns_is_recreated() {
        if url().is_none() {
            return;
        }
        let s = schema("recreate");
        sql(&[
            &format!("CREATE SCHEMA \"{s}\""),
            &format!("CREATE TABLE \"{s}\".obs (stale text)"),
        ]);
        let (_dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\ntables = [\"obs\"]\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'obs' AND column_name = 'stale'"
            )),
            "0"
        );
        // `tables` limited the load to obs.
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.tables WHERE table_schema = '{s}' AND table_name = 'patient'"
            )),
            "0"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// A missing or unreadable warehouse fails before the database changes.
    #[test]
    fn a_missing_warehouse_leaves_the_database_alone() {
        if url().is_none() {
            return;
        }
        let s = schema("missing");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        cmd(&config)
            .args(["--only", "database", "--data"])
            .arg(dir.path().join("nowhere"))
            .assert()
            .failure()
            .stderr(has("the database was not changed"));
        assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// An empty view gets its table; tables named t, t_id and t_id_idx do
    /// not clash with the index names PostgreSQL picks.
    #[test]
    fn empty_and_similarly_named_tables_load() {
        if url().is_none() {
            return;
        }
        let s = schema("names");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        let views = views_with(
            dir.path(),
            &["IcrAlpha", "IcrAlphaId", "IcrAlphaIdIdx"],
            true,
        );
        for _ in 0..2 {
            cmd(&config)
                .args(["--only", "views", "--reuse-raw", "--views"])
                .arg(&views)
                .assert()
                .success()
                .stdout(has(format!("empty → {s}.empty (0 rows)")))
                .stdout(has(format!("alpha_id → {s}.alpha_id (2 rows)")));
        }
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".empty")),
            "0"
        );
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".alpha")),
            "2"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// Two loads at once: the second waits for the first, so the rows are
    /// there once.
    #[test]
    fn overlapping_loads_do_not_duplicate_rows() {
        if url().is_none() {
            return;
        }
        let s = schema("overlap");
        let (_dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        let run = || {
            let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_fhir-pipes"));
            c.current_dir(config.parent().unwrap())
                .args(["refresh", "--config"])
                .arg(&config)
                .args(["--only", "database"])
                .env_remove("FHIR_PIPES_DATA")
                .env_remove("FHIR_PIPES_VIEWS")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        for _ in 0..3 {
            let mut a = run();
            let mut b = run();
            assert!(a.wait().unwrap().success() && b.wait().unwrap().success());
            assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        }
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn database_settings_are_checked() {
        let (_dir, config) = setup_with_raw(
            "\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\ntables = [\"nope\"]\n",
        );
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .failure()
            .stderr(has("`nope` is not a view table (obs, patient)"));
        let (_dir, config) = setup_with_raw("\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\n");
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .env_remove("FHIR_PIPES_NO_SUCH_URL")
            .assert()
            .failure()
            .stderr(has("$FHIR_PIPES_NO_SUCH_URL"));
    }
}

/// The username from the config, the password only from the environment.
#[test]
fn locations_are_extracted_with_basic_auth() {
    let server = Server::run();
    let basic = || request::headers(contains(("authorization", "Basic YWRtaW46QWRtaW4xMjM=")));
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Location"),
            basic()
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![]))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            basic()
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![]))),
    );
    let (_dir, config) = setup(&server.url_str("/fhir"), "");
    let text = fs::read_to_string(&config).unwrap().replace(
        "name = \"test\"\n",
        "name = \"test\"\nusername = \"admin\"\n",
    );
    fs::write(&config, text).unwrap();
    cmd(&config)
        .args(["--only", "locations", "--full", "--check"])
        .env("FHIR_PIPES_PASSWORD", "Admin123")
        .assert()
        .success()
        .stdout(has("auth:       basic"));
    cmd(&config)
        .args(["--only", "locations", "--full"])
        .env("FHIR_PIPES_PASSWORD", "Admin123")
        .assert()
        .success();
    cmd(&config)
        .args(["--only", "locations", "--full"])
        .assert()
        .failure()
        .stderr(has("FHIR_PIPES_PASSWORD"));
}

/// Every file of the warehouse but raw/, by path relative to it, with its
/// contents.
fn warehouse(data: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, data: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(data).unwrap();
            if rel.starts_with("raw") {
                continue;
            }
            if p.is_dir() {
                walk(&p, data, out);
            } else {
                let name = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(name, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(data, data, &mut out);
    out
}

/// The paths of `warehouse`, for assertion messages.
fn paths(files: &std::collections::BTreeMap<String, Vec<u8>>) -> Vec<&str> {
    files.keys().map(String::as_str).collect()
}

fn assert_nothing_staged(data: &Path) {
    assert!(!data.join(".staging").exists());
    assert!(!data.join("parquet/.staging").exists());
}

/// Drop the last Observation, so the next run would write a different obs table.
fn drop_an_observation(root: &Path) {
    let raw = root.join("data/raw/test/Observation.ndjson");
    let text = fs::read_to_string(&raw).unwrap();
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    fs::write(&raw, kept[..kept.len() - 1].join("\n") + "\n").unwrap();
}

/// A view whose table fails to write (here its lookup table cannot be read)
/// leaves the tables written before it, the manifest and the catalog as the
/// previous run left them.
#[test]
fn a_failing_view_leaves_every_table_as_it_was() {
    let (dir, config) =
        setup_with_raw("\n[[partition.lookup]]\nkey = \"id\"\ntable = \"bad\"\n\n[catalog]\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let before = warehouse(&data);
    assert!(before.contains_key("parquet/obs/country=SLE/part-0.parquet"));

    // obs is written first and would change; patient then looks its partition
    // up in a table that cannot be read.
    drop_an_observation(dir.path());
    fs::create_dir_all(data.join("parquet/bad/country=SLE")).unwrap();
    fs::write(
        data.join("parquet/bad/country=SLE/part-0.parquet"),
        "not parquet",
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stdout(has("parquet/obs (2 rows)"))
        .stderr(has("reading lookup table bad"));
    let mut after = warehouse(&data);
    after.remove("parquet/bad/country=SLE/part-0.parquet");
    assert_eq!(paths(&after), paths(&before));
    assert!(after == before, "the warehouse changed");
    assert_nothing_staged(&data);
}

/// A catalog step that fails (here `extra` names a missing file) leaves the
/// tables, the manifest and catalog.sql as the previous run left them.
#[test]
fn a_failing_catalog_leaves_the_tables_and_manifest_as_they_were() {
    let (dir, config) = setup_with_raw("\n[catalog]\nextra = \"extra.sql\"\n");
    let extra = dir.path().join("extra.sql");
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let before = warehouse(&data);
    assert!(before.contains_key("manifest.json"));
    assert!(before.contains_key("catalog.sql"));

    drop_an_observation(dir.path());
    fs::remove_file(&extra).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stdout(has("parquet/obs (2 rows)"))
        .stderr(has("extra.sql"));
    let after = warehouse(&data);
    assert_eq!(paths(&after), paths(&before));
    assert!(after == before, "the warehouse changed");
    assert_nothing_staged(&data);

    // Once the catalog can be written, the run publishes the new tables.
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 2);
}

/// Hooks run after the swap, on the published warehouse: a hook that fails
/// stops the run before the push, with the new tables, manifest and catalog
/// in place.
#[test]
#[cfg(unix)]
fn a_failing_hook_sees_the_published_warehouse_and_stops_the_push() {
    let tools = tempfile::tempdir().unwrap();
    let rclone = fake_rclone(tools.path());
    let log = tools.path().join("rclone.log");
    let (dir, config) = setup_with_raw(&format!(
        r#"
[tools]
rclone = '{}'

[push]
remote = "r"
bucket = "bkt"

[catalog]

[[hooks]]
name = "fails"
run = ["sh", "-c", "cp \"$DATA/manifest.json\" seen.json; exit 3"]
"#,
        rclone.display()
    ));
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw", "--push"])
        .env("MIRROR", tools.path().join("mirror"))
        .env("LOG", &log)
        .assert()
        .failure()
        .stderr(has("hook fails"));
    let data = dir.path().join("data");
    let seen = fs::read_to_string(dir.path().join("seen.json")).unwrap();
    assert_eq!(
        seen,
        fs::read_to_string(data.join("manifest.json")).unwrap()
    );
    let manifest: Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert!(data.join("catalog.sql").is_file());
    assert!(!log.exists(), "pushed after a failing hook");
    assert_nothing_staged(&data);
}

/// A successful run leaves the files it always did, and nothing staged.
#[test]
fn a_successful_run_publishes_every_table_and_leaves_nothing_staged() {
    let server = Server::run();
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot");
    let locations: Vec<Value> = read_ndjson(&snapshot.join("locations.ndjson"))
        .into_iter()
        .map(|mut l| {
            l["meta"] = json!({"lastUpdated": "2026-01-01T00:00:00Z"});
            l
        })
        .collect();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Location"))
            .times(1)
            .respond_with(status_code(200).body(bundle(locations))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Organization"))
            .times(1)
            .respond_with(status_code(200).body(bundle(vec![]))),
    );
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(&server.url_str("/fhir"), "\n[catalog]\n");
    cmd(&config)
        .args(["--only", "locations,views", "--full"])
        .assert()
        .success()
        .stdout(has("== publish"));
    let data = dir.path().join("data");
    let mut files = warehouse(&data);
    // The one new file: the lock a run holds on the warehouse.
    assert!(files.remove(".refresh.lock").is_some());
    let tops: std::collections::BTreeSet<&str> = files
        .keys()
        .map(|k| {
            let mut it = k.split('/');
            match it.next().unwrap() {
                "parquet" => k.splitn(3, '/').take(2).last().unwrap(),
                top => top,
            }
        })
        .collect();
    assert_eq!(
        tops.into_iter().collect::<Vec<_>>(),
        [
            "_report.json",
            "catalog.sql",
            "locations",
            "manifest.json",
            "obs",
            "patient",
            "views"
        ],
        "{:#?}",
        paths(&files)
    );
    assert!(
        files
            .keys()
            .all(|k| !k.split('/').any(|c| c.starts_with('.'))),
        "{:#?}",
        paths(&files)
    );
    // The fixture's country=SLE table was replaced by the extracted registry.
    assert!(!data.join("parquet/locations/country=SLE").exists());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    let tables: Vec<&String> = manifest["tables"].as_object().unwrap().keys().collect();
    assert_eq!(tables, ["locations", "obs", "patient"]);
    assert!(manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 2);
    let sql = fs::read_to_string(data.join("catalog.sql")).unwrap();
    assert!(
        sql.contains("read_parquet('parquet/locations/country=*/**/*.parquet'"),
        "{sql}"
    );
    assert!(!sql.contains(".staging"), "{sql}");
}

/// What a killed run left behind is cleaned up at the start of the next: its
/// staging directories, and a table a swap left only as `.<table>.old`.
#[test]
fn a_killed_run_is_cleaned_up_by_the_next() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let junk = data.join("parquet/.staging/obs/country=XXX");
    fs::create_dir_all(&junk).unwrap();
    fs::write(junk.join("part-0.parquet"), "half written").unwrap();
    fs::create_dir_all(data.join(".staging")).unwrap();
    fs::write(data.join(".staging/manifest.json"), "{}").unwrap();
    fs::rename(
        data.join("parquet/locations"),
        data.join("parquet/.locations.old"),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has("restored parquet/locations"));
    assert_nothing_staged(&data);
    assert!(!data.join("parquet/.locations.old").exists());
    // The restored registry was there for the views' partition lookups.
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert!(!data.join("parquet/obs/country=XXX").exists());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["locations"]["rows"], 2);
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
}

/// `server.google_auth`: tokens from a service account key, shown by
/// `--check` without any secret.
#[test]
fn locations_are_extracted_with_google_auth() {
    let server = Server::run();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/token"),
            request::body(url_decoded(contains((
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer"
            )))),
        ])
        .times(1)
        .respond_with(json_encoded(
            json!({"access_token": "ya29.r", "expires_in": 3599}),
        )),
    );
    let bearer = || request::headers(contains(("authorization", "Bearer ya29.r")));
    for t in ["Location", "Organization"] {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", format!("/fhir/{t}")),
                bearer()
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(vec![]))),
        );
    }
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("name = \"test\"\n", "name = \"test\"\ngoogle_auth = true\n");
    fs::write(&config, text).unwrap();
    let key = dir.path().join("key.json");
    fs::write(
        &key,
        json!({
            "type": "service_account",
            "private_key": fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/google/test-only-service-account-key.pem")
            )
            .unwrap(),
            "client_email": "pipes-test@test-project.iam.gserviceaccount.com",
            "token_uri": server.url_str("/token"),
        })
        .to_string(),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "locations", "--full", "--check"])
        .env("GOOGLE_APPLICATION_CREDENTIALS", &key)
        .assert()
        .success()
        .stdout(has(
            "auth:       Google application default credentials: service account pipes-test@test-project.iam.gserviceaccount.com",
        ))
        .stdout(has("PRIVATE KEY").not());
    cmd(&config)
        .args(["--only", "locations", "--full"])
        .env("GOOGLE_APPLICATION_CREDENTIALS", &key)
        .assert()
        .success();
    cmd(&config)
        .args(["--only", "locations", "--full"])
        .env("GOOGLE_APPLICATION_CREDENTIALS", &key)
        .env("FHIR_PIPES_TOKEN", "t")
        .assert()
        .failure()
        .stderr(has("use one way to authenticate"));
}

/// The environment wins over the config file, false as well as true.
#[test]
fn google_auth_false_in_the_environment_overrides_the_config() {
    let (_dir, config) = setup(NOWHERE, "");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("name = \"test\"\n", "name = \"test\"\ngoogle_auth = true\n");
    fs::write(&config, text).unwrap();
    cmd(&config)
        .args(["--only", "locations", "--check"])
        .env("FHIR_PIPES_GOOGLE_AUTH", "0")
        .assert()
        .success()
        .stdout(has("auth:       none"));
    cmd(&config)
        .args(["--only", "locations", "--check"])
        .env("FHIR_PIPES_GOOGLE_AUTH", "0")
        .env("FHIR_PIPES_TOKEN", "t")
        .assert()
        .success()
        .stdout(has("auth:       bearer token"));
    cmd(&config)
        .args(["--only", "locations", "--check", "--google-auth=false"])
        .env("FHIR_PIPES_TOKEN", "t")
        .assert()
        .success()
        .stdout(has("auth:       bearer token"));
    // Without the override, the file's setting still excludes a token.
    cmd(&config)
        .args(["--only", "locations", "--check"])
        .env("FHIR_PIPES_TOKEN", "t")
        .assert()
        .failure()
        .stderr(has("use one way to authenticate"));
}

#[test]
fn a_store_path_is_expanded_in_check() {
    let (_dir, config) = setup("projects/p/locations/l/datasets/d/fhirStores/s", "");
    cmd(&config)
        .args(["--only", "locations", "--check"])
        .assert()
        .success()
        .stdout(has(
            "server:     https://healthcare.googleapis.com/v1/projects/p/locations/l/datasets/d/fhirStores/s/fhir",
        ));
}

/// Write `files` (relative path → text) under `dir/input`; returns that
/// directory.
fn inputs(dir: &Path, files: &[(&str, String)]) -> PathBuf {
    let input = dir.join("input");
    for (name, text) in files {
        let path = input.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    input
}

fn lines(resources: &[Value]) -> String {
    resources.iter().map(|r| format!("{r}\n")).collect()
}

fn obs(id: &str, value: i64, updated: Option<&str>) -> Value {
    let mut o = json!({"resourceType": "Observation", "id": id, "status": "final",
        "code": {"text": "x"}, "subject": {"reference": "Location/loc-bo"}, "valueInteger": value});
    if let Some(at) = updated {
        o["meta"] = json!({"lastUpdated": at});
    }
    o
}

/// The rows of every part of a table, with their partition directory.
fn table_rows(table: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    let mut parts: Vec<PathBuf> = fs::read_dir(table)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    parts.sort();
    for part in parts {
        let name = part.file_name().unwrap().to_string_lossy().into_owned();
        for row in rows(&part.join("part-0.parquet")) {
            out.push((name.clone(), row));
        }
    }
    out
}

#[test]
fn refresh_reads_ndjson_and_bundles_from_files_without_a_server() {
    let (dir, config) = setup(NOWHERE, "");
    let root = dir.path();
    // A transaction Bundle: the Observation names its subject by the
    // Location entry's urn:uuid, resolved to Location/loc-bo.
    let transaction = json!({"resourceType": "Bundle", "type": "transaction", "entry": [
        {"fullUrl": "urn:uuid:7f3a", "resource": {"resourceType": "Location", "id": "loc-bo", "name": "Bo"}},
        {"fullUrl": "urn:uuid:91c2", "resource": {"resourceType": "Observation", "id": "o4",
            "status": "final", "code": {"text": "x"}, "subject": {"reference": "urn:uuid:7f3a"},
            "valueInteger": 4}},
        // o2 again, updated later than in a.ndjson: this one is kept.
        {"fullUrl": "urn:uuid:5d0e", "resource": obs("o2", 50, Some("2026-02-01T00:00:00Z"))},
    ]});
    let input = inputs(
        root,
        &[
            (
                "a.ndjson",
                lines(&[
                    obs("o1", 30, Some("2026-03-01T00:00:00Z")),
                    obs("o2", 5, Some("2026-01-01T00:00:00Z")),
                    json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-05-01"}),
                ]),
            ),
            (
                "b.ndjson",
                lines(&[
                    // o1 again, older: the copy in a.ndjson is kept.
                    obs("o1", 3, Some("2026-01-01T00:00:00Z")),
                    // p1 again, neither with lastUpdated: the later file wins.
                    json!({"resourceType": "Patient", "id": "p1", "birthDate": "2019-07-01"}),
                ]),
            ),
            ("sub/transaction.json", transaction.to_string()),
            (".hidden.ndjson", "not json\n".into()),
        ],
    );
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .assert()
        .success()
        .stdout(has("== files (3 files"))
        .stdout(has("Observation: 3 resources (2 duplicates by id merged)"))
        .stdout(has("IcrObs (Observation) → parquet/obs (3 rows)"))
        .stdout(has("parquet/patient (1 rows)"));

    let raw = read_ndjson(&root.join("data/raw/test/Observation.ndjson"));
    let ids: Vec<&str> = raw.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["o1", "o2", "o4"]);
    assert_eq!(
        read_ndjson(&root.join("data/raw/test/Location.ndjson")).len(),
        1
    );

    let got: Vec<(String, String, String, String)> = table_rows(&root.join("data/parquet/obs"))
        .into_iter()
        .map(|(part, r)| {
            let col = |c| get(&r, c).to_string();
            (part, col("id"), col("value"), col("location_id"))
        })
        .collect();
    let row = |p: &str, id: &str, v: &str| (p.into(), id.into(), v.into(), "loc-bo".into());
    // Every row found its country through the locations table, o4 through
    // the resolved urn:uuid.
    assert_eq!(
        got,
        [
            row("country=SLE", "o1", "30"),
            row("country=SLE", "o2", "50"),
            row("country=SLE", "o4", "4"),
        ]
    );
    let patients = table_rows(&root.join("data/parquet/patient"));
    assert_eq!(get(&patients[0].1, "born"), "2019-07-01");

    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["server"], Value::Null);
    assert_eq!(manifest["files"], json!([input.display().to_string()]));
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
}

#[test]
fn locations_in_the_files_build_the_locations_table() {
    let (dir, config) = setup(NOWHERE, "");
    let root = dir.path();
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot");
    let n = read_ndjson(&snapshot.join("locations.ndjson")).len();
    cmd(&config)
        .args(["--only", "locations,views", "--input"])
        .arg(snapshot.join("locations.ndjson"))
        .assert()
        .success()
        .stdout(has(format!("snapshot: {n} Locations")));
    let snap = root.join("data/raw/test/locations");
    assert_eq!(read_ndjson(&snap.join("locations.ndjson")).len(), n);
    assert!(!snap.join("organizations.ndjson").exists());
    let state: Value =
        serde_json::from_str(&fs::read_to_string(snap.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["server"], "files");
    assert_eq!(state["count"], n);

    // parquet/locations is the files' registry now, not the fixture's.
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["extract"]["count"], n);
    let rows = manifest["tables"]["locations"]["rows"].as_i64().unwrap();
    assert!(rows > 2, "{manifest}");
    assert!(root.join("data/parquet/locations/country=NG").is_dir());
    assert!(!root.join("data/parquet/locations/country=SLE").exists());
}

#[test]
fn types_the_files_lack_give_empty_tables() {
    let (dir, config) = setup(NOWHERE, "\n[source]\nfiles = [\"input/*.ndjson\"]\n");
    let root = dir.path();
    // No locations table at all: the observations go to the null partition.
    fs::remove_dir_all(root.join("data/parquet/locations")).unwrap();
    inputs(
        root,
        &[
            ("obs.ndjson", lines(&[obs("o1", 3, None)])),
            (
                "patients.ndjson",
                lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
            ),
        ],
    );
    cmd(&config)
        .args(["--only", "locations,views"])
        .assert()
        .success()
        .stdout(has("the files have no Location resources; skipped"))
        .stdout(has("parquet/obs (1 rows)"));
    let obs_rows = table_rows(&root.join("data/parquet/obs"));
    assert_eq!(obs_rows[0].0, "country=__HIVE_DEFAULT_PARTITION__");
    assert!(!root.join("data/parquet/locations").exists());

    // Without observations in the files the second run's obs table is
    // empty, and the first run's Observation.ndjson is gone.
    fs::remove_file(root.join("input/obs.ndjson")).unwrap();
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("removed raw/Observation.ndjson"))
        .stdout(has(
            "Observation: none in the files; its views write empty tables",
        ))
        .stdout(has("parquet/obs (0 rows)"));
    assert!(!root.join("data/raw/test/Observation.ndjson").exists());
    let empty = root.join("data/parquet/obs/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet");
    assert!(rows(&empty).is_empty());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 0);
}

#[test]
fn check_shows_the_file_source() {
    let (dir, config) = setup(
        NOWHERE,
        "\n[source]\nfiles = [\"input\", \"exports/*.ndjson\"]\n",
    );
    inputs(
        dir.path(),
        &[("a.ndjson", "{}\n".into()), ("sub/b.json", "{}".into())],
    );
    cmd(&config)
        .args(["--only", "views", "--check"])
        .assert()
        .success()
        .stdout(has("source:     files, no server (raw/test)"))
        .stdout(has("input:      ").and(has("input: 2 files")))
        .stdout(has("exports/*.ndjson: input pattern").and(has("matches nothing")))
        .stdout(has("server:").not())
        .stdout(has("auth:").not());
}

#[test]
fn a_view_column_named_like_the_partition_column_stops_the_run() {
    let (dir, config) = setup(NOWHERE, "");
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("v.json"),
        json!({"resourceType": "ViewDefinition", "name": "PatientCountry", "resource": "Patient",
            "status": "active", "select": [{"column": [
                {"name": "id", "path": "getResourceKey()"},
                {"name": "country", "path": "address.country.first()"}]}]})
        .to_string(),
    )
    .unwrap();
    let input = inputs(
        dir.path(),
        &[(
            "p.ndjson",
            lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
        )],
    );
    cmd(&config)
        .args(["--only", "views", "--views"])
        .arg(&views)
        .arg("--input")
        .arg(&input)
        .assert()
        .failure()
        .stderr(has(
            "view PatientCountry has a column `country`, which is the partition column",
        ));
    assert!(!dir.path().join("data/parquet/patient_country").exists());
}

fn snapshot_locations() -> Vec<Value> {
    read_ndjson(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson"),
    )
}

#[test]
fn a_file_run_makes_the_next_server_refresh_download_in_full() {
    let mut server = Server::run();
    for t in ["Observation", "Patient"] {
        server.expect(
            Expectation::matching(full_search(t))
                .times(1)
                .respond_with(status_code(200).body(bundle(stamped(t, "2026-10-01T00:00:00Z")))),
        );
    }
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    let root = dir.path();
    cmd(&config).args(["--only", "views"]).assert().success();
    server.verify_and_clear();
    assert!(root.join("data/raw/test/.export-state.json").exists());

    // Older copies from files, into the same raw/test: the server's export
    // state no longer describes these files.
    let mut older = stamped("Patient", "2026-09-01T00:00:00Z");
    older[0]["birthDate"] = json!("1999-01-01");
    let input = inputs(
        root,
        &[
            ("p.ndjson", lines(&older)),
            (
                "o.ndjson",
                lines(&stamped("Observation", "2026-09-01T00:00:00Z")),
            ),
        ],
    );
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .assert()
        .success()
        .stdout(has("removed the server export's state"));
    assert!(!root.join("data/raw/test/.export-state.json").exists());

    // Back to the server: every type is downloaded in full, so no resource
    // keeps the files' older copy.
    for t in ["Observation", "Patient"] {
        server.expect(
            Expectation::matching(full_search(t))
                .times(1)
                .respond_with(status_code(200).body(bundle(stamped(t, "2026-10-01T00:00:00Z")))),
        );
    }
    cmd(&config).args(["--only", "views"]).assert().success();
    let patients = read_ndjson(&root.join("data/raw/test/Patient.ndjson"));
    assert_eq!(patients, stamped("Patient", "2026-10-01T00:00:00Z"));
}

#[test]
fn a_snapshot_built_from_files_is_extracted_in_full_from_a_server() {
    let server = Server::run();
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    let input = inputs(dir.path(), &[("l.ndjson", lines(&snapshot_locations()))]);
    cmd(&config)
        .args(["--only", "locations", "--input"])
        .arg(&input)
        .assert()
        .success();

    let locations: Vec<Value> = snapshot_locations()
        .into_iter()
        .take(3)
        .map(|mut l| {
            l["meta"] = json!({"lastUpdated": "2026-10-01T00:00:00Z"});
            l
        })
        .collect();
    for (t, body) in [("Location", locations), ("Organization", vec![])] {
        server.expect(
            Expectation::matching(full_search(t))
                .times(1)
                .respond_with(status_code(200).body(bundle(body))),
        );
    }
    cmd(&config)
        .args(["--only", "locations"])
        .assert()
        .success()
        .stdout(has("the snapshot was built from files"));
    let snap = dir.path().join("data/raw/test/locations");
    // The full extract replaced the files' 12 Locations.
    assert_eq!(read_ndjson(&snap.join("locations.ndjson")).len(), 3);
    let state: Value =
        serde_json::from_str(&fs::read_to_string(snap.join("state.json")).unwrap()).unwrap();
    assert_eq!(
        state["server"],
        server.url_str("/fhir").trim_end_matches('/')
    );
}

#[test]
fn locations_are_found_under_another_partition_column() {
    let (dir, config) = setup(NOWHERE, "\n[partition]\ncolumn = \"part\"\n\n[catalog]\n");
    let root = dir.path();
    let mut o = obs("o1", 3, None);
    o["subject"] = json!({"reference": "Location/kano"});
    let input = inputs(
        root,
        &[
            ("l.ndjson", lines(&snapshot_locations())),
            ("o.ndjson", lines(&[o])),
        ],
    );
    cmd(&config)
        .args(["--only", "locations,views", "--input"])
        .arg(&input)
        .assert()
        .success();
    // The Locations stay partitioned by country; the view table by `part`,
    // its value looked up in the Locations' country.
    assert!(root.join("data/parquet/locations/country=NG").is_dir());
    let obs_rows = table_rows(&root.join("data/parquet/obs"));
    assert_eq!(obs_rows.len(), 1);
    assert_eq!(obs_rows[0].0, "part=NG");
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert!(
        manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 0,
        "{manifest}"
    );
    assert_eq!(manifest["tables"]["obs"]["rows"], 1);
    let catalog = fs::read_to_string(root.join("data/catalog.sql")).unwrap();
    assert!(
        catalog.contains("parquet/locations/country=*/**/*.parquet"),
        "{catalog}"
    );
    assert!(
        catalog.contains("parquet/obs/part=*/**/*.parquet"),
        "{catalog}"
    );
}

#[test]
fn a_file_source_needs_no_server_auth() {
    let (dir, config) = setup(NOWHERE, "");
    // Basic auth without a password, and Google auth besides.
    let text = fs::read_to_string(&config).unwrap().replace(
        "name = \"test\"",
        "name = \"test\"\nusername = \"reader\"\ngoogle_auth = true",
    );
    fs::write(&config, text).unwrap();
    // A server run stops: two ways to authenticate, and no password.
    cmd(&config)
        .args(["--only", "views", "--check"])
        .assert()
        .failure()
        .stderr(has("use one way to authenticate"));
    let input = inputs(
        dir.path(),
        &[(
            "p.ndjson",
            lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
        )],
    );
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .env("FHIR_PIPES_CLIENT_ID", "unused")
        .assert()
        .success()
        .stdout(has("parquet/patient (1 rows)"));
}

#[test]
fn the_tiles_tools_are_needed_only_when_there_are_locations() {
    let (dir, config) = setup(
        NOWHERE,
        "\n[tools]\nduckdb = \"/no/such/duckdb\"\ntippecanoe = \"/no/such/tippecanoe\"\n",
    );
    let root = dir.path();
    fs::remove_dir_all(root.join("data/parquet/locations")).unwrap();
    let patients = inputs(
        root,
        &[(
            "p.ndjson",
            lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
        )],
    );
    // No Locations: the tiles step is skipped, and its missing tools do not
    // matter.
    cmd(&config)
        .arg("--input")
        .arg(&patients)
        .assert()
        .success()
        .stdout(has("== tiles: there is no locations table; skipped"));

    // With Locations the tiles step runs, and stops the run before anything
    // is published.
    let locations = inputs(root, &[("l.ndjson", lines(&snapshot_locations()))]);
    cmd(&config)
        .arg("--input")
        .arg(&locations)
        .assert()
        .failure()
        .stderr(has("not found: /no/such/duckdb (for tiles)"));
    assert!(!root.join("data/parquet/locations").exists());
}
