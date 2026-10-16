//! Binary-level tests for `fhir-pipes export --bulk` against a fake FHIR
//! server that answers the Bulk Data `$export` kick-off, its status URL and
//! the output files.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

fn ids(p: &Path) -> Vec<String> {
    let mut ids: Vec<String> = read_ndjson(p)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

fn state(out: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(out.join(".export-state.json")).unwrap()).unwrap()
}

fn patient(id: &str, at: &str) -> Value {
    json!({"resourceType": "Patient", "id": id, "meta": {"lastUpdated": at}})
}

fn ndjson(resources: &[Value]) -> String {
    resources.iter().map(|r| format!("{r}\n")).collect()
}

const TRANSACTION_TIME: &str = "2026-10-15T08:00:00Z";

/// The kick-off for `types` (comma-separated) answers 202 with a status URL.
fn expect_kick_off(server: &Server, types: &str, since: Option<&str>, status: &str) {
    let kick_off = all_of![
        request::method_path("GET", "/fhir/$export"),
        request::query(url_decoded(contains(("_type", types.to_string())))),
        request::headers(contains(("prefer", "respond-async"))),
        request::headers(contains(("accept", "application/fhir+json"))),
    ];
    let expectation = match since {
        Some(s) => Expectation::matching(all_of![
            kick_off,
            request::query(url_decoded(contains(("_since", s.to_string())))),
        ]),
        None => Expectation::matching(all_of![
            kick_off,
            request::query(url_decoded(not(contains(key("_since"))))),
        ]),
    };
    server.expect(
        expectation
            .times(1)
            .respond_with(status_code(202).insert_header("Content-Location", status)),
    );
}

/// A completion manifest listing `output` as (type, path) pairs.
fn manifest(server: &Server, output: &[(&str, &str)], requires_token: bool) -> Value {
    json!({
        "transactionTime": TRANSACTION_TIME,
        "request": server.url_str("/fhir/$export"),
        "requiresAccessToken": requires_token,
        "output": output.iter().map(|(t, p)| json!({"type": t, "url": server.url_str(p)})).collect::<Vec<_>>(),
        "deleted": [],
        "error": [],
    })
}

fn done(m: &Value) -> impl Responder + use<> {
    status_code(200)
        .insert_header("Content-Type", "application/json")
        .body(m.to_string())
}

fn expect_file(server: &Server, path: &str, body: String) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", path.to_string()),
            request::headers(contains(("accept", "application/fhir+ndjson"))),
        ])
        .times(1)
        .respond_with(status_code(200).body(body)),
    );
}

/// The DELETE of the status URL, after the files are read or to cancel.
fn expect_delete(server: &Server, path: &str) {
    server.expect(
        Expectation::matching(request::method_path("DELETE", path.to_string()))
            .times(1)
            .respond_with(status_code(202)),
    );
}

fn export_bulk(server: &Server, out: &Path, types: &[&str]) -> Command {
    let mut c = fhir_pipes();
    c.args(["export", "--bulk", "--server"])
        .arg(server.url_str("/fhir"))
        .arg("--out")
        .arg(out);
    for t in types {
        c.args(["--type", t]);
    }
    c
}

#[test]
fn a_bulk_export_is_polled_until_done_and_written_per_type() {
    let server = Server::run();
    let bearer = || request::headers(contains(("authorization", "Bearer tok")));
    expect_kick_off(
        &server,
        "Observation,Patient",
        None,
        &server.url_str("/status/1"),
    );
    let m = manifest(
        &server,
        &[
            ("Patient", "/files/p1.ndjson"),
            ("Patient", "/files/p2.ndjson"),
            ("Observation", "/files/o1.ndjson"),
        ],
        true,
    );
    server.expect(
        Expectation::matching(all_of![request::method_path("GET", "/status/1"), bearer()])
            .times(2)
            .respond_with(cycle![
                status_code(202)
                    .insert_header("X-Progress", "Exported 2 of 3 types")
                    .insert_header("Retry-After", "1"),
                done(&m),
            ]),
    );
    // The manifest asks for the token, so the files get it.
    for (path, body) in [
        (
            "/files/p1.ndjson",
            ndjson(&[patient("a", "2026-10-01T00:00:00Z")]),
        ),
        (
            "/files/p2.ndjson",
            // A blank line and a line without an id among the resources.
            format!(
                "{}\n\n{}\n",
                patient("b", "2026-10-02T00:00:00Z"),
                json!({"resourceType": "Patient"})
            ),
        ),
        (
            "/files/o1.ndjson",
            ndjson(&[json!({"resourceType": "Observation", "id": "o"})]),
        ),
    ] {
        server.expect(
            Expectation::matching(all_of![request::method_path("GET", path), bearer()])
                .times(1)
                .respond_with(status_code(200).body(body)),
        );
    }
    expect_delete(&server, "/status/1");

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let started = Instant::now();
    let a = export_bulk(&server, &out, &["Patient", "Observation"])
        .args(["--token", "tok"])
        .assert()
        .success();
    assert!(started.elapsed() >= Duration::from_secs(1), "Retry-After");
    let text = stdout(&a);
    assert!(
        text.contains("bulk export: Exported 2 of 3 types"),
        "{text}"
    );
    assert!(text.contains("Patient: 2 resources, 2 files"), "{text}");
    assert!(text.contains("Observation: 1 resources, 1 files"), "{text}");
    assert!(text.contains("bulk_line_skipped: 1"), "{text}");
    assert_eq!(ids(&out.join("Patient.ndjson")), ["a", "b"]);
    assert_eq!(ids(&out.join("Observation.ndjson")), ["o"]);

    let s = state(&out);
    assert_eq!(s["types"]["Patient"]["watermark"], TRANSACTION_TIME);
    assert_eq!(s["types"]["Patient"]["count"], 2);
    assert_eq!(s["types"]["Observation"]["watermark"], TRANSACTION_TIME);
    assert!(s.get("bulk").is_none(), "{s}");
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [".export-state.json", "Observation.ndjson", "Patient.ndjson"]
    );
}

/// Output files answered after `delay`, counting the requests in flight.
#[derive(Clone)]
struct SlowFiles {
    delay: Duration,
    bodies: std::sync::Arc<std::collections::HashMap<String, (u16, String)>>,
    in_flight: std::sync::Arc<AtomicUsize>,
    most: std::sync::Arc<AtomicUsize>,
}

impl Responder for SlowFiles {
    fn respond<'a>(
        &mut self,
        req: &'a httptest::http::Request<httptest::bytes::Bytes>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = httptest::http::Response<httptest::bytes::Bytes>> + Send + 'a>,
    > {
        let me = self.clone();
        let path = req.uri().path().to_string();
        Box::pin(async move {
            let now = me.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            me.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(me.delay).await;
            let (status, body) = me.bodies[&path].clone();
            me.in_flight.fetch_sub(1, Ordering::SeqCst);
            httptest::http::Response::builder()
                .status(status)
                .body(httptest::bytes::Bytes::from(body))
                .unwrap()
        })
    }
}

/// An export of Patient and Observation whose nine output files, the two
/// types interleaved, each take `delay` to answer; `failing` answers 404.
fn slow_export(server: &Server, delay: Duration, failing: Option<&str>) -> SlowFiles {
    expect_kick_off(
        server,
        "Observation,Patient",
        None,
        &server.url_str("/status/1"),
    );
    let mut output = Vec::new();
    let mut bodies = std::collections::HashMap::new();
    for i in 0..9 {
        let t = if i % 3 == 2 { "Observation" } else { "Patient" };
        let path = format!("/files/{i}.ndjson");
        let lines: Vec<Value> = (0..3)
            .map(|k| json!({"resourceType": t, "id": format!("{t}-{i}-{k}"), "n": "é\\\"x"}))
            .collect();
        let status = if failing == Some(path.as_str()) {
            404
        } else {
            200
        };
        bodies.insert(path.clone(), (status, ndjson(&lines)));
        output.push((t, path));
    }
    let pairs: Vec<(&str, &str)> = output.iter().map(|(t, p)| (*t, p.as_str())).collect();
    let m = manifest(server, &pairs, false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/1"))
            .times(1)
            .respond_with(done(&m)),
    );
    let files = SlowFiles {
        delay,
        bodies: std::sync::Arc::new(bodies),
        in_flight: Default::default(),
        most: Default::default(),
    };
    server.expect(
        Expectation::matching(request::method_path("GET", matches("^/files/")))
            .times(..)
            .respond_with(files.clone()),
    );
    files
}

#[test]
fn output_files_download_side_by_side_and_join_in_manifest_order() {
    let server = Server::run();
    let files = slow_export(&server, Duration::from_millis(300), None);
    expect_delete(&server, "/status/1");
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let a = export_bulk(&server, &out, &["Patient", "Observation"])
        .args(["--concurrency", "3"])
        .assert()
        .success();
    let text = stdout(&a);
    assert!(text.contains("Patient: 18 resources, 6 files"), "{text}");
    assert!(text.contains("Observation: 9 resources, 3 files"), "{text}");
    assert_eq!(
        files.most.load(Ordering::SeqCst),
        3,
        "--concurrency at once"
    );
    // Each type's lines in manifest order, as a download a file at a time
    // writes them.
    let types: [(&str, &[usize]); 2] = [
        ("Patient", &[0, 1, 3, 4, 6, 7]),
        ("Observation", &[2, 5, 8]),
    ];
    for (t, want) in types {
        let want: String = want
            .iter()
            .map(|i| files.bodies[&format!("/files/{i}.ndjson")].1.clone())
            .collect();
        assert_eq!(
            std::fs::read_to_string(out.join(format!("{t}.ndjson"))).unwrap(),
            want,
            "{t}"
        );
    }
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [".export-state.json", "Observation.ndjson", "Patient.ndjson"]
    );
}

#[test]
fn a_failed_output_file_keeps_no_part_and_advances_nothing() {
    let server = Server::run();
    slow_export(&server, Duration::from_millis(50), Some("/files/4.ndjson"));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("Patient.ndjson"),
        ndjson(&[patient("old", "2026-01-01T00:00:00Z")]),
    )
    .unwrap();
    let a = export_bulk(&server, &out, &["Patient", "Observation"])
        .args(["--concurrency", "4"])
        .assert()
        .failure();
    assert!(stderr(&a).contains("/files/4.ndjson"), "{}", stderr(&a));
    assert_eq!(ids(&out.join("Patient.ndjson")), ["old"]);
    assert!(!out.join("Observation.ndjson").exists());
    let s = state(&out);
    assert!(s["types"].as_object().is_none_or(|t| t.is_empty()), "{s}");
    assert!(
        s.get("bulk").is_some(),
        "the export is picked up next run: {s}"
    );
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".export-state.json", "Patient.ndjson"]);
}

/// `parts` as one gzip body of a member each. Concatenated members are one
/// valid gzip body (RFC 1952 section 2.2).
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
fn every_member_of_a_multi_member_gzip_manifest_and_file_is_read() {
    // A decoder that stops after the first member saved one Patient of
    // three, reported no issue and moved the watermark on.
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/g"));
    let m = manifest(&server, &[("Patient", "/files/g.ndjson")], false).to_string();
    let (head, tail) = m.split_at(m.len() / 2);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/status/g"),
            request::headers(contains(("accept-encoding", matches("gzip")))),
        ])
        .times(1)
        .respond_with(
            status_code(200)
                .insert_header("Content-Type", "application/json")
                .insert_header("Content-Encoding", "gzip")
                .body(gzip_members(&[head.as_bytes(), tail.as_bytes()])),
        ),
    );
    let lines: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|id| format!("{}\n", patient(id, "2026-10-01T00:00:00Z")))
        .collect();
    let parts: Vec<&[u8]> = lines.iter().map(|l| l.as_bytes()).collect();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/files/g.ndjson"),
            request::headers(contains(("accept-encoding", matches("gzip")))),
        ])
        .times(1)
        .respond_with(
            status_code(200)
                .insert_header("Content-Encoding", "gzip")
                .body(gzip_members(&parts)),
        ),
    );
    expect_delete(&server, "/status/g");

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let a = export_bulk(&server, &out, &["Patient"]).assert().success();
    let text = stdout(&a);
    assert!(text.contains("Patient: 3 resources, 1 files"), "{text}");
    assert_eq!(ids(&out.join("Patient.ndjson")), ["a", "b", "c"]);
    let s = state(&out);
    assert_eq!(s["types"]["Patient"]["count"], 3);
    assert_eq!(s["types"]["Patient"]["watermark"], TRANSACTION_TIME);
}

#[test]
fn without_retry_after_the_status_is_polled_with_a_backoff() {
    let server = Server::run();
    // A relative Content-Location is resolved against the kick-off URL.
    expect_kick_off(&server, "Patient", None, "/status/2");
    let m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/2"))
            .times(3)
            .respond_with(cycle![status_code(202), status_code(202), done(&m)]),
    );
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[patient("a", "2026-10-01T00:00:00Z")]),
    );
    expect_delete(&server, "/status/2");
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .success();
    // One second, then two.
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["a"]);
}

#[test]
fn a_429_from_the_status_url_is_waited_out() {
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/3"));
    let m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/3"))
            .times(2)
            .respond_with(cycle![
                status_code(429).insert_header("Retry-After", "1"),
                done(&m)
            ]),
    );
    // requiresAccessToken is false: the file must not get the token.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/files/p.ndjson"),
            request::headers(not(contains(key("authorization")))),
        ])
        .times(1)
        .respond_with(status_code(200).body(ndjson(&[patient("a", "2026-10-01T00:00:00Z")]))),
    );
    expect_delete(&server, "/status/3");
    let dir = tempfile::tempdir().unwrap();
    export_bulk(&server, dir.path(), &["Patient"])
        .args(["--token", "tok"])
        .assert()
        .success();
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["a"]);
}

/// OUT with Patient a, b and gone, and a watermark of `at`.
fn previous_patients(out: &Path, server: &Server, at: &str) {
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(
        out.join("Patient.ndjson"),
        ndjson(&[
            patient("a", "2026-10-01T00:00:00Z"),
            patient("b", "2026-10-01T00:00:00Z"),
            patient("gone", "2026-10-01T00:00:00Z"),
        ]),
    )
    .unwrap();
    std::fs::write(
        out.join(".export-state.json"),
        json!({"server": server.url_str("/fhir"), "types": {"Patient": {
            "watermark": at, "count": 3,
            "completed_at": at, "fhir_pipes_version": "0.4.0"}}})
        .to_string(),
    )
    .unwrap();
}

fn count_reply(server: &Server, n: u64) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Patient"),
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

#[test]
fn an_incremental_bulk_export_merges_by_id_and_removes_deleted_resources() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let since = "2026-10-10T00:00:00Z";
    previous_patients(&out, &server, since);

    expect_kick_off(
        &server,
        "Patient",
        Some(since),
        &server.url_str("/status/4"),
    );
    let mut m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    m["deleted"] = json!([{"type": "Bundle", "url": server.url_str("/files/deleted.ndjson")}]);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/4"))
            .times(1)
            .respond_with(done(&m)),
    );
    // b changed, c is new.
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[
            json!({"resourceType": "Patient", "id": "b", "active": false, "meta": {"lastUpdated": "2026-10-12T00:00:00Z"}}),
            patient("c", "2026-10-12T00:00:00Z"),
        ]),
    );
    // gone was deleted; c was deleted and created again, so it stays; the
    // Observation is not a type this export asked for.
    let deleted = json!({"resourceType": "Bundle", "type": "transaction", "entry": [
        {"request": {"method": "DELETE", "url": "Patient/gone"}},
        {"request": {"method": "DELETE", "url": "Patient/c"}},
        {"request": {"method": "DELETE", "url": "Observation/a"}},
    ]});
    server.expect(
        Expectation::matching(request::method_path("GET", "/files/deleted.ndjson"))
            .times(1)
            .respond_with(status_code(200).body(format!("{deleted}\n"))),
    );
    count_reply(&server, 3);
    expect_delete(&server, "/status/4");

    let a = export_bulk(&server, &out, &["Patient"])
        .arg("--incremental")
        .assert()
        .success();
    let text = stdout(&a);
    assert!(
        text.contains(&format!(
            "Patient: 1 new, 1 updated or fetched again since {since}, 1 deleted, 3 in all, 1 files"
        )),
        "{text}"
    );
    assert_eq!(ids(&out.join("Patient.ndjson")), ["a", "b", "c"]);
    let rows = read_ndjson(&out.join("Patient.ndjson"));
    let b = rows.iter().find(|r| r["id"] == "b").unwrap();
    assert_eq!(b["active"], false);
    let s = state(&out);
    assert_eq!(s["types"]["Patient"]["watermark"], TRANSACTION_TIME);
    assert_eq!(s["types"]["Patient"]["count"], 3);
}

#[test]
fn a_count_mismatch_after_a_bulk_merge_exports_the_type_in_full() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");
    let since = "2026-10-10T00:00:00Z";
    previous_patients(&out, &server, since);

    // Nothing changed and no deleted list, but the server has two Patients.
    expect_kick_off(
        &server,
        "Patient",
        Some(since),
        &server.url_str("/status/5"),
    );
    let changes = manifest(&server, &[], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/5"))
            .times(1)
            .respond_with(done(&changes)),
    );
    count_reply(&server, 2);
    expect_delete(&server, "/status/5");
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/6"));
    let all = manifest(&server, &[("Patient", "/files/all.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/6"))
            .times(1)
            .respond_with(done(&all)),
    );
    expect_file(
        &server,
        "/files/all.ndjson",
        ndjson(&[
            patient("a", "2026-10-01T00:00:00Z"),
            patient("b", "2026-10-01T00:00:00Z"),
        ]),
    );
    expect_delete(&server, "/status/6");

    let a = export_bulk(&server, &out, &["Patient"])
        .arg("--incremental")
        .assert()
        .success();
    let text = stdout(&a);
    assert!(text.contains("downloading in full"), "{text}");
    assert!(text.contains("Patient: 2 resources, 1 files"), "{text}");
    assert_eq!(ids(&out.join("Patient.ndjson")), ["a", "b"]);
    assert_eq!(state(&out)["types"]["Patient"]["count"], 2);
}

#[test]
fn an_error_manifest_fails_the_run_with_its_text() {
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/7"));
    let mut m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    m["error"] =
        json!([{"type": "OperationOutcome", "url": server.url_str("/files/errors.ndjson")}]);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/7"))
            .times(1)
            .respond_with(done(&m)),
    );
    let outcomes = [
        json!({"resourceType": "OperationOutcome", "issue": [
            {"severity": "error", "code": "processing", "diagnostics": "Patient/9 could not be read"}]}),
        json!({"resourceType": "OperationOutcome", "issue": [
            {"severity": "information", "code": "informational", "diagnostics": "took a while"}]}),
    ];
    server.expect(
        Expectation::matching(request::method_path("GET", "/files/errors.ndjson"))
            .times(1)
            .respond_with(status_code(200).body(ndjson(&outcomes))),
    );
    expect_delete(&server, "/status/7");
    let dir = tempfile::tempdir().unwrap();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(
        err.contains(
            "the bulk export reported 1 error(s): error processing: Patient/9 could not be read"
        ),
        "{err}"
    );
    assert!(!dir.path().join("Patient.ndjson").exists());
    assert!(state(dir.path()).get("bulk").is_none());
}

#[test]
fn a_failed_kick_off_or_export_fails_with_the_operation_outcome() {
    let outcome = |text: &str| {
        json!({"resourceType": "OperationOutcome", "issue": [
            {"severity": "error", "code": "not-supported", "diagnostics": text}]})
        .to_string()
    };
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/$export"))
            .times(1)
            .respond_with(status_code(400).body(outcome("bulk export is not enabled"))),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(err.contains("HTTP 400"), "{err}");
    assert!(err.contains("bulk export is not enabled"), "{err}");

    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/8"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/8"))
            .times(1)
            .respond_with(status_code(500).body(outcome("the job ran out of disk"))),
    );
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(
        err.contains(
            "the bulk export failed: HTTP 500: error not-supported: the job ran out of disk"
        ),
        "{err}"
    );
    assert!(state(dir.path()).get("bulk").is_none());
}

#[test]
fn an_export_that_does_not_finish_in_time_is_cancelled() {
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/9"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/9"))
            .times(2..)
            .respond_with(status_code(202).insert_header("Retry-After", "1")),
    );
    expect_delete(&server, "/status/9");
    let dir = tempfile::tempdir().unwrap();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .args(["--bulk-timeout", "2"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(
        err.contains("did not finish within --bulk-timeout (2 s); cancelled it"),
        "{err}"
    );
    assert!(state(dir.path()).get("bulk").is_none());
}

#[test]
fn the_status_url_is_saved_before_polling_and_polled_again_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("raw");

    // First run: the status URL cannot be reached, so the run fails and
    // keeps it.
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, "http://127.0.0.1:9/status/10");
    export_bulk(&server, &out, &["Patient"])
        .args(["--retries", "1"])
        .assert()
        .failure();
    let saved = state(&out);
    assert_eq!(saved["bulk"]["status_url"], "http://127.0.0.1:9/status/10");
    assert_eq!(saved["bulk"]["types"], json!(["Patient"]));
    drop(server);

    // Second run: no kick-off, the saved status URL is polled. It is moved
    // to this server to stand for the server being back.
    let server = Server::run();
    let mut moved = saved.clone();
    moved["server"] = json!(server.url_str("/fhir"));
    moved["bulk"]["status_url"] = json!(server.url_str("/status/10"));
    std::fs::write(out.join(".export-state.json"), moved.to_string()).unwrap();
    let m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/10"))
            .times(1)
            .respond_with(done(&m)),
    );
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[patient("a", "2026-10-01T00:00:00Z")]),
    );
    expect_delete(&server, "/status/10");
    let a = export_bulk(&server, &out, &["Patient"]).assert().success();
    assert!(
        stdout(&a).contains("Patient: resuming the bulk export (in full)"),
        "{}",
        stdout(&a)
    );
    assert_eq!(ids(&out.join("Patient.ndjson")), ["a"]);
    assert!(state(&out).get("bulk").is_none());
}

/// A state file with a saved export of `types` at `status`.
fn saved_export(out: &Path, server: &Server, types: &[&str], status: &str) {
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(
        out.join(".export-state.json"),
        json!({"server": server.url_str("/fhir"), "types": {}, "bulk": {
            "status_url": status, "types": types, "started_at": "2026-10-15T07:00:00Z"}})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn a_saved_export_the_server_no_longer_has_is_started_again() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    saved_export(
        dir.path(),
        &server,
        &["Patient"],
        &server.url_str("/status/old"),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/old"))
            .times(1)
            .respond_with(status_code(404)),
    );
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/new"));
    let m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/new"))
            .times(1)
            .respond_with(done(&m)),
    );
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[patient("a", "2026-10-01T00:00:00Z")]),
    );
    expect_delete(&server, "/status/new");
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .success();
    assert!(
        stdout(&a).contains("no longer has the saved bulk export"),
        "{}",
        stdout(&a)
    );
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["a"]);
}

#[test]
fn a_saved_export_of_other_types_is_cancelled() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    saved_export(
        dir.path(),
        &server,
        &["Observation"],
        &server.url_str("/status/other"),
    );
    expect_delete(&server, "/status/other");
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/11"));
    let m = manifest(&server, &[("Patient", "/files/p.ndjson")], false);
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/11"))
            .times(1)
            .respond_with(done(&m)),
    );
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[patient("a", "2026-10-01T00:00:00Z")]),
    );
    expect_delete(&server, "/status/11");
    export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .success();
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["a"]);
}

#[test]
fn bulk_timeout_needs_bulk() {
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args([
            "export",
            "--server",
            "http://127.0.0.1:9/fhir",
            "--type",
            "Patient",
            "--bulk-timeout",
            "5",
        ])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .code(2);
    assert!(stderr(&a).contains("--bulk"), "{}", stderr(&a));
}

/// A status request that hangs past --bulk-timeout ends as a timeout, which
/// cancels the export, not as a transport error that leaves it saved.
#[test]
fn a_status_request_that_hangs_past_the_deadline_cancels_the_export() {
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/7"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/7"))
            .times(1..)
            .respond_with(delay_and_then(
                Duration::from_secs(5),
                status_code(202).insert_header("Retry-After", "1"),
            )),
    );
    expect_delete(&server, "/status/7");
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .args(["--bulk-timeout", "1", "--timeout", "30"])
        .assert()
        .failure();
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let err = stderr(&a);
    assert!(err.contains("cancelled it"), "{err}");
    assert!(state(dir.path()).get("bulk").is_none());
}

/// An OperationOutcome longer than the shown error text is still read whole:
/// a transient one is waited out, a permanent one fails with a cut text.
#[test]
fn long_operation_outcomes_are_classified_whole() {
    let outcome = |code: &str| {
        json!({"resourceType": "OperationOutcome", "issue": [
            {"severity": "error", "code": code, "diagnostics": "x".repeat(2000)}]})
        .to_string()
    };
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/6"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/6"))
            .times(2)
            .respond_with(cycle![
                status_code(500)
                    .insert_header("Retry-After", "0")
                    .body(outcome("transient")),
                done(&manifest(&server, &[("Patient", "/files/p.ndjson")], false)),
            ]),
    );
    expect_file(
        &server,
        "/files/p.ndjson",
        ndjson(&[patient("p1", "2026-10-15T07:00:00Z")]),
    );
    expect_delete(&server, "/status/6");
    let dir = tempfile::tempdir().unwrap();
    export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .success();
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["p1"]);

    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/5"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/5"))
            .times(1)
            .respond_with(status_code(400).body(outcome("processing"))),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(err.contains("HTTP 400: error processing: xxx"), "{err}");
    assert!(err.len() < 1500, "{} chars", err.len());
    assert!(state(dir.path()).get("bulk").is_none());
}

/// The bulk deadline only cuts a status request short; --timeout still
/// bounds each one when the deadline is further away.
#[test]
fn a_status_request_keeps_the_request_timeout() {
    let server = Server::run();
    expect_kick_off(&server, "Patient", None, &server.url_str("/status/4"));
    server.expect(
        Expectation::matching(request::method_path("GET", "/status/4"))
            .times(1..)
            .respond_with(delay_and_then(
                Duration::from_secs(3),
                done(&manifest(&server, &[], false)),
            )),
    );
    // With the deadline replacing --timeout, the 3 s answer would arrive
    // and the run succeed.
    let dir = tempfile::tempdir().unwrap();
    let a = export_bulk(&server, dir.path(), &["Patient"])
        .args(["--timeout", "1", "--retries", "1", "--bulk-timeout", "30"])
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(err.contains("polling"), "{err}");
}
