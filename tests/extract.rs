//! Binary-level tests for `fhir-pipes extract` against a fake FHIR server.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::{all_of, cycle, matchers::*, responders::*, Expectation, Server};
use serde_json::{json, Value};

const BOUNDARY_EXT: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson";
const GEOJSON: &str = r#"{"type":"Point","coordinates":[3.25,6.25]}"#;

fn loc(id: &str, updated: &str, boundary: Option<&str>) -> Value {
    let mut r = json!({
        "resourceType": "Location",
        "id": id,
        "meta": {"lastUpdated": updated},
        "status": "active",
        "name": id,
        "position": {"longitude": 3.25, "latitude": 6.25},
        "type": [{"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/location-physical-type",
            "code": "facility"
        }]}],
    });
    if let Some(url) = boundary {
        r["extension"] = json!([{
            "url": BOUNDARY_EXT,
            "valueAttachment": {"contentType": "application/geo+json", "url": url},
        }]);
    }
    r
}

fn bundle(entries: Vec<Value>, next: Option<&str>) -> String {
    let mut b = json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "total": entries.len(),
        "entry": entries
            .into_iter()
            .map(|r| json!({"resource": r}))
            .collect::<Vec<_>>(),
    });
    if let Some(n) = next {
        b["link"] = json!([{"relation": "next", "url": n}]);
    }
    b.to_string()
}

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

fn extract_at(base: &str, snapshot: &Path, extra: &[&str]) -> Assert {
    fhir_pipes()
        .args(["extract", "--server", base, "--snapshot"])
        .arg(snapshot)
        .args(extra)
        .assert()
}

fn extract(server: &Server, snapshot: &Path, extra: &[&str]) -> Assert {
    extract_at(&server.url("/fhir").to_string(), snapshot, extra)
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn state(snapshot: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(snapshot.join("state.json")).unwrap()).unwrap()
}

fn lines(snapshot: &Path) -> Vec<Value> {
    std::fs::read_to_string(snapshot.join("locations.ndjson"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn attachment(resource: &Value) -> &Value {
    &resource["extension"][0]["valueAttachment"]
}

fn inlined_bytes(resource: &Value) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(attachment(resource)["data"].as_str().unwrap())
        .unwrap()
}

type Req = httptest::http::Request<httptest::bytes::Bytes>;

/// `GET <base>/Location` with no `_lastUpdated` parameter: a full extract.
fn full_search() -> impl Matcher<Req> {
    all_of![
        request::method_path("GET", "/fhir/Location"),
        request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        request::query(url_decoded(not(contains(key("_summary"))))),
    ]
}

/// Answers the `_summary=count` cross-check an incremental run makes, for
/// both resource types, any number of times.
fn expect_counts(server: &Server, locations: usize, organizations: usize) {
    for (path, total) in [
        ("/fhir/Location", locations),
        ("/fhir/Organization", organizations),
    ] {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", path),
                request::query(url_decoded(contains(("_summary", "count")))),
            ])
            .times(0..)
            .respond_with(ok(
                json!({"resourceType": "Bundle", "type": "searchset", "total": total}).to_string(),
            )),
        );
    }
}

/// `GET <base>/Location?_lastUpdated=ge<since>`: an incremental extract.
fn since_search(since: &str) -> impl Matcher<Req> {
    all_of![
        request::method_path("GET", "/fhir/Location"),
        request::query(url_decoded(contains((
            "_lastUpdated",
            format!("ge{since}")
        )))),
    ]
}

/// `GET <base>/Organization?_lastUpdated=ge<since>`.
fn org_since_search(since: &str) -> impl Matcher<Req> {
    all_of![
        request::method_path("GET", "/fhir/Organization"),
        request::query(url_decoded(contains((
            "_lastUpdated",
            format!("ge{since}")
        )))),
    ]
}

/// Every extract pages Organizations after Locations. Tests about
/// Locations answer that search with an empty bundle, any number of times.
fn expect_no_organizations(server: &Server) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            request::query(url_decoded(not(contains(key("_summary"))))),
        ])
        .times(0..)
        .respond_with(ok(bundle(vec![], None))),
    );
}

fn org(id: &str, updated: &str, name: &str) -> Value {
    json!({"resourceType": "Organization", "id": id, "active": true, "name": name,
           "meta": {"lastUpdated": updated}})
}

fn org_lines(snapshot: &Path) -> Vec<Value> {
    std::fs::read_to_string(snapshot.join("organizations.ndjson"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ok(body: String) -> ResponseBuilder<String> {
    status_code(200)
        .insert_header("content-type", "application/fhir+json")
        .body(body)
}

#[test]
fn full_extract_writes_snapshot_state_and_inlines_boundaries() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    loc("a", "2026-01-01T00:00:00Z", None),
                    loc("b", "2026-01-02T00:00:00Z", Some(&burl)),
                ],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    // A trailing slash on --server must not survive into state.json.
    let assert = extract_at(&format!("{}/", server.url("/fhir")), snap.path(), &[]);
    let assert = assert.success();

    let rows = lines(snap.path());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "a");
    assert_eq!(rows[1]["id"], "b");
    assert!(attachment(&rows[1])["data"].is_string());
    assert!(attachment(&rows[1])["url"].is_null());
    assert_eq!(inlined_bytes(&rows[1]), GEOJSON.as_bytes());
    assert_eq!(attachment(&rows[1])["contentType"], "application/geo+json");

    let st = state(snap.path());
    assert_eq!(st["server"], server.url("/fhir").to_string());
    assert_eq!(st["watermark"], "2026-01-02T00:00:00Z");
    assert_eq!(st["count"], 2);
    assert_eq!(st["fhir_pipes_version"], env!("CARGO_PKG_VERSION"));
    assert!(st["completed_at"].as_str().unwrap().ends_with('Z'));

    let cached: Vec<_> = std::fs::read_dir(snap.path().join("boundaries"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(cached.len(), 2, "{cached:?}");

    let err = stderr(&assert);
    assert!(
        err.contains("boundaries: 0 cached, 1 fetched, 0 failed"),
        "{err}"
    );
    let out = stdout(&assert);
    assert!(
        out.contains("snapshot: 2 resources, 2 new, 0 updated"),
        "{out}"
    );

    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    assert!(report["counts"].is_object(), "{report}");
    assert!(!snap.path().join(".incoming.ndjson").exists());
}

#[test]
fn second_run_with_nothing_new_leaves_the_snapshot_byte_identical() {
    let server = Server::run();
    expect_counts(&server, 2, 0);
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    loc("a", "2026-01-01T00:00:00Z", None),
                    loc("b", "2026-01-02T00:00:00Z", Some(&burl)),
                ],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(since_search("2026-01-02T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(vec![], None))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    extract(&server, snap.path(), &[]).success();
    let first = std::fs::read(snap.path().join("locations.ndjson")).unwrap();
    let count = state(snap.path())["count"].clone();

    let second = extract(&server, snap.path(), &[]).success();
    let out = stdout(&second);
    assert!(out.contains("0 new, 0 updated"), "{out}");
    let err = stderr(&second);
    assert!(
        err.contains("incremental extract since 2026-01-02T00:00:00Z"),
        "{err}"
    );
    assert!(
        err.contains("boundaries: 0 cached, 0 fetched, 0 failed"),
        "{err}"
    );
    assert_eq!(
        std::fs::read(snap.path().join("locations.ndjson")).unwrap(),
        first
    );
    assert_eq!(state(snap.path())["count"], count);
}

#[test]
fn incremental_run_upserts_and_advances_the_watermark() {
    let server = Server::run();
    expect_counts(&server, 3, 0);
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    loc("a", "2026-01-01T00:00:00Z", None),
                    loc("b", "2026-01-02T00:00:00Z", None),
                ],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(since_search("2026-01-02T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    loc("b", "2026-01-03T00:00:00Z", Some(&burl)),
                    loc("c", "2026-01-04T00:00:00Z", None),
                ],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    extract(&server, snap.path(), &[]).success();
    let second = extract(&server, snap.path(), &[]).success();

    let rows = lines(snap.path());
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert_eq!(inlined_bytes(&rows[1]), GEOJSON.as_bytes());
    let out = stdout(&second);
    assert!(out.contains("1 new, 1 updated"), "{out}");
    assert_eq!(state(snap.path())["watermark"], "2026-01-04T00:00:00Z");
    assert_eq!(state(snap.path())["count"], 3);
}

#[test]
fn full_flag_replaces_the_snapshot() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(2)
            .respond_with(cycle![
                ok(bundle(
                    vec![
                        loc("a", "2026-01-01T00:00:00Z", None),
                        loc("b", "2026-01-02T00:00:00Z", None),
                    ],
                    None,
                )),
                ok(bundle(vec![loc("a", "2026-01-01T00:00:00Z", None)], None,)),
            ]),
    );

    extract(&server, snap.path(), &[]).success();
    assert_eq!(lines(snap.path()).len(), 2);

    let second = extract(&server, snap.path(), &["--full"]).success();
    let rows = lines(snap.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "a");
    let err = stderr(&second);
    assert!(err.contains("full extract"), "{err}");
    assert_eq!(state(snap.path())["count"], 1);
}

#[test]
fn snapshot_without_state_is_treated_as_full_unless_since() {
    let server = Server::run();
    expect_counts(&server, 1, 0);
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    std::fs::write(
        snap.path().join("locations.ndjson"),
        format!("{}\n", loc("old", "2026-01-01T00:00:00Z", None)),
    )
    .unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-02T00:00:00Z", None)],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(since_search("2026-01-01T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(vec![], None))),
    );

    // No state.json: full, so `old` is dropped.
    extract(&server, snap.path(), &[]).success();
    let rows = lines(snap.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "a");

    // --since overrides the stored watermark (2026-01-02) with an earlier one.
    extract(&server, snap.path(), &["--since", "2026-01-01T00:00:00Z"]).success();
}

#[test]
fn server_mismatch_is_a_usage_error_and_full_repoints() {
    let a = Server::run();
    let b = Server::run();
    expect_no_organizations(&a);
    expect_no_organizations(&b);
    let snap = tempfile::tempdir().unwrap();
    a.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-01T00:00:00Z", None)],
                None,
            ))),
    );
    b.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("z", "2026-01-05T00:00:00Z", None)],
                None,
            ))),
    );

    extract(&a, snap.path(), &[]).success();
    let mismatch = extract(&b, snap.path(), &[]).failure().code(2);
    let err = stderr(&mismatch);
    assert!(err.contains("--full"), "{err}");
    assert_eq!(state(snap.path())["server"], a.url("/fhir").to_string());

    extract(&b, snap.path(), &["--full"]).success();
    assert_eq!(state(snap.path())["server"], b.url("/fhir").to_string());
    assert_eq!(lines(snap.path())[0]["id"], "z");
}

#[test]
fn circuit_breaker_aborts_and_keeps_the_old_snapshot() {
    let good = Server::run();
    let bad = Server::run();
    expect_no_organizations(&good);
    expect_no_organizations(&bad);
    let snap = tempfile::tempdir().unwrap();
    good.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-01T00:00:00Z", None)],
                None,
            ))),
    );
    extract(&good, snap.path(), &[]).success();
    let before = std::fs::read(snap.path().join("locations.ndjson")).unwrap();
    let state_before = std::fs::read(snap.path().join("state.json")).unwrap();

    let broken: Vec<Value> = (0..8)
        .map(|i| {
            loc(
                &format!("x{i}"),
                "2026-02-01T00:00:00Z",
                Some(&bad.url(&format!("/b/x{i}.geojson")).to_string()),
            )
        })
        .collect();
    bad.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(broken, None))),
    );
    bad.expect(
        Expectation::matching(request::path(matches("^/b/")))
            .times(0..)
            .respond_with(status_code(500).body("boom")),
    );

    let assert = extract(
        &bad,
        snap.path(),
        &[
            "--full",
            "--retries",
            "1",
            "--max-consecutive-failures",
            "3",
        ],
    )
    .failure()
    .code(1);
    let err = stderr(&assert);
    assert!(err.contains("consecutive"), "{err}");

    assert_eq!(
        std::fs::read(snap.path().join("locations.ndjson")).unwrap(),
        before
    );
    assert_eq!(lines(snap.path()).len(), 1);
    assert_eq!(
        std::fs::read(snap.path().join("state.json")).unwrap(),
        state_before
    );
}

#[test]
fn conflicting_flags_and_a_bad_since_are_usage_errors() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();

    let a = extract(&server, snap.path(), &["--no-cache", "--refresh"])
        .failure()
        .code(2);
    assert!(stderr(&a).contains("--no-cache"), "{}", stderr(&a));

    let b = extract(
        &server,
        snap.path(),
        &["--no-cache", "--cache-dir", "/tmp/x"],
    )
    .failure()
    .code(2);
    assert!(stderr(&b).contains("--no-cache"), "{}", stderr(&b));

    let c = extract(&server, snap.path(), &["--since", "nope"])
        .failure()
        .code(2);
    assert!(stderr(&c).contains("--since"), "{}", stderr(&c));

    let d = extract(&server, snap.path(), &["--concurrency", "0"])
        .failure()
        .code(2);
    assert!(stderr(&d).contains("--concurrency"), "{}", stderr(&d));

    let e = extract(
        &server,
        snap.path(),
        &["--full", "--since", "2026-01-01T00:00:00Z"],
    )
    .failure()
    .code(2);
    assert!(stderr(&e).contains("--since"), "{}", stderr(&e));

    // A --snapshot that cannot be a directory is the operator's mistake.
    let file = snap.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let f = extract_at(&server.url("/fhir").to_string(), &file, &[])
        .failure()
        .code(2);
    assert!(stderr(&f).contains("--snapshot"), "{}", stderr(&f));
}

#[test]
fn extract_report_counts_skipped_entries() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    json!({"resourceType": "Location"}),
                    loc("a", "2026-01-01T00:00:00Z", None),
                ],
                None,
            ))),
    );

    extract(&server, snap.path(), &[]).success();
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["counts"]["page_resource_skipped"], 1);
    assert_eq!(lines(snap.path()).len(), 1);
}

#[test]
fn no_cache_still_inlines() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    extract(&server, snap.path(), &["--no-cache"]).success();
    let rows = lines(snap.path());
    assert_eq!(inlined_bytes(&rows[0]), GEOJSON.as_bytes());
    assert!(!snap.path().join("boundaries").exists());
}

#[test]
fn second_run_hits_the_cache() {
    let server = Server::run();
    expect_counts(&server, 1, 0);
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    let rows = vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))];
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(rows.clone(), None))),
    );
    // The `ge` comparison refetches the row that set the watermark.
    server.expect(
        Expectation::matching(since_search("2026-01-02T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(rows, None))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    extract(&server, snap.path(), &[]).success();
    let second = extract(&server, snap.path(), &[]).success();
    let err = stderr(&second);
    assert!(
        err.contains("boundaries: 1 cached, 0 fetched, 0 failed"),
        "{err}"
    );
    let out = stdout(&second);
    assert!(out.contains("1 resources, 0 new, 1 updated"), "{out}");
    assert_eq!(inlined_bytes(&lines(snap.path())[0]), GEOJSON.as_bytes());
}

#[test]
fn a_401_on_the_search_keeps_the_old_snapshot() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-01T00:00:00Z", None)],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(since_search("2026-01-01T00:00:00Z"))
            .times(1)
            .respond_with(status_code(401).body("no token for you")),
    );

    extract(&server, snap.path(), &[]).success();
    let before = std::fs::read(snap.path().join("locations.ndjson")).unwrap();
    let state_before = std::fs::read(snap.path().join("state.json")).unwrap();

    let assert = extract(&server, snap.path(), &[]).failure().code(1);
    assert!(stderr(&assert).contains("401"), "{}", stderr(&assert));
    assert_eq!(
        std::fs::read(snap.path().join("locations.ndjson")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(snap.path().join("state.json")).unwrap(),
        state_before
    );
    assert!(!snap.path().join(".incoming.ndjson").exists());
}

#[test]
fn a_non_bundle_search_response_keeps_the_old_snapshot() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-01T00:00:00Z", None)],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(since_search("2026-01-01T00:00:00Z"))
            .times(1)
            .respond_with(ok(json!({"resourceType": "OperationOutcome"}).to_string())),
    );

    extract(&server, snap.path(), &[]).success();
    let before = std::fs::read(snap.path().join("locations.ndjson")).unwrap();
    let state_before = std::fs::read(snap.path().join("state.json")).unwrap();

    let assert = extract(&server, snap.path(), &[]).failure().code(1);
    assert!(
        stderr(&assert).contains("expected a Bundle"),
        "{}",
        stderr(&assert)
    );
    assert_eq!(
        std::fs::read(snap.path().join("locations.ndjson")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(snap.path().join("state.json")).unwrap(),
        state_before
    );
    assert!(!snap.path().join(".incoming.ndjson").exists());
}

#[test]
fn cache_dir_is_shared_between_snapshots() {
    let server = Server::run();
    expect_no_organizations(&server);
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(2)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(200).body(GEOJSON)),
    );

    extract(&server, a.path(), &[]).success();
    let shared = a.path().join("boundaries");
    let second = extract(
        &server,
        b.path(),
        &["--cache-dir", shared.to_str().unwrap()],
    )
    .success();
    let err = stderr(&second);
    assert!(
        err.contains("boundaries: 1 cached, 0 fetched, 0 failed"),
        "{err}"
    );
    assert_eq!(inlined_bytes(&lines(b.path())[0]), GEOJSON.as_bytes());
    assert!(!b.path().join("boundaries").exists());
}

#[test]
fn a_python_written_cache_entry_is_honoured() {
    use sha2::{Digest, Sha256};
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    // The boundary endpoint must never be touched.
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(0)
            .respond_with(status_code(500)),
    );

    let dir = snap.path().join("boundaries");
    std::fs::create_dir_all(&dir).unwrap();
    let key = hex(&Sha256::digest(burl.as_bytes()));
    std::fs::write(dir.join(format!("{key}.bin")), GEOJSON).unwrap();
    std::fs::write(
        dir.join(format!("{key}.meta.json")),
        serde_json::to_string(&json!({
            "url": burl,
            "fetched_at": "2026-01-01T00:00:00Z",
            "sha256": hex(&Sha256::digest(GEOJSON.as_bytes())),
        }))
        .unwrap(),
    )
    .unwrap();

    let assert = extract(&server, snap.path(), &[]).success();
    let err = stderr(&assert);
    assert!(
        err.contains("boundaries: 1 cached, 0 fetched, 0 failed"),
        "{err}"
    );
    assert_eq!(inlined_bytes(&lines(snap.path())[0]), GEOJSON.as_bytes());
}

#[test]
fn refresh_with_a_failing_refetch_reports_stale_cache() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(2)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(2)
            .respond_with(cycle![
                status_code(200).body(GEOJSON),
                status_code(500).body("boom"),
            ]),
    );

    extract(&server, snap.path(), &[]).success();
    extract(
        &server,
        snap.path(),
        &["--full", "--refresh", "--retries", "1"],
    )
    .success();

    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["counts"]["boundary_stale_from_cache"], 1, "{report}");
    assert!(
        report["counts"]["boundary_fetch_failed"].is_null(),
        "{report}"
    );
    // The last known good copy is still inlined.
    assert_eq!(inlined_bytes(&lines(snap.path())[0]), GEOJSON.as_bytes());
}

#[test]
fn a_boundary_404_leaves_the_url_and_reports() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/b.geojson"))
            .times(1)
            .respond_with(status_code(404).body("gone")),
    );

    let assert = extract(&server, snap.path(), &[]).success();
    let err = stderr(&assert);
    assert!(
        err.contains("boundaries: 0 cached, 0 fetched, 1 failed"),
        "{err}"
    );
    let rows = lines(snap.path());
    assert_eq!(attachment(&rows[0])["url"], burl);
    assert!(attachment(&rows[0])["data"].is_null());
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["counts"]["boundary_fetch_failed"], 1, "{report}");
}

/// Recursively collect every file under `dir` whose name ends in `.parquet`.
fn find_parquet_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(find_parquet_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
            out.push(path);
        }
    }
    out
}

#[test]
fn run_extracts_then_transforms() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let burl = server.url("/b/ng.geojson").to_string();

    // An admin unit "ng" with a polygon boundary served by the server, and a
    // facility "clinic" that is partOf it with a position inside the square.
    let ng = json!({
        "resourceType": "Location",
        "id": "ng",
        "meta": {"lastUpdated": "2026-01-01T00:00:00Z"},
        "status": "active",
        "name": "ng",
        "type": [{"coding": [{"code": "admin-unit"}]}],
        "identifier": [{
            "system": "https://icr.healthcampaigns.org/identifiers/pcode",
            "value": "NG",
        }],
        "extension": [{
            "url": BOUNDARY_EXT,
            "valueAttachment": {"contentType": "application/geo+json", "url": burl},
        }],
    });
    let clinic = json!({
        "resourceType": "Location",
        "id": "clinic",
        "meta": {"lastUpdated": "2026-01-02T00:00:00Z"},
        "status": "active",
        "name": "clinic",
        "type": [{"coding": [{"code": "facility"}]}],
        "partOf": {"reference": "Location/ng"},
        "position": {"longitude": 3.5, "latitude": 6.5},
    });

    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(vec![ng, clinic], None))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/b/ng.geojson"))
            .times(1)
            .respond_with(
                status_code(200)
                    .body(r#"{"type":"Polygon","coordinates":[[[3,6],[4,6],[4,7],[3,7],[3,6]]]}"#),
            ),
    );

    let assert = fhir_pipes()
        .args([
            "run",
            "--server",
            &server.url("/fhir").to_string(),
            "--snapshot",
        ])
        .arg(snap.path())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();

    let parquet_files = find_parquet_files(&out.path().join("locations"));
    assert!(!parquet_files.is_empty(), "{parquet_files:?}");

    let stdout_text = stdout(&assert);
    assert!(
        stdout_text.contains("snapshot: 2 resources"),
        "{stdout_text}"
    );
    assert!(
        stdout_text.contains("Wrote 2 rows across 2 partitions"),
        "{stdout_text}"
    );

    let stderr_text = stderr(&assert);
    assert!(stderr_text.contains("== extract =="), "{stderr_text}");
    assert!(stderr_text.contains("== transform =="), "{stderr_text}");

    assert!(snap.path().join("state.json").exists());
}

#[test]
fn run_rejects_a_bad_out_before_extracting() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let bad_out = out.path().join("not-a-dir");
    std::fs::write(&bad_out, b"nope").unwrap();

    server.expect(
        Expectation::matching(full_search())
            .times(0)
            .respond_with(ok(bundle(vec![], None))),
    );

    fhir_pipes()
        .args([
            "run",
            "--server",
            &server.url("/fhir").to_string(),
            "--snapshot",
        ])
        .arg(snap.path())
        .arg("--out")
        .arg(&bad_out)
        .assert()
        .failure()
        .code(2);
}

#[test]
fn run_skips_transform_when_extract_fails() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();

    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(status_code(401).body("no token for you")),
    );

    let assert = fhir_pipes()
        .args([
            "run",
            "--server",
            &server.url("/fhir").to_string(),
            "--snapshot",
        ])
        .arg(snap.path())
        .arg("--out")
        .arg(out.path())
        .assert()
        .failure()
        .code(1);

    assert!(!out.path().join("locations").exists());
    assert!(stderr(&assert).contains("401"), "{}", stderr(&assert));
}

#[cfg(unix)]
#[test]
fn an_unwritable_snapshot_dir_is_a_usage_error() {
    use std::os::unix::fs::PermissionsExt;

    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    std::fs::set_permissions(snap.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root (and some CI sandboxes) can write through a read-only mode bit;
    // when that's the case here, the premise of this test doesn't hold, so
    // skip rather than fail on an environment we can't control.
    let probe = snap.path().join(".fhir-pipes-write-test-probe");
    let writable = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if writable {
        eprintln!("skipping: the test runner can write through 0o555 (likely running as root)");
        let _ = std::fs::set_permissions(snap.path(), std::fs::Permissions::from_mode(0o755));
        return;
    }

    let assert = extract(&server, snap.path(), &[]).failure().code(2);
    assert!(
        stderr(&assert).contains("--snapshot"),
        "{}",
        stderr(&assert)
    );

    let _ = std::fs::set_permissions(snap.path(), std::fs::Permissions::from_mode(0o755));
}

#[test]
fn organizations_are_paged_after_locations_with_their_own_watermark() {
    let server = Server::run();
    expect_counts(&server, 1, 2);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("a", "2026-01-01T00:00:00Z", None)],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            request::query(url_decoded(not(contains(key("_lastUpdated"))))),
            request::query(url_decoded(not(contains(key("_summary"))))),
        ])
        .times(1)
        .respond_with(ok(bundle(
            vec![org("org-a", "2026-02-01T00:00:00Z", "A")],
            None,
        ))),
    );
    let a = extract(&server, snap.path(), &[]).success();
    assert!(
        stdout(&a).contains("organizations: 1 resources, 1 new, 0 updated"),
        "{}",
        stdout(&a)
    );
    assert_eq!(org_lines(snap.path()).len(), 1);
    let st = state(snap.path());
    assert_eq!(
        st["watermark"], "2026-01-01T00:00:00Z",
        "the Location watermark is its own"
    );
    assert_eq!(st["organization_watermark"], "2026-02-01T00:00:00Z");
    assert_eq!(st["organization_count"], 1);

    // Incremental: each type asks since its own watermark.
    server.expect(
        Expectation::matching(since_search("2026-01-01T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(vec![], None))),
    );
    server.expect(
        Expectation::matching(org_since_search("2026-02-01T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(
                vec![org("org-b", "2026-02-02T00:00:00Z", "B")],
                None,
            ))),
    );
    extract(&server, snap.path(), &[]).success();
    let orgs = org_lines(snap.path());
    assert_eq!(orgs.len(), 2);
    assert_eq!(orgs[1]["id"], "org-b");
    assert_eq!(
        state(snap.path())["organization_watermark"],
        "2026-02-02T00:00:00Z"
    );
    assert_eq!(state(snap.path())["count"], 1);
}

#[test]
fn a_snapshot_without_organizations_fetches_them_in_full() {
    let server = Server::run();
    expect_counts(&server, 1, 1);
    let snap = tempfile::tempdir().unwrap();
    // A plan 2 snapshot: locations and a state file with no organization fields.
    std::fs::write(
        snap.path().join("locations.ndjson"),
        format!("{}\n", loc("a", "2026-01-01T00:00:00Z", None)),
    )
    .unwrap();
    std::fs::write(
        snap.path().join("state.json"),
        json!({"server": server.url("/fhir").to_string(), "watermark": "2026-01-01T00:00:00Z", "count": 1,
               "kiln_version": "0.2.0", "completed_at": "2026-01-01T00:00:00Z"}).to_string(),
    )
    .unwrap();
    server.expect(
        Expectation::matching(since_search("2026-01-01T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(vec![], None))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            request::query(url_decoded(not(contains(key("_lastUpdated"))))),
            request::query(url_decoded(not(contains(key("_summary"))))),
        ])
        .times(1)
        .respond_with(ok(bundle(
            vec![org("org-a", "2026-02-01T00:00:00Z", "A")],
            None,
        ))),
    );
    extract(&server, snap.path(), &[]).success();
    assert_eq!(org_lines(snap.path()).len(), 1);
    assert_eq!(
        state(snap.path())["organization_watermark"],
        "2026-02-01T00:00:00Z"
    );
}

#[test]
fn a_full_last_page_with_no_next_link_is_cross_checked_against_the_count() {
    // HAPI FHIR 8.12 (stock prefetch thresholds) stopped issuing next links
    // after 3,000 results: the last page was exactly full, and a full extract
    // would have passed a 3,000-row snapshot as complete.
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    let page: Vec<Value> = (0..1000)
        .map(|i| loc(&format!("l{i}"), "2026-01-01T00:00:00Z", None))
        .collect();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(page, None))),
    );
    expect_counts(&server, 2500, 0);
    let out = stdout(&extract(&server, snap.path(), &[]).success());
    assert!(out.contains("count_mismatch: 1"), "{out}");
    assert_eq!(
        lines(snap.path()).len(),
        1000,
        "what was paged is still written"
    );
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    let detail = report["issues"][0]["detail"].as_str().unwrap();
    assert!(
        detail.contains("reports 2500 Location resources") && detail.contains("paged 1000"),
        "{detail}"
    );
}

#[test]
fn a_deletion_on_the_server_is_reported_by_the_count_check() {
    let server = Server::run();
    expect_no_organizations(&server);
    let snap = tempfile::tempdir().unwrap();
    server.expect(
        Expectation::matching(full_search())
            .times(1)
            .respond_with(ok(bundle(
                vec![
                    loc("a", "2026-01-01T00:00:00Z", None),
                    loc("b", "2026-01-02T00:00:00Z", None),
                ],
                None,
            ))),
    );
    let first = extract(&server, snap.path(), &[]).success();
    assert!(
        !stdout(&first).contains("count_mismatch"),
        "a full run makes no count check"
    );

    // "a" was deleted on the server; the incremental search cannot show that.
    server.expect(
        Expectation::matching(since_search("2026-01-02T00:00:00Z"))
            .times(1)
            .respond_with(ok(bundle(vec![], None))),
    );
    expect_counts(&server, 1, 0);
    let second = extract(&server, snap.path(), &[]).success();
    let out = stdout(&second);
    assert!(out.contains("count_mismatch: 1"), "{out}");
    assert_eq!(
        lines(snap.path()).len(),
        2,
        "the snapshot itself is untouched"
    );
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(snap.path().join("_extract_report.json")).unwrap(),
    )
    .unwrap();
    let issue = &report["issues"][0];
    assert_eq!(issue["kind"], "count_mismatch");
    assert_eq!(issue["location_id"], "Location");
    assert!(
        issue["detail"].as_str().unwrap().contains("--full"),
        "{issue}"
    );
}

#[test]
fn client_credentials_from_the_environment_authorise_every_request() {
    let server = Server::run();
    let snap = tempfile::tempdir().unwrap();
    let bearer = || request::headers(contains(("authorization", "Bearer tok")));
    let burl = server.url("/b/b.geojson").to_string();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/token"),
            request::body(url_decoded(contains(("client_id", "key-x")))),
            request::body(url_decoded(contains(("client_secret", "shh")))),
        ])
        .times(1)
        .respond_with(json_encoded(
            json!({"access_token": "tok", "expires_in": 3600}),
        )),
    );
    server.expect(
        Expectation::matching(all_of![full_search(), bearer()])
            .times(1)
            .respond_with(ok(bundle(
                vec![loc("b", "2026-01-02T00:00:00Z", Some(&burl))],
                None,
            ))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/b/b.geojson"),
            bearer()
        ])
        .times(1)
        .respond_with(status_code(200).body(GEOJSON)),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Organization"),
            bearer()
        ])
        .times(1)
        .respond_with(ok(bundle(vec![], None))),
    );

    fhir_pipes()
        .args(["extract", "--server", &server.url("/fhir").to_string()])
        .arg("--snapshot")
        .arg(snap.path())
        .env("FHIR_PIPES_CLIENT_ID", "key-x")
        .env("FHIR_PIPES_CLIENT_SECRET", "shh")
        .env("FHIR_PIPES_TOKEN_URL", server.url("/token").to_string())
        .assert()
        .success();
    assert_eq!(lines(snap.path()).len(), 1);
}
