//! Binary-level tests for `fhir-pipes sync` against a fake sink FHIR server.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::matchers::ExecutionContext;
use httptest::{Expectation, Server, all_of, cycle, matchers::*, responders::*};
use serde_json::{Value, json};

fn patient(id: &str, updated: &str) -> Value {
    json!({"resourceType": "Patient", "id": id, "active": true,
           "meta": {"versionId": "7", "lastUpdated": updated, "tag": [{"code": "keep"}]}})
}

fn loc(id: &str, updated: &str, parent: Option<&str>) -> Value {
    let mut r = json!({"resourceType": "Location", "id": id, "name": id,
                       "meta": {"versionId": "1", "lastUpdated": updated}});
    if let Some(p) = parent {
        r["partOf"] = json!({"reference": format!("Location/{p}")});
    }
    r
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";

/// Write `<TYPE>.ndjson` as an export would.
fn write_type(dir: &Path, resource_type: &str, resources: &[Value]) {
    let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(dir.join(format!("{resource_type}.ndjson")), text).unwrap();
}

fn fhir_pipes() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    for var in [
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
        "FHIR_PIPES_GOOGLE_AUTH",
    ] {
        c.env_remove(var);
    }
    c
}

fn sync(server: &Server, dir: &Path, extra: &[&str]) -> Assert {
    fhir_pipes()
        .args([
            "sync",
            "--server",
            &server.url("/fhir").to_string(),
            "--from",
        ])
        .arg(dir)
        .args(extra)
        .assert()
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn ok(body: String) -> ResponseBuilder<String> {
    status_code(200)
        .insert_header("content-type", "application/fhir+json")
        .body(body)
}

fn expect_metadata(server: &Server) {
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/metadata")).respond_with(ok(
            json!({"resourceType": "CapabilityStatement", "rest": [{"resource": [
                {"type": "Patient", "updateCreate": true},
                {"type": "Location", "updateCreate": true}]}]})
            .to_string(),
        )),
    );
}

fn transaction_response() -> String {
    json!({"resourceType": "Bundle", "type": "transaction-response"}).to_string()
}

type Req = httptest::http::Request<httptest::bytes::Bytes>;

/// Matches a POSTed bundle by a predicate over its JSON.
struct BundleWhere(Box<dyn Fn(&Value) -> bool + Send>);

impl Matcher<Req> for BundleWhere {
    fn matches(&mut self, req: &Req, _ctx: &mut ExecutionContext) -> bool {
        serde_json::from_slice::<Value>(req.body())
            .map(|v| (self.0)(&v))
            .unwrap_or(false)
    }
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("BundleWhere(..)")
    }
}

fn posted_bundle(pred: impl Fn(&Value) -> bool + Send + 'static) -> impl Matcher<Req> {
    all_of![
        request::method_path("POST", "/fhir"),
        BundleWhere(Box::new(pred))
    ]
}

/// The request URLs of a bundle's entries, e.g. `PUT Patient/a`.
fn requests(b: &Value) -> Vec<String> {
    b["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            format!(
                "{} {}",
                e["request"]["method"].as_str().unwrap_or(""),
                e["request"]["url"].as_str().unwrap_or("")
            )
        })
        .collect()
}

fn bundle_is(expected: &'static [&'static str]) -> impl Matcher<Req> {
    posted_bundle(move |b| requests(b) == expected)
}

#[test]
fn puts_carry_no_if_match_and_no_version_fields_and_parents_go_first() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    write_type(
        dir.path(),
        "Location",
        &[
            loc("clinic", T1, Some("ward")),
            loc("ward", T1, Some("state")),
            loc("state", T1, None),
        ],
    );
    expect_metadata(&server);
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            let e = &b["entry"][0];
            b["type"] == "transaction"
                && e["request"] == json!({"method": "PUT", "url": "Patient/a"})
                // versionId and lastUpdated dropped, the tag kept.
                && e["resource"]["meta"] == json!({"tag": [{"code": "keep"}]})
                && e["resource"]["active"] == true
        }))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    // One bundle per partOf depth: state, then ward, then clinic.
    for expected in [
        &["PUT Location/state"][..],
        &["PUT Location/ward"][..],
        &["PUT Location/clinic"][..],
    ] {
        server.expect(
            Expectation::matching(posted_bundle(move |b| {
                requests(b) == expected
                    && b["entry"][0]["request"].get("ifMatch").is_none()
                    && b["entry"][0]["resource"].get("meta").is_none()
            }))
            .times(1)
            .respond_with(ok(transaction_response())),
        );
    }
    let a = sync(
        &server,
        dir.path(),
        &["--type", "Patient", "--type", "Location"],
    )
    .success();
    let out = stdout(&a);
    assert!(
        out.contains("Patient: 1 sent, 0 unchanged, 0 deleted, 0 failed"),
        "{out}"
    );
    assert!(
        out.contains("Location: 3 sent, 0 unchanged, 0 deleted, 0 failed"),
        "{out}"
    );
    let err = stderr(&a);
    let at = |s: &str| err.find(s).unwrap_or_else(|| panic!("{s} in {err}"));
    assert!(at("Location: bundle 1/3 committed") < at("Location: bundle 2/3 committed"));
    assert!(at("Location: bundle 2/3 committed") < at("Location: bundle 3/3 committed"));
}

#[test]
fn a_second_run_sends_only_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    write_type(
        dir.path(),
        "Patient",
        &[patient("a", T1), patient("b", T1), patient("c", T1)],
    );
    let mut server = Server::run();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&[
            "PUT Patient/a",
            "PUT Patient/b",
            "PUT Patient/c",
        ]))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    sync(&server, dir.path(), &[]).success();

    // Nothing changed: no bundle at all.
    server.verify_and_clear();
    expect_metadata(&server);
    let a = sync(&server, dir.path(), &[]).success();
    assert!(
        stdout(&a).contains("Patient: 0 sent, 3 unchanged, 0 deleted, 0 failed"),
        "{}",
        stdout(&a)
    );

    // b updated, d new: only those two.
    write_type(
        dir.path(),
        "Patient",
        &[
            patient("a", T1),
            patient("b", T2),
            patient("c", T1),
            patient("d", T1),
        ],
    );
    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["PUT Patient/b", "PUT Patient/d"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    let a = sync(&server, dir.path(), &[]).success();
    assert!(
        stdout(&a).contains("Patient: 2 sent, 2 unchanged"),
        "{}",
        stdout(&a)
    );

    // --full sends everything again.
    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&[
            "PUT Patient/a",
            "PUT Patient/b",
            "PUT Patient/c",
            "PUT Patient/d",
        ]))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    sync(&server, dir.path(), &["--full"]).success();
}

#[test]
fn each_sink_has_its_own_record_of_what_it_was_sent() {
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    for _ in 0..2 {
        // A second sink gets everything although the first has it already.
        let server = Server::run();
        expect_metadata(&server);
        server.expect(
            Expectation::matching(bundle_is(&["PUT Patient/a"]))
                .times(1)
                .respond_with(ok(transaction_response())),
        );
        sync(&server, dir.path(), &[]).success();
    }
}

#[test]
fn deletions_are_sent_only_with_delete_and_a_mass_deletion_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let all: Vec<Value> = (0..10).map(|i| patient(&format!("p{i}"), T1)).collect();
    write_type(dir.path(), "Patient", &all);
    let mut server = Server::run();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(request::method_path("POST", "/fhir"))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    sync(&server, dir.path(), &[]).success();

    // p0 and p1 gone: 20%, at the default limit.
    write_type(dir.path(), "Patient", &all[2..]);
    server.verify_and_clear();
    expect_metadata(&server);
    let a = sync(&server, dir.path(), &[]).success();
    assert!(
        stdout(&a).contains("2 gone from the download, not deleted without --delete"),
        "{}",
        stdout(&a)
    );

    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["DELETE Patient/p0", "DELETE Patient/p1"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    let a = sync(&server, dir.path(), &["--delete"]).success();
    assert!(
        stdout(&a).contains("Patient: 0 sent, 8 unchanged, 2 deleted, 0 failed"),
        "{}",
        stdout(&a)
    );

    // Deleted once: not again.
    server.verify_and_clear();
    expect_metadata(&server);
    sync(&server, dir.path(), &["--delete"]).success();

    // An empty download would delete everything: refused, nothing sent.
    write_type(dir.path(), "Patient", &[]);
    server.verify_and_clear();
    expect_metadata(&server);
    let a = sync(&server, dir.path(), &["--delete"]).failure().code(2);
    let out = stdout(&a);
    assert!(
        out.contains("8 of 8 synced resources (100%) are gone"),
        "{out}"
    );
    assert!(out.contains("--force-delete"), "{out}");
    assert!(
        stderr(&a).contains("deletions refused for Patient"),
        "{}",
        stderr(&a)
    );

    // A lower limit refuses a smaller share.
    write_type(dir.path(), "Patient", &all[3..]);
    server.verify_and_clear();
    expect_metadata(&server);
    sync(
        &server,
        dir.path(),
        &["--delete", "--max-delete-share", "0.1"],
    )
    .failure()
    .code(2);

    // Forced, it goes ahead.
    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["DELETE Patient/p2"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    sync(
        &server,
        dir.path(),
        &["--delete", "--max-delete-share", "0.1", "--force-delete"],
    )
    .success();
}

#[test]
fn dry_run_sends_nothing_and_prints_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1), patient("b", T1)]);
    let mut server = Server::run();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(request::method_path("POST", "/fhir"))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    sync(&server, dir.path(), &[]).success();

    write_type(
        dir.path(),
        "Patient",
        &[patient("b", T2), patient("c", T1), patient("d", T1)],
    );
    // Only the preflight: any POST would fail the test.
    server.verify_and_clear();
    expect_metadata(&server);
    let a = sync(
        &server,
        dir.path(),
        &["--dry-run", "--delete", "--max-delete-share", "0.5"],
    )
    .success();
    let out = stdout(&a);
    assert!(out.contains("Dry run: nothing sent"), "{out}");
    assert!(
        out.contains("Patient: 3 to send, 0 unchanged, 1 to delete: a"),
        "{out}"
    );

    // The dry run recorded nothing: the real run sends the same.
    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&[
            "PUT Patient/b",
            "PUT Patient/c",
            "PUT Patient/d",
        ]))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    server.expect(
        Expectation::matching(bundle_is(&["DELETE Patient/a"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    sync(
        &server,
        dir.path(),
        &["--delete", "--max-delete-share", "0.5"],
    )
    .success();
}

#[test]
fn repeated_field_names_are_read_as_their_last_value() {
    // As a `Value` reads them, which is how sync read them before.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Patient.ndjson"),
        concat!(
            r#"{"resourceType":"Patient","resourceType":"Patient","id":"a"}"#,
            "\n",
            r#"{"resourceType":"Patient","id":"x","id":"b","meta":{"lastUpdated":"2026-01-01T00:00:00Z"},"meta":{"lastUpdated":"2026-02-01T00:00:00Z"}}"#,
            "\n",
        ),
    )
    .unwrap();
    let mut server = Server::run();
    expect_metadata(&server);
    let a = sync(&server, dir.path(), &["--dry-run"]).success();
    let out = stdout(&a);
    assert!(out.contains("Patient: 2 to send, 0 unchanged"), "{out}");
    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["PUT Patient/a", "PUT Patient/b"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    sync(&server, dir.path(), &[]).success();
}

#[test]
fn a_failed_bundle_is_sent_again_by_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    write_type(
        dir.path(),
        "Patient",
        &[
            patient("a", T1),
            patient("b", T1),
            patient("c", T1),
            patient("d", T1),
        ],
    );
    let mut server = Server::run();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["PUT Patient/a", "PUT Patient/b"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    server.expect(
        Expectation::matching(bundle_is(&["PUT Patient/c", "PUT Patient/d"]))
            .times(1)
            .respond_with(status_code(400).body("invalid resource")),
    );
    let a = sync(&server, dir.path(), &["--batch-size", "2"])
        .failure()
        .code(1);
    assert!(
        stdout(&a).contains("Patient: 2 sent, 0 unchanged, 0 deleted, 2 failed"),
        "{}",
        stdout(&a)
    );
    let err = stderr(&a);
    assert!(
        err.contains("Patient: bundle 2/2 failed: HTTP 400: invalid resource"),
        "{err}"
    );
    assert!(err.contains("sent again by the next run"), "{err}");

    server.verify_and_clear();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(bundle_is(&["PUT Patient/c", "PUT Patient/d"]))
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    let a = sync(&server, dir.path(), &["--batch-size", "2"]).success();
    assert!(
        stdout(&a).contains("Patient: 2 sent, 2 unchanged, 0 deleted, 0 failed"),
        "{}",
        stdout(&a)
    );
}

#[test]
fn a_503_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    let server = Server::run();
    expect_metadata(&server);
    server.expect(
        Expectation::matching(request::method_path("POST", "/fhir"))
            .times(2)
            .respond_with(cycle![status_code(503), ok(transaction_response())]),
    );
    sync(&server, dir.path(), &[]).success();
}

#[test]
fn basic_auth_goes_with_every_request() {
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    let server = Server::run();
    let basic = || request::headers(contains(("authorization", "Basic YWRtaW46QWRtaW4xMjM=")));
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/metadata"),
            basic()
        ])
        .times(1)
        .respond_with(ok(
            json!({"resourceType": "CapabilityStatement"}).to_string()
        )),
    );
    server.expect(
        Expectation::matching(all_of![request::method_path("POST", "/fhir"), basic()])
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    fhir_pipes()
        .args([
            "sync",
            "--server",
            &server.url("/fhir").to_string(),
            "--from",
        ])
        .arg(dir.path())
        .args(["--username", "admin"])
        .env("FHIR_PIPES_PASSWORD", "Admin123")
        .assert()
        .success();
}

#[test]
fn a_bearer_token_goes_with_every_request_including_deletes() {
    let dir = tempfile::tempdir().unwrap();
    write_type(dir.path(), "Patient", &[patient("a", T1), patient("b", T1)]);
    let bearer = || request::headers(contains(("authorization", "Bearer sink-token")));
    let mut server = Server::run();
    let mut run = |expected: &'static [&'static str]| {
        server.verify_and_clear();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/metadata"),
                bearer()
            ])
            .times(1)
            .respond_with(ok(
                json!({"resourceType": "CapabilityStatement"}).to_string()
            )),
        );
        server.expect(
            Expectation::matching(all_of![bundle_is(expected), bearer()])
                .times(1)
                .respond_with(ok(transaction_response())),
        );
        fhir_pipes()
            .args([
                "sync",
                "--server",
                &server.url("/fhir").to_string(),
                "--from",
            ])
            .arg(dir.path())
            .args(["--delete", "--max-delete-share", "1"])
            .env("FHIR_PIPES_TOKEN", "sink-token")
            .assert()
            .success();
    };
    run(&["PUT Patient/a", "PUT Patient/b"]);
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    run(&["DELETE Patient/b"]);
}

#[test]
fn input_problems_are_usage_errors_before_any_request() {
    // No expectations: none of these may reach the server.
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    sync(&server, dir.path(), &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no <TYPE>.ndjson files"));
    sync(&server, dir.path(), &["--type", "Patient"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Patient.ndjson not found"));
    sync(&server, dir.path(), &["--type", "../x"])
        .failure()
        .code(2);

    write_type(dir.path(), "Patient", &[patient("a", T1), patient("a", T1)]);
    sync(&server, dir.path(), &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("duplicate resource Patient/a"));
    write_type(dir.path(), "Patient", &[loc("x", T1, None)]);
    sync(&server, dir.path(), &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("not a Patient"));
    write_type(
        dir.path(),
        "Patient",
        &[json!({"resourceType": "Patient", "id": "a/b"})],
    );
    sync(&server, dir.path(), &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("not a FHIR id"));
    write_type(dir.path(), "Patient", &[patient("a", T1)]);
    sync(&server, dir.path(), &["--max-delete-share", "2"])
        .failure()
        .code(2);
    sync(&server, dir.path(), &["--force-delete"])
        .failure()
        .code(2);
}
