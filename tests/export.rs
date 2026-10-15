//! Binary-level tests for `fhir-pipes export` against a fake FHIR server.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use serde_json::{Value, json};

fn fhir_pipes() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    for var in [
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
    ] {
        c.env_remove(var);
    }
    c
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn read_ndjson(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn bundle(resources: Vec<Value>, next: Option<String>) -> String {
    let mut b = json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
    });
    if let Some(n) = next {
        b["link"] = json!([{"relation": "next", "url": n}]);
    }
    b.to_string()
}

fn report(id: &str) -> Value {
    json!({"resourceType": "MeasureReport", "id": id, "status": "complete"})
}

#[test]
fn export_follows_next_links_and_writes_one_file_per_type() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    let bearer = || request::headers(contains(("authorization", "Bearer tok")));
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_count", "1000")))),
            request::query(url_decoded(not(contains(key("page"))))),
            bearer(),
        ])
        .respond_with(status_code(200).body(bundle(
            vec![report("a"), report("b")],
            Some(format!("{base}/MeasureReport?_count=1000&page=2")),
        ))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("page", "2")))),
            bearer(),
        ])
        .respond_with(status_code(200).body(bundle(vec![report("c")], None))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Measure"),
            bearer()
        ])
        .respond_with(status_code(200).body(bundle(
            vec![json!({"resourceType": "Measure", "id": "m1"})],
            None,
        ))),
    );

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let report_path = dir.path().join("report.json");
    let a = fhir_pipes()
        .args(["export", "--server", &base, "--token", "tok"])
        .args([
            "--type",
            "MeasureReport",
            "--type",
            "Measure",
            "--type",
            "MeasureReport",
        ])
        .arg("--out")
        .arg(&out)
        .arg("--report")
        .arg(&report_path)
        .assert()
        .success();
    let text = stdout(&a);
    assert!(
        text.contains("MeasureReport: 3 resources, 2 pages"),
        "{text}"
    );
    assert!(text.contains("Measure: 1 resources, 1 pages"), "{text}");
    assert!(text.contains("No issues found."), "{text}");

    let ids: Vec<String> = read_ndjson(&out.join("MeasureReport.ndjson"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(read_ndjson(&out.join("Measure.ndjson")).len(), 1);
    // The two files and the export state: the temporary file was renamed
    // into place.
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ".export-state.json",
            "Measure.ndjson",
            "MeasureReport.ndjson"
        ]
    );
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(r["counts"], json!({}));
}

#[test]
fn a_failed_export_keeps_the_previous_file() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Measure"))
            .times(1..)
            .respond_with(status_code(500).body("boom")),
    );
    let dir = tempfile::tempdir().unwrap();
    let previous = dir.path().join("Measure.ndjson");
    std::fs::write(&previous, "{\"resourceType\":\"Measure\",\"id\":\"old\"}\n").unwrap();

    let a = fhir_pipes()
        .args([
            "export",
            "--server",
            &base,
            "--type",
            "Measure",
            "--retries",
            "1",
        ])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .failure();
    assert!(stderr(&a).contains("Measure"), "{}", stderr(&a));
    assert_eq!(read_ndjson(&previous)[0]["id"], "old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn a_full_last_page_is_checked_against_the_server_count() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    // Exactly one full page and no next link: what a server that stops
    // paging early looks like.
    let page: Vec<Value> = (0..1000).map(|i| report(&format!("r{i}"))).collect();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_count", "1000")))),
        ])
        .respond_with(status_code(200).body(bundle(page, None))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_summary", "count")))),
        ])
        .respond_with(
            status_code(200).body(r#"{"resourceType":"Bundle","type":"searchset","total":1500}"#),
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args(["export", "--server", &base, "--type", "MeasureReport"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    let text = stdout(&a);
    assert!(text.contains("count_mismatch: 1"), "{text}");
    assert_eq!(
        read_ndjson(&dir.path().join("MeasureReport.ndjson")).len(),
        1000
    );
}

#[test]
fn a_type_that_is_not_a_resource_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args([
            "export",
            "--server",
            "http://127.0.0.1:9/fhir",
            "--type",
            "../Location",
        ])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("not a FHIR resource type name"),
        "{}",
        stderr(&a)
    );
}

fn updated(id: &str, at: &str, status: &str) -> Value {
    json!({"resourceType": "MeasureReport", "id": id, "status": status, "meta": {"lastUpdated": at}})
}

fn count_reply(server: &Server, n: u64) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_summary", "count")))),
        ])
        .times(1)
        .respond_with(
            status_code(200).body(
                json!({"resourceType": "Bundle", "type": "searchset", "total": n}).to_string(),
            ),
        ),
    );
}

fn export_incremental(server: &Server, out: &Path) -> Assert {
    fhir_pipes()
        .args([
            "export",
            "--incremental",
            "--type",
            "MeasureReport",
            "--server",
        ])
        .arg(server.url_str("/fhir"))
        .arg("--out")
        .arg(out)
        .assert()
        .success()
}

#[test]
fn incremental_export_merges_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");

    // First run: no state yet, so a full download that records the watermark.
    let mut server = Server::run();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(
            vec![
                updated("a", "2026-10-01T00:00:00Z", "complete"),
                updated("b", "2026-10-02T00:00:00Z", "complete"),
            ],
            None,
        ))),
    );
    let a = export_incremental(&server, &out);
    assert!(
        stdout(&a).contains("MeasureReport: 2 resources"),
        "{}",
        stdout(&a)
    );
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(".export-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["types"]["MeasureReport"]["watermark"],
        "2026-10-02T00:00:00Z"
    );
    server.verify_and_clear();

    // Second run: only what changed since the watermark, merged by id. b is
    // refetched (ge) and updated, c is new, a is kept from the file.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains((
                "_lastUpdated",
                "ge2026-10-02T00:00:00Z"
            )))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(
            vec![
                updated("b", "2026-10-03T00:00:00Z", "error"),
                updated("c", "2026-10-03T00:00:00Z", "complete"),
            ],
            None,
        ))),
    );
    count_reply(&server, 3);
    let a = export_incremental(&server, &out);
    assert!(
        stdout(&a).contains(
            "MeasureReport: 1 new, 1 updated or fetched again since 2026-10-02T00:00:00Z, 3 in all"
        ),
        "{}",
        stdout(&a)
    );
    let rows = read_ndjson(&out.join("MeasureReport.ndjson"));
    let by_id = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap().clone();
    assert_eq!(rows.len(), 3);
    assert_eq!(by_id("a")["status"], "complete");
    assert_eq!(by_id("b")["status"], "error");
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(".export-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["types"]["MeasureReport"]["watermark"],
        "2026-10-03T00:00:00Z"
    );
    assert_eq!(state["types"]["MeasureReport"]["count"], 3);
}

#[test]
fn a_count_mismatch_after_an_incremental_merge_downloads_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("MeasureReport.ndjson"),
        format!(
            "{}\n{}\n",
            updated("a", "2026-10-01T00:00:00Z", "complete"),
            updated("gone", "2026-10-01T00:00:00Z", "complete")
        ),
    )
    .unwrap();
    let server = Server::run();
    std::fs::write(
        out.join(".export-state.json"),
        json!({"server": server.url_str("/fhir"), "types": {"MeasureReport": {
            "watermark": "2026-10-01T00:00:00Z", "count": 2,
            "completed_at": "2026-10-01T00:00:00Z", "fhir_pipes_version": "0.3.0"}}})
        .to_string(),
    )
    .unwrap();

    // Nothing changed, but `gone` was deleted on the server: 1, not 2.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(key("_lastUpdated")))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(
            vec![updated("a", "2026-10-01T00:00:00Z", "complete")],
            None,
        ))),
    );
    count_reply(&server, 1);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(not(contains(key("_lastUpdated"))))),
            request::query(url_decoded(not(contains(key("_summary"))))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(
            vec![updated("a", "2026-10-01T00:00:00Z", "complete")],
            None,
        ))),
    );
    let a = export_incremental(&server, &out);
    assert!(stdout(&a).contains("downloading in full"), "{}", stdout(&a));
    let rows = read_ndjson(&out.join("MeasureReport.ndjson"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "a");
}

#[test]
fn state_from_another_server_is_not_used() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("MeasureReport.ndjson"),
        format!("{}\n", updated("old", "2026-10-01T00:00:00Z", "complete")),
    )
    .unwrap();
    std::fs::write(
        out.join(".export-state.json"),
        json!({"server": "http://elsewhere.test/fhir", "types": {"MeasureReport": {
            "watermark": "2026-10-01T00:00:00Z", "count": 1,
            "completed_at": "2026-10-01T00:00:00Z", "fhir_pipes_version": "0.3.0"}}})
        .to_string(),
    )
    .unwrap();
    let server = Server::run();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(
            vec![updated("new", "2026-10-05T00:00:00Z", "complete")],
            None,
        ))),
    );
    export_incremental(&server, &out);
    let rows = read_ndjson(&out.join("MeasureReport.ndjson"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "new");
}

#[test]
fn the_watermark_never_passes_the_servers_clock_at_the_start() {
    // A resource stamped after the search began (here, absurdly, in 2099)
    // must not push the watermark past the server's clock: an update made
    // while paging would be skipped by every later run.
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/MeasureReport"))
            .times(1)
            .respond_with(status_code(200).body(bundle(
                vec![updated("a", "2099-01-01T00:00:00Z", "complete")],
                None,
            ))),
    );
    export_incremental(&server, &out);
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(".export-state.json")).unwrap())
            .unwrap();
    let watermark = state["types"]["MeasureReport"]["watermark"]
        .as_str()
        .unwrap();
    assert!(watermark < "2099", "{watermark}");
}

#[test]
fn basic_auth_authorises_every_request() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Patient"),
            request::headers(contains(("authorization", "Basic YWRtaW46QWRtaW4xMjM="))),
        ])
        .respond_with(status_code(200).body(bundle(
            vec![json!({"resourceType": "Patient", "id": "p1"})],
            None,
        ))),
    );
    let dir = tempfile::tempdir().unwrap();
    // The password from the environment, the username as a flag.
    fhir_pipes()
        .args([
            "export",
            "--server",
            &base,
            "--username",
            "admin",
            "--type",
            "Patient",
        ])
        .arg("--out")
        .arg(dir.path())
        .env("FHIR_PIPES_PASSWORD", "Admin123")
        .assert()
        .success();
    assert_eq!(read_ndjson(&dir.path().join("Patient.ndjson")).len(), 1);
}

#[test]
fn basic_auth_needs_a_password_and_no_other_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let export = || {
        let mut c = fhir_pipes();
        c.args([
            "export",
            "--server",
            "http://127.0.0.1:9/fhir",
            "--type",
            "Patient",
        ])
        .arg("--out")
        .arg(dir.path());
        c
    };
    export()
        .args(["--username", "admin"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--password"));
    export()
        .args(["--username", "admin", "--password", "x", "--token", "tok"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
    export()
        .args(["--username", "admin", "--password", "x"])
        .env("FHIR_PIPES_CLIENT_ID", "key-x")
        .env("FHIR_PIPES_CLIENT_SECRET", "shh")
        .env("FHIR_PIPES_TOKEN_URL", "http://127.0.0.1:9/token")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}
