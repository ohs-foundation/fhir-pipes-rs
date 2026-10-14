//! Binary-level tests for `kiln load` against a fake FHIR server.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::matchers::ExecutionContext;
use httptest::{all_of, cycle, matchers::*, responders::*, Expectation, Server};
use serde_json::{json, Value};

fn loc(id: &str, version: Option<&str>, parent: Option<&str>) -> Value {
    let mut r = json!({"resourceType": "Location", "id": id, "name": id, "status": "active"});
    if let Some(v) = version {
        r["meta"] = json!({"versionId": v});
    }
    if let Some(p) = parent {
        r["partOf"] = json!({"reference": format!("Location/{p}")});
    }
    r
}

fn write_ndjson(dir: &Path, resources: &[Value]) -> PathBuf {
    let p = dir.join("changes.ndjson");
    let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(&p, text).unwrap();
    p
}

fn kiln() -> Command {
    let mut c = Command::cargo_bin("kiln").unwrap();
    for var in [
        "KILN_TOKEN",
        "KILN_CLIENT_ID",
        "KILN_CLIENT_SECRET",
        "KILN_TOKEN_URL",
    ] {
        c.env_remove(var);
    }
    c
}

fn load(server: &Server, input: &Path, extra: &[&str]) -> Assert {
    kiln()
        .args(["load", "--server", &server.url("/fhir").to_string(), "--in"])
        .arg(input)
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

fn capability(update_create: bool) -> String {
    json!({"resourceType": "CapabilityStatement", "rest": [{"resource": [
        {"type": "Location", "updateCreate": update_create}]}]})
    .to_string()
}

fn capability_unstated() -> String {
    json!({"resourceType": "CapabilityStatement", "rest": [{"resource": [{"type": "Location"}]}]})
        .to_string()
}

fn expect_metadata(server: &Server, update_create: bool) {
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/metadata"))
            .respond_with(ok(capability(update_create))),
    );
}

fn transaction_response() -> String {
    json!({"resourceType": "Bundle", "type": "transaction-response"}).to_string()
}

type Req = httptest::http::Request<httptest::bytes::Bytes>;

/// Matches a POSTed bundle by a predicate over its JSON.
struct BundleWhere(fn(&Value) -> bool);

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

fn posted_bundle(pred: fn(&Value) -> bool) -> impl Matcher<Req> {
    all_of![request::method_path("POST", "/fhir"), BundleWhere(pred)]
}

fn any_bundle() -> impl Matcher<Req> {
    request::method_path("POST", "/fhir")
}

#[test]
fn bundles_arrive_parents_first_with_if_match_on_existing_entries() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(
        dir.path(),
        &[
            loc("clinic", Some("2"), Some("ward")),
            loc("ward", Some("1"), Some("state")),
            loc("state", Some("3"), None),
            loc("newsite", None, Some("ward")),
        ],
    );
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            b["type"] == "transaction"
                && b["entry"][0]["request"]
                    == json!({"method": "PUT", "url": "Location/state", "ifMatch": "W/\"3\""})
                && b["entry"][1]["request"]
                    == json!({"method": "PUT", "url": "Location/ward", "ifMatch": "W/\"1\""})
                && b["entry"][0]["resource"]["name"] == "state"
        }))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            b["entry"][0]["request"]
                == json!({"method": "PUT", "url": "Location/clinic", "ifMatch": "W/\"2\""})
                && b["entry"][1]["request"] == json!({"method": "PUT", "url": "Location/newsite"})
        }))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    let a = load(&server, &input, &["--batch-size", "2"]).success();
    assert!(
        stderr(&a).contains("bundle 2/2 committed (2 resources)"),
        "{}",
        stderr(&a)
    );
    assert!(
        stdout(&a).contains("Loaded 4 resources in 2 bundle(s)"),
        "{}",
        stdout(&a)
    );
}

#[test]
fn update_create_is_required_only_when_the_input_creates() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(
        dir.path(),
        &[loc("a", Some("1"), None), loc("b", None, None)],
    );
    expect_metadata(&server, false);
    load(&server, &input, &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("updateCreate"));

    let server = Server::run();
    let input = write_ndjson(dir.path(), &[loc("a", Some("1"), None)]);
    expect_metadata(&server, false);
    server.expect(
        Expectation::matching(any_bundle())
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    load(&server, &input, &[]).success();
}

#[test]
fn an_unstated_update_create_flag_warns_and_proceeds() {
    // HAPI FHIR supports update-as-create but omits rest.resource.updateCreate.
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(dir.path(), &[loc("b", None, None)]);
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/metadata"))
            .respond_with(ok(capability_unstated())),
    );
    server.expect(
        Expectation::matching(any_bundle())
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    load(&server, &input, &[])
        .success()
        .stderr(predicates::str::contains(
            "does not state updateCreate for Location",
        ));
}

#[test]
fn a_412_names_the_conflicting_ids_and_stops() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(
        dir.path(),
        &[
            loc("a", Some("1"), None),
            loc("b", Some("2"), None),
            loc("c", Some("3"), None),
        ],
    );
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(any_bundle())
            .times(2)
            .respond_with(cycle![
                ok(transaction_response()),
                status_code(412).body(r#"{"resourceType":"OperationOutcome"}"#),
            ]),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Location/b"))
            .times(1)
            .respond_with(ok(
                json!({"resourceType": "Location", "id": "b", "meta": {"versionId": "5"}})
                    .to_string(),
            )),
    );
    let a = load(&server, &input, &["--batch-size", "1"])
        .failure()
        .code(1);
    let err = stderr(&a);
    assert!(err.contains("bundle 2/3 rejected with HTTP 412"), "{err}");
    assert!(
        err.contains("Location/b (expected version 2, server has 5)"),
        "{err}"
    );
    assert!(err.contains("bundle 1/3 committed"), "{err}");
    assert!(err.contains("diff again"), "{err}");
}

#[test]
fn a_503_is_retried_and_a_400_aborts_with_the_body() {
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(dir.path(), &[loc("a", Some("1"), None)]);

    let server = Server::run();
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(any_bundle())
            .times(2)
            .respond_with(cycle![status_code(503), ok(transaction_response())]),
    );
    load(&server, &input, &[]).success();

    let server = Server::run();
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(any_bundle())
            .times(1)
            .respond_with(status_code(400).body("no such profile")),
    );
    load(&server, &input, &[])
        .failure()
        .code(1)
        .stderr(predicates::str::contains("HTTP 400: no such profile"));
}

#[test]
fn dry_run_posts_nothing_and_prints_the_plan() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(
        dir.path(),
        &[
            loc("clinic", None, Some("state")),
            loc("state", Some("3"), None),
        ],
    );
    expect_metadata(&server, true);
    let a = load(&server, &input, &["--dry-run"]).success();
    let out = stdout(&a);
    assert!(out.contains("Dry run: 2 resources in 1 bundle(s)"), "{out}");
    assert!(
        out.contains("bundle 1/1: Location/state update@3, Location/clinic create"),
        "{out}"
    );
}

#[test]
fn input_problems_are_usage_errors_before_any_request() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();

    let input = write_ndjson(
        dir.path(),
        &[
            loc("a", Some("1"), Some("b")),
            loc("b", Some("1"), Some("a")),
        ],
    );
    load(&server, &input, &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("cycle"));

    let server = Server::run();
    let input = write_ndjson(
        dir.path(),
        &[loc("a", Some("1"), None), loc("a", Some("1"), None)],
    );
    load(&server, &input, &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("duplicate resource Location/a"));

    let input = write_ndjson(
        dir.path(),
        &[json!({"resourceType": "Location", "name": "no id"})],
    );
    load(&server, &input, &[])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("line 1"));

    let input = write_ndjson(dir.path(), &[loc("a", Some("1"), None)]);
    load(&server, &input, &["--batch-size", "0"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--batch-size"));
}

#[test]
fn organizations_are_posted_before_their_locations() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let mut clinic = loc("clinic", Some("2"), None);
    clinic["managingOrganization"] = json!({"reference": "Organization/org-clinic"});
    let org = json!({"resourceType": "Organization", "id": "org-clinic", "name": "clinic", "meta": {"versionId": "5"}});
    let input = write_ndjson(dir.path(), &[clinic, org]);
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            b["entry"][0]["request"]
                == json!({"method": "PUT", "url": "Organization/org-clinic", "ifMatch": "W/\"5\""})
                && b["entry"][1]["request"]["url"] == "Location/clinic"
        }))
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    load(&server, &input, &[]).success();
}

#[test]
fn client_credentials_authorise_the_preflight_and_every_bundle() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(dir.path(), &[loc("state", Some("3"), None)]);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/token"),
            request::body(url_decoded(contains(("grant_type", "client_credentials")))),
            request::body(url_decoded(contains(("client_id", "key-x")))),
            request::body(url_decoded(contains(("client_secret", "shh")))),
        ])
        .times(1)
        .respond_with(json_encoded(
            json!({"access_token": "tok", "expires_in": 3600}),
        )),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/metadata"),
            request::headers(contains(("authorization", "Bearer tok"))),
        ])
        .times(1)
        .respond_with(ok(capability(true))),
    );
    server.expect(
        Expectation::matching(all_of![
            any_bundle(),
            request::headers(contains(("authorization", "Bearer tok"))),
        ])
        .times(1)
        .respond_with(ok(transaction_response())),
    );
    // The secret from the environment, the rest as flags.
    kiln()
        .args(["load", "--server", &server.url("/fhir").to_string(), "--in"])
        .arg(&input)
        .args(["--client-id", "key-x", "--token-url"])
        .arg(server.url("/token").to_string())
        .env("KILN_CLIENT_SECRET", "shh")
        .assert()
        .success();
}

#[test]
fn rejected_client_credentials_stop_before_the_preflight() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(dir.path(), &[loc("state", Some("3"), None)]);
    server.expect(
        Expectation::matching(request::method_path("POST", "/token"))
            .times(1)
            .respond_with(status_code(401).body(r#"{"error":"unauthorized_client"}"#)),
    );
    let token_url = server.url("/token").to_string();
    let a = load(
        &server,
        &input,
        &[
            "--client-id",
            "key-x",
            "--client-secret",
            "wrong",
            "--token-url",
            &token_url,
        ],
    )
    .failure()
    .code(1);
    let err = stderr(&a);
    assert!(err.contains("cannot get a token from"), "{err}");
    assert!(err.contains("unauthorized_client"), "{err}");
}

#[test]
fn incomplete_or_conflicting_auth_flags_are_usage_errors() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = write_ndjson(dir.path(), &[loc("state", Some("3"), None)]);
    // No expectations: none of these may reach the server.
    for flags in [
        &["--client-id", "key-x", "--client-secret", "s"][..],
        &["--client-id", "key-x", "--token-url", "http://t"][..],
        &["--client-secret", "s"][..],
        &[
            "--token",
            "t",
            "--client-id",
            "key-x",
            "--client-secret",
            "s",
            "--token-url",
            "http://t",
        ][..],
    ] {
        load(&server, &input, flags).failure().code(2);
    }
}

#[test]
fn a_gzip_input_is_read_like_a_plain_one() {
    use std::io::Write;
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("changes.ndjson.gz");
    let mut gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&input).unwrap(),
        flate2::Compression::default(),
    );
    for r in [
        loc("state", Some("3"), None),
        loc("ward", Some("1"), Some("state")),
    ] {
        writeln!(gz, "{r}").unwrap();
    }
    gz.finish().unwrap();
    expect_metadata(&server, true);
    server.expect(
        Expectation::matching(any_bundle())
            .times(1)
            .respond_with(ok(transaction_response())),
    );
    let a = load(&server, &input, &[]).success();
    assert!(
        stdout(&a).contains("Loaded 2 resources in 1 bundle(s)"),
        "{}",
        stdout(&a)
    );
}

#[test]
fn parallel_bundles_still_put_parents_first() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    // One resource per bundle: a three-level chain and two on their own.
    let input = write_ndjson(
        dir.path(),
        &[
            loc("state", Some("3"), None),
            loc("ward", Some("1"), Some("state")),
            loc("clinic", Some("2"), Some("ward")),
            loc("lone1", Some("1"), None),
            loc("lone2", Some("1"), None),
        ],
    );
    expect_metadata(&server, true);
    let slow = |ms| {
        delay_and_then(
            std::time::Duration::from_millis(ms),
            ok(transaction_response()),
        )
    };
    // The chain is slow to commit; the lone ones are quick.
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            b["entry"][0]["request"]["url"] == "Location/state"
        }))
        .times(1)
        .respond_with(slow(300)),
    );
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            b["entry"][0]["request"]["url"] == "Location/ward"
        }))
        .times(1)
        .respond_with(slow(100)),
    );
    server.expect(
        Expectation::matching(posted_bundle(|b| {
            let url = &b["entry"][0]["request"]["url"];
            url == "Location/clinic" || url == "Location/lone1" || url == "Location/lone2"
        }))
        .times(3)
        .respond_with(ok(transaction_response())),
    );
    let a = load(
        &server,
        &input,
        &["--batch-size", "1", "--concurrency", "4"],
    )
    .success();
    let err = stderr(&a);
    let at = |n: usize| err.find(&format!("bundle {n}/5 committed")).unwrap();
    // Roots first by depth: state, lone1, lone2 (depth 0), then ward, clinic.
    // The lone ones do not wait for the slow state bundle...
    assert!(at(2) < at(1) && at(3) < at(1), "{err}");
    // ...but ward waits for state, and clinic for ward.
    assert!(at(1) < at(4) && at(4) < at(5), "{err}");
    assert!(
        stdout(&a).contains("Loaded 5 resources in 5 bundle(s)"),
        "{}",
        stdout(&a)
    );
}
