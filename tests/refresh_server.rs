//! End-to-end runs of `fhir-pipes refresh` against a fake FHIR server serving
//! the fixture NDJSON: downloads, incremental and bulk runs, auth.

mod common;

use std::fs;
use std::path::Path;

use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use predicates::prelude::*;
use predicates::str::contains as has;
use serde_json::{Value, json};

use common::*;

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

/// A run that downloads nothing records no server it did not contact: the
/// one in raw/'s export state if there is one, else none.
#[test]
fn a_run_that_downloads_nothing_records_the_server_of_the_downloads() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let read = |p: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(data.join(p)).unwrap()).unwrap()
    };
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    assert_eq!(read("manifest.json")["server"], Value::Null);
    assert_eq!(read("raw/test/last-refresh.json")["server"], Value::Null);

    fs::write(
        data.join("raw/test/.export-state.json"),
        r#"{"server": "https://hapi.example.org/fhir", "types": {}}"#,
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    assert_eq!(
        read("manifest.json")["server"],
        "https://hapi.example.org/fhir"
    );
    assert_eq!(
        read("raw/test/last-refresh.json")["server"],
        "https://hapi.example.org/fhir"
    );
}

/// --reuse-raw transforms the Location snapshot in raw/ without extracting.
#[test]
fn reuse_raw_transforms_the_location_snapshot_without_extracting() {
    let (dir, config) = setup(NOWHERE, "");
    let snap = dir.path().join("data/raw/test/locations");
    fs::create_dir_all(&snap).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson"),
        snap.join("locations.ndjson"),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "locations", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has("--reuse-raw:"))
        .stdout(has("no extract"));
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("data/manifest.json")).unwrap())
            .unwrap();
    assert!(manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 2);
    assert_eq!(manifest["server"], Value::Null);
}

#[test]
fn zero_threads_are_refused() {
    let (_dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--threads", "0", "--check"])
        .assert()
        .failure()
        .stderr(has("--threads"));
}

/// `--server-kind` and FHIR_PIPES_SERVER_KIND win over `server.kind`, the
/// flag over the environment.
#[test]
fn the_server_kind_flag_and_environment_override_the_config() {
    let (_dir, config) = setup(NOWHERE, "");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("name = \"test\"\n", "name = \"test\"\nkind = \"medplum\"\n");
    fs::write(&config, text).unwrap();
    cmd(&config)
        .args(["--only", "views", "--check"])
        .assert()
        .success()
        .stdout(has("kind:       medplum"));
    cmd(&config)
        .args(["--only", "views", "--check"])
        .env("FHIR_PIPES_SERVER_KIND", "aidbox")
        .assert()
        .success()
        .stdout(has("kind:       aidbox"));
    cmd(&config)
        .args(["--only", "views", "--check", "--server-kind", "generic"])
        .env("FHIR_PIPES_SERVER_KIND", "aidbox")
        .assert()
        .success()
        .stdout(has("kind:       generic"));
    cmd(&config)
        .args(["--only", "views", "--check", "--server-kind", "fhirbase"])
        .assert()
        .code(2);
}

/// `--check` names the partition lookups, or says there are none.
#[test]
fn check_describes_the_partition_lookups() {
    let (_dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--only", "views", "--check"])
        .assert()
        .success()
        .stdout(has("partition:  country via location_id → locations\n"));
    let text = fs::read_to_string(&config).unwrap();
    let start = text.find("[[partition.lookup]]").unwrap();
    fs::write(&config, &text[..start]).unwrap();
    cmd(&config)
        .args(["--only", "views", "--check"])
        .assert()
        .success()
        .stdout(has(
            "partition:  country, no [[partition.lookup]] rules: view rows go to the null partition\n",
        ));
}
