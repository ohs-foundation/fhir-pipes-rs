//! End-to-end runs of `fhir-pipes refresh` from files instead of a server.

mod common;

use std::fs;
use std::path::Path;

use httptest::{Expectation, Server, responders::*};
use predicates::prelude::*;
use predicates::str::contains as has;
use serde_json::{Value, json};

use common::*;

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

/// `text` as one gzip member.
fn gzip(text: &str) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(text.as_bytes()).unwrap();
    e.finish().unwrap()
}

#[test]
fn refresh_reads_gzip_inputs() {
    let (dir, config) = setup(NOWHERE, "");
    let root = dir.path();
    let input = inputs(
        root,
        &[(
            "b.ndjson",
            // o1 again, older than in a.ndjson.gz: that copy is kept.
            lines(&[obs("o1", 3, Some("2026-01-01T00:00:00Z"))]),
        )],
    );
    // Two gzip members, as `cat` of two .gz files writes them.
    let mut a = gzip(&lines(&[obs("o1", 30, Some("2026-03-01T00:00:00Z"))]));
    a.extend(gzip(&lines(&[
        obs("o2", 5, None),
        json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-05-01"}),
    ])));
    fs::write(input.join("a.ndjson.gz"), a).unwrap();
    fs::create_dir_all(input.join("sub")).unwrap();
    fs::write(
        input.join("sub/c.json.gz"),
        gzip(&bundle(vec![obs("o4", 4, None)])),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .assert()
        .success()
        .stdout(has("== files (3 files"))
        .stdout(has("Observation: 3 resources (1 duplicates by id merged)"))
        .stdout(has("parquet/patient (1 rows)"));
    let raw = read_ndjson(&root.join("data/raw/test/Observation.ndjson"));
    let got: Vec<(&str, i64)> = raw
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap(),
                r["valueInteger"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, [("o1", 30), ("o2", 5), ("o4", 4)]);
    // A line of a gzip input is copied as it is written there.
    let text = fs::read_to_string(root.join("data/raw/test/Observation.ndjson")).unwrap();
    assert!(text.starts_with(&format!(
        "{}\n",
        obs("o1", 30, Some("2026-03-01T00:00:00Z"))
    )));

    // A file beside its gzip copy is refused: both would be read.
    fs::write(input.join("a.ndjson"), "{}\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .assert()
        .code(2)
        .stderr(has("a.ndjson.gz"));
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
        // The pattern is shown as a path, with `\` on Windows.
        .stdout(has("*.ndjson: input pattern").and(has("matches nothing")))
        .stdout(has("server:").not())
        .stdout(has("auth:").not());
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
