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
    // Nothing but the two files: the temporary file was renamed into place.
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);
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
