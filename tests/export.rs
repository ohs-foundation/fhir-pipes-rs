//! Binary-level tests for `fhir-pipes export` against a fake FHIR server.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use predicates::prelude::*;
use serde_json::{Value, json};

fn fhir_pipes() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    for var in [
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
        "FHIR_PIPES_TOKEN_AUTH",
        "FHIR_PIPES_SCOPE",
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
        "FHIR_PIPES_GOOGLE_AUTH",
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
    // More than one page: the count decides whether to split the type into
    // _lastUpdated windows. Three resources are too few.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_summary", "count")))),
            bearer(),
        ])
        .times(1)
        .respond_with(
            status_code(200).body(r#"{"resourceType":"Bundle","type":"searchset","total":3}"#),
        ),
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

/// `parts` as one gzip body of a member each (RFC 1952 section 2.2).
fn gzip_members(parts: &[&[u8]]) -> Vec<u8> {
    use std::io::Write as _;
    let mut body = Vec::new();
    for part in parts {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(part).unwrap();
        body.extend(e.finish().unwrap());
    }
    body
}

#[test]
fn a_search_page_in_several_gzip_members_is_read_whole() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    let page = bundle(
        vec![
            json!({"resourceType": "Measure", "id": "m1"}),
            json!({"resourceType": "Measure", "id": "m2"}),
            json!({"resourceType": "Measure", "id": "m3"}),
        ],
        None,
    );
    let third = page.len() / 3;
    let (a, rest) = page.as_bytes().split_at(third);
    let (b, c) = rest.split_at(third);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Measure"),
            request::headers(contains(("accept-encoding", matches("gzip")))),
            request::headers(contains(("cache-control", "no-cache"))),
        ])
        .times(1)
        .respond_with(
            status_code(200)
                .insert_header("Content-Encoding", "gzip")
                .body(gzip_members(&[a, b, c])),
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args(["export", "--server", &base, "--type", "Measure"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    let text = stdout(&a);
    assert!(text.contains("Measure: 3 resources, 1 pages"), "{text}");
    let ids: Vec<String> = read_ndjson(&dir.path().join("Measure.ndjson"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["m1", "m2", "m3"]);
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

/// A token endpoint that takes the client id and secret only in a Basic
/// header (client_secret_basic, the form Aidbox documents), with the scope.
#[test]
fn token_auth_basic_sends_the_client_credentials_in_a_basic_header() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/token"),
            // base64("key-x:shh")
            request::headers(contains(("authorization", "Basic a2V5LXg6c2ho"))),
            request::body(url_decoded(contains(("grant_type", "client_credentials")))),
            request::body(url_decoded(contains(("scope", "system/*.read")))),
            request::body(url_decoded(not(contains(key("client_secret"))))),
        ])
        .times(1)
        .respond_with(json_encoded(
            json!({"access_token": "tok", "expires_in": 3600}),
        )),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Patient"),
            request::headers(contains(("authorization", "Bearer tok"))),
        ])
        .respond_with(status_code(200).body(bundle(
            vec![json!({"resourceType": "Patient", "id": "p1"})],
            None,
        ))),
    );
    let dir = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args(["export", "--server", &base, "--type", "Patient"])
        .args(["--client-id", "key-x", "--token-url"])
        .arg(server.url_str("/token"))
        .args(["--token-auth", "basic", "--scope", "system/*.read"])
        .arg("--out")
        .arg(dir.path())
        .env("FHIR_PIPES_CLIENT_SECRET", "shh")
        .assert()
        .success();
    assert_eq!(read_ndjson(&dir.path().join("Patient.ndjson")).len(), 1);
}

#[test]
fn token_auth_and_scope_need_a_client_id() {
    let dir = tempfile::tempdir().unwrap();
    for flag in [["--token-auth", "basic"], ["--scope", "system/*.read"]] {
        fhir_pipes()
            .args([
                "export",
                "--server",
                "http://127.0.0.1:9/fhir",
                "--type",
                "Patient",
            ])
            .args(flag)
            .arg("--out")
            .arg(dir.path())
            .assert()
            .code(2)
            .stderr(predicates::str::contains("--client-id"));
    }
}

/// A request quota of one window: 429 with Medplum's `RateLimit` header
/// (seconds left in `t=`, no Retry-After) until `window` after the first
/// request, then `body`.
#[derive(Clone)]
struct Quota {
    window: std::time::Duration,
    first: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    body: String,
}

impl Responder for Quota {
    fn respond<'a>(
        &mut self,
        _req: &'a httptest::http::Request<httptest::bytes::Bytes>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = httptest::http::Response<httptest::bytes::Bytes>> + Send + 'a>,
    > {
        let first = *self
            .first
            .lock()
            .unwrap()
            .get_or_insert_with(std::time::Instant::now);
        let left = self.window.saturating_sub(first.elapsed());
        let response = httptest::http::Response::builder();
        let response = if left.is_zero() {
            response
                .status(200)
                .body(httptest::bytes::Bytes::from(self.body.clone()))
        } else {
            response
                .status(429)
                .header(
                    "RateLimit",
                    format!("\"requests\";r=0;t={}", left.as_secs_f64().ceil()),
                )
                .body(httptest::bytes::Bytes::new())
        };
        let response = response.unwrap();
        Box::pin(async move { response })
    }
}

/// Medplum answers a 429 with `RateLimit: …;t=<seconds>` and no
/// Retry-After. The search waits that long; the backoff alone (0.5, 1 s)
/// would use up the three tries inside the window.
#[test]
fn a_429_with_only_a_ratelimit_header_waits_for_its_reset() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Patient"))
            .times(2)
            .respond_with(Quota {
                window: std::time::Duration::from_secs(2),
                first: Default::default(),
                body: bundle(vec![json!({"resourceType": "Patient", "id": "p1"})], None),
            }),
    );
    let dir = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args(["export", "--server", &base, "--type", "Patient"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    assert_eq!(read_ndjson(&dir.path().join("Patient.ndjson")).len(), 1);
}

/// A service account key file with the test-only key under
/// tests/fixtures/google, whose tokens come from `token_uri`.
fn service_account_file(dir: &Path, token_uri: &str) -> std::path::PathBuf {
    let key = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/google/test-only-service-account-key.pem"),
    )
    .unwrap();
    let path = dir.join("key.json");
    std::fs::write(
        &path,
        json!({
            "type": "service_account",
            "private_key_id": "test-key-id",
            "private_key": key,
            "client_email": "pipes-test@test-project.iam.gserviceaccount.com",
            "token_uri": token_uri,
        })
        .to_string(),
    )
    .unwrap();
    path
}

#[test]
fn google_auth_signs_in_with_a_service_account_key() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/token"),
            request::body(url_decoded(contains((
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer"
            )))),
            request::body(url_decoded(contains(key("assertion")))),
        ])
        .times(1)
        .respond_with(json_encoded(json!({
            "access_token": "ya29.test",
            "expires_in": 3599,
            "token_type": "Bearer",
        }))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Patient"),
            request::headers(contains(("authorization", "Bearer ya29.test"))),
        ])
        .times(2)
        .respond_with(status_code(200).body(bundle(
            vec![json!({"resourceType": "Patient", "id": "p1"})],
            None,
        ))),
    );
    let dir = tempfile::tempdir().unwrap();
    let key = service_account_file(dir.path(), &server.url_str("/token"));
    let out = dir.path().join("out");
    let a = fhir_pipes()
        .args([
            "export",
            "--server",
            &base,
            "--google-auth",
            "--type",
            "Patient",
        ])
        .arg("--out")
        .arg(&out)
        .env("GOOGLE_APPLICATION_CREDENTIALS", &key)
        .assert()
        .success();
    assert_eq!(read_ndjson(&out.join("Patient.ndjson")).len(), 1);
    let printed = format!("{}{}", stdout(&a), stderr(&a));
    for secret in ["ya29.test", "PRIVATE KEY"] {
        assert!(!printed.contains(secret), "{printed}");
    }

    // FHIR_PIPES_GOOGLE_AUTH=1 is the same as the flag.
    server.expect(
        Expectation::matching(request::method_path("POST", "/token"))
            .times(1)
            .respond_with(json_encoded(
                json!({"access_token": "ya29.test", "expires_in": 3599}),
            )),
    );
    fhir_pipes()
        .args(["export", "--server", &base, "--type", "Patient"])
        .arg("--out")
        .arg(&out)
        .env("GOOGLE_APPLICATION_CREDENTIALS", &key)
        .env("FHIR_PIPES_GOOGLE_AUTH", "1")
        .assert()
        .success();
}

#[test]
fn google_auth_excludes_the_other_credentials() {
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
        .args(["--google-auth", "--token", "tok"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with --token"));
    export()
        .args(["--username", "admin", "--password", "x"])
        .env("FHIR_PIPES_GOOGLE_AUTH", "true")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with --username"));
    // FHIR_PIPES_GOOGLE_AUTH=0 turns it off rather than conflicting.
    export()
        .args(["--token", "tok", "--retries", "1"])
        .env("FHIR_PIPES_GOOGLE_AUTH", "0")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with").not());
}

#[test]
fn a_missing_google_key_file_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args([
            "export",
            "--server",
            "projects/p/locations/l/datasets/d/fhirStores/s",
            "--google-auth",
            "--type",
            "Patient",
        ])
        .arg("--out")
        .arg(dir.path())
        .env(
            "GOOGLE_APPLICATION_CREDENTIALS",
            dir.path().join("missing.json"),
        )
        .assert()
        .failure()
        .stderr(predicates::str::contains("GOOGLE_APPLICATION_CREDENTIALS"))
        .stderr(predicates::str::contains("missing.json"));
}

/// Answers with `body` after `delay`, recording the most requests the server
/// had in flight at once (shared by every `Recorded` with the same counters).
#[derive(Clone)]
struct Recorded {
    body: String,
    delay: std::time::Duration,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    most: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl httptest::responders::Responder for Recorded {
    fn respond<'a>(
        &mut self,
        _req: &'a httptest::http::Request<httptest::bytes::Bytes>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = httptest::http::Response<httptest::bytes::Bytes>>
                + Send
                + 'a,
        >,
    > {
        use std::sync::atomic::Ordering::SeqCst;
        let me = self.clone();
        Box::pin(async move {
            let now = me.in_flight.fetch_add(1, SeqCst) + 1;
            me.most.fetch_max(now, SeqCst);
            tokio::time::sleep(me.delay).await;
            me.in_flight.fetch_sub(1, SeqCst);
            httptest::http::Response::builder()
                .status(200)
                .body(httptest::bytes::Bytes::from(me.body))
                .unwrap()
        })
    }
}

const TYPES: [&str; 6] = [
    "Patient",
    "Encounter",
    "Observation",
    "Condition",
    "Procedure",
    "Immunization",
];

/// A server with two pages of each of `TYPES`, every answer 100 ms late.
/// Returns the most requests it had in flight at once.
fn slow_server() -> (Server, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let server = Server::run();
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let most = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = |body: String| Recorded {
        body,
        delay: std::time::Duration::from_millis(100),
        in_flight: in_flight.clone(),
        most: most.clone(),
    };
    for t in TYPES {
        let resource = |id: String| json!({"resourceType": t, "id": id});
        let mut first: Value = serde_json::from_str(&bundle(
            vec![resource(format!("{t}-b")), resource(format!("{t}-a"))],
            Some(format!("/fhir/{t}?page=2")),
        ))
        .unwrap();
        // A count on the first page: too few to split into windows.
        first["total"] = json!(3);
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", format!("/fhir/{t}")),
                request::query(url_decoded(not(contains(key("page"))))),
            ])
            .respond_with(recorded(first.to_string())),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", format!("/fhir/{t}")),
                request::query(url_decoded(contains(("page", "2")))),
            ])
            .respond_with(recorded(bundle(vec![resource(format!("{t}-c"))], None))),
        );
    }
    (server, most)
}

fn export_types(server: &Server, out: &Path, concurrency: &str) -> Assert {
    let mut c = fhir_pipes();
    c.args(["export", "--server", &server.url_str("/fhir")]);
    for t in TYPES {
        c.args(["--type", t]);
    }
    c.args(["--concurrency", concurrency])
        .arg("--out")
        .arg(out)
        .arg("--report")
        .arg(out.join("report.json"))
        .assert()
        .success()
}

#[test]
fn types_downloaded_side_by_side_give_the_same_files() {
    let dir = tempfile::tempdir().unwrap();
    let (server, most) = slow_server();
    export_types(&server, &dir.path().join("one"), "1");
    assert_eq!(most.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(server);

    let (server, most) = slow_server();
    let a = export_types(&server, &dir.path().join("four"), "4");
    let most = most.load(std::sync::atomic::Ordering::SeqCst);
    assert!((2..=4).contains(&most), "{most} requests at once");
    for t in TYPES {
        assert!(
            stdout(&a).contains(&format!("{t}: 3 resources, 2 pages")),
            "{}",
            stdout(&a)
        );
        let name = format!("{t}.ndjson");
        let one = std::fs::read(dir.path().join("one").join(&name)).unwrap();
        let four = std::fs::read(dir.path().join("four").join(&name)).unwrap();
        assert_eq!(one, four, "{name}");
    }
    assert_eq!(
        std::fs::read(dir.path().join("one/report.json")).unwrap(),
        std::fs::read(dir.path().join("four/report.json")).unwrap()
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("four/.export-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["types"].as_object().unwrap().len(), TYPES.len());
}

#[test]
fn concurrency_limits_requests_across_types() {
    let dir = tempfile::tempdir().unwrap();
    let (server, most) = slow_server();
    export_types(&server, dir.path(), "2");
    assert_eq!(most.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn concurrency_must_be_at_least_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args([
            "export",
            "--server",
            "http://127.0.0.1:9/fhir",
            "--type",
            "Patient",
            "--concurrency",
            "0",
        ])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .code(2);
    assert!(stderr(&a).contains("--concurrency"), "{}", stderr(&a));
}

#[test]
fn a_large_type_on_a_server_that_cannot_sort_is_paged_as_one_stream() {
    let server = Server::run();
    let base = server.url_str("/fhir");
    let mut first: Value = serde_json::from_str(&bundle(
        vec![report("a"), report("b")],
        Some(format!("{base}/MeasureReport?page=2")),
    ))
    .unwrap();
    // Large enough to split into _lastUpdated windows.
    first["total"] = json!(20_000);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(not(contains(key("page"))))),
            request::query(url_decoded(not(contains(key("_sort"))))),
        ])
        // Once to decide, once more as one stream.
        .times(2)
        .respond_with(status_code(200).body(first.to_string())),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("_sort", "_lastUpdated")))),
        ])
        .times(1)
        .respond_with(status_code(400).body(r#"{"resourceType":"OperationOutcome"}"#)),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/MeasureReport"),
            request::query(url_decoded(contains(("page", "2")))),
        ])
        .times(1)
        .respond_with(status_code(200).body(bundle(vec![report("c")], None))),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args(["export", "--server", &base, "--type", "MeasureReport"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    let text = stdout(&a);
    assert!(
        text.contains("MeasureReport: the server did not sort by _lastUpdated"),
        "{text}"
    );
    assert!(text.contains("paging it as one stream"), "{text}");
    assert!(
        text.contains("MeasureReport: 3 resources, 2 pages"),
        "{text}"
    );
    let ids: Vec<Value> = read_ndjson(&dir.path().join("MeasureReport.ndjson"))
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
}
