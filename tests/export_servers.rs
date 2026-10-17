//! `fhir-pipes export --server-kind` against fakes that page as Medplum and
//! Aidbox do, and against real Medplum and Aidbox servers when the
//! environment names them (skipped otherwise):
//!
//! - FHIR_PIPES_TEST_MEDPLUM_URL (the FHIR base, e.g.
//!   http://localhost:8103/fhir/R4), FHIR_PIPES_TEST_MEDPLUM_CLIENT_ID and
//!   FHIR_PIPES_TEST_MEDPLUM_CLIENT_SECRET; FHIR_PIPES_TEST_MEDPLUM_TOKEN_URL
//!   if the token endpoint is not `/oauth2/token` on the same host.
//! - FHIR_PIPES_TEST_AIDBOX_URL (the `/fhir` base),
//!   FHIR_PIPES_TEST_AIDBOX_USERNAME and FHIR_PIPES_TEST_AIDBOX_PASSWORD (a
//!   Client with Basic auth); FHIR_PIPES_TEST_AIDBOX_BULK=1 also checks
//!   `$export`, which needs a bulk storage provider.
//!
//! FHIR_PIPES_TEST_MEDPLUM_TYPES and FHIR_PIPES_TEST_AIDBOX_TYPES
//! (comma-separated, default Patient) name the types the real-server tests
//! download.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::bytes::Bytes;
use httptest::http;
use httptest::responders::Responder;
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

fn ids(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["id"].as_str().unwrap().to_string()
        })
        .collect()
}

const T0: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

fn instant(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn seconds(at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .unwrap()
        .timestamp()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Like {
    /// `_offset` paging that stops past 10,000; `_cursor` links instead with
    /// `_sort=_lastUpdated`.
    Medplum,
    /// `_page` paging; a search without `_total=none` fails, as a count of
    /// a large type runs into Aidbox's search timeout.
    Aidbox,
}

/// A FHIR server holding `n` Observations, one second apart from T0 in an
/// order that is not their time order, that pages like `like`. It
/// filters by `_lastUpdated` (ge, lt), sorts by it and counts with
/// `_summary=count`.
#[derive(Clone)]
struct Fake {
    like: Like,
    data: Arc<Vec<(String, i64)>>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(like: Like, n: usize) -> Self {
        let data = (0..n)
            .map(|i| (format!("o{i:06}"), T0 + ((i * 7919) % n) as i64))
            .collect();
        Fake {
            like,
            data: Arc::new(data),
            queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }

    fn answer(&self, path: &str, query: &str) -> (u16, String) {
        self.queries.lock().unwrap().push(query.to_string());
        let params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let get = |k: &str| -> Vec<&str> {
            params
                .iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .collect()
        };
        let counting = get("_summary") == ["count"];
        if self.like == Like::Aidbox && !counting && get("_total") != ["none"] {
            return (
                500,
                json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error",
                    "code": "exception", "diagnostics": "canceling statement due to statement timeout"}]})
                .to_string(),
            );
        }
        let mut rows: Vec<&(String, i64)> = self.data.iter().collect();
        for v in get("_lastUpdated") {
            let (op, bound) = v.split_at(2);
            let bound = seconds(bound);
            rows.retain(|r| match op {
                "ge" => r.1 >= bound,
                "lt" => r.1 < bound,
                _ => panic!("{v}"),
            });
        }
        if counting {
            return (
                200,
                json!({"resourceType": "Bundle", "type": "searchset", "total": rows.len()})
                    .to_string(),
            );
        }
        let sort = get("_sort").first().copied();
        if let Some(sort) = sort {
            rows.sort_by_key(|r| (r.1, r.0.clone()));
            if sort.starts_with('-') {
                rows.reverse();
            }
        }
        let count: usize = get("_count").first().map_or(20, |c| c.parse().unwrap());
        let (start, next) = match self.like {
            Like::Medplum => {
                let cursor = sort == Some("_lastUpdated") && count >= 20;
                let start: usize = if cursor {
                    get("_cursor").first().map_or(0, |c| c.parse().unwrap())
                } else {
                    get("_offset").first().map_or(0, |c| c.parse().unwrap())
                };
                if start > 10_000 && !cursor {
                    return (
                        400,
                        json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error",
                            "code": "invalid", "details": {"text":
                            format!("Search offset exceeds maximum (got {start}, max 10000)")}}]})
                        .to_string(),
                    );
                }
                let key = if cursor { "_cursor" } else { "_offset" };
                (start, (key, (start + count).to_string()))
            }
            Like::Aidbox => {
                let page: usize = get("_page").first().map_or(1, |p| p.parse().unwrap());
                ((page - 1) * count, ("_page", (page + 1).to_string()))
            }
        };
        let page: Vec<&(String, i64)> = rows.iter().skip(start).take(count).copied().collect();
        let mut bundle = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": page.iter().map(|(id, at)| json!({"resource": {
                "resourceType": "Observation", "id": id, "meta": {"lastUpdated": instant(*at)}
            }})).collect::<Vec<_>>(),
        });
        if start + count < rows.len() {
            let mut link = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in params
                .iter()
                .filter(|(k, _)| !["_offset", "_cursor", "_page"].contains(&k.as_str()))
            {
                link.append_pair(k, v);
            }
            link.append_pair(next.0, &next.1);
            bundle["link"] = json!([{"relation": "next",
                "url": format!("{path}?{}", link.finish())}]);
        }
        (200, bundle.to_string())
    }
}

impl Responder for Fake {
    fn respond<'a>(
        &mut self,
        req: &'a http::Request<Bytes>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = http::Response<Bytes>> + Send + 'a>>
    {
        let fake = self.clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        Box::pin(async move {
            let (status, body) = fake.answer(&path, &query);
            http::Response::builder()
                .status(status)
                .body(Bytes::from(body))
                .unwrap()
        })
    }
}

fn serve(fake: &Fake) -> Server {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Observation"))
            .times(..)
            .respond_with(fake.clone()),
    );
    server
}

fn export(server: &Server, out: &Path, kind: &str, concurrency: &str) -> Assert {
    fhir_pipes()
        .args(["export", "--server", &server.url_str("/fhir")])
        .args(["--type", "Observation", "--server-kind", kind])
        .args(["--concurrency", concurrency, "--retries", "1"])
        .arg("--out")
        .arg(out)
        .assert()
}

/// The queries that asked for a page of resources (not a count or a probe).
fn page_queries(fake: &Fake) -> Vec<String> {
    fake.queries()
        .into_iter()
        .filter(|q| q.contains("_count=1000"))
        .collect()
}

#[test]
fn medplum_pages_past_ten_thousand_with_cursor_links() {
    let fake = Fake::new(Like::Medplum, 11_500);
    let server = serve(&fake);
    let dir = tempfile::tempdir().unwrap();

    // Without the kind, the search pages by offset and stops at 10,000.
    let a = export(&server, &dir.path().join("generic"), "generic", "1").failure();
    assert!(
        stderr(&a).contains("Search offset exceeds maximum"),
        "{}",
        stderr(&a)
    );

    let fake = Fake::new(Like::Medplum, 11_500);
    let server = serve(&fake);
    let out = dir.path().join("medplum");
    let a = export(&server, &out, "medplum", "1").success();
    assert!(
        stdout(&a).contains("Observation: 11500 resources, 12 pages"),
        "{}",
        stdout(&a)
    );
    let got = ids(&out.join("Observation.ndjson"));
    assert_eq!(got.len(), 11_500);
    assert_eq!(got.iter().collect::<BTreeSet<_>>().len(), 11_500);
    let pages = page_queries(&fake);
    assert_eq!(pages.len(), 12, "{pages:?}");
    for q in &pages {
        assert!(q.contains("_sort=_lastUpdated"), "{q}");
        assert!(!q.contains("_offset"), "{q}");
    }
}

#[test]
fn medplum_windows_are_paged_with_cursor_links_too() {
    let fake = Fake::new(Like::Medplum, 11_500);
    let server = serve(&fake);
    let dir = tempfile::tempdir().unwrap();
    let a = export(&server, dir.path(), "medplum", "4").success();
    assert!(
        stdout(&a).contains("_lastUpdated windows"),
        "{}",
        stdout(&a)
    );
    let got = ids(&dir.path().join("Observation.ndjson"));
    assert_eq!(got.iter().collect::<BTreeSet<_>>().len(), 11_500);
    for q in page_queries(&fake) {
        assert!(q.contains("_sort=_lastUpdated"), "{q}");
        assert!(!q.contains("_offset"), "{q}");
    }
}

#[test]
fn aidbox_pages_ask_for_no_total() {
    let fake = Fake::new(Like::Aidbox, 3_500);
    let server = serve(&fake);
    let dir = tempfile::tempdir().unwrap();
    let a = export(&server, &dir.path().join("generic"), "generic", "1").failure();
    assert!(stderr(&a).contains("HTTP 500"), "{}", stderr(&a));

    let fake = Fake::new(Like::Aidbox, 3_500);
    let server = serve(&fake);
    let out = dir.path().join("aidbox");
    let a = export(&server, &out, "aidbox", "4").success();
    assert!(
        stdout(&a).contains("Observation: 3500 resources, 4 pages"),
        "{}",
        stdout(&a)
    );
    assert_eq!(ids(&out.join("Observation.ndjson")).len(), 3_500);
    let pages = page_queries(&fake);
    assert_eq!(pages.len(), 4, "{pages:?}");
    assert!(pages.iter().any(|q| q.contains("_page=4")), "{pages:?}");
    for q in fake.queries() {
        assert!(
            q.contains("_total=none") || q.contains("_summary=count"),
            "{q}"
        );
    }
}

#[test]
fn aidbox_windows_hold_at_most_fifty_thousand_even_one_request_at_a_time() {
    let fake = Fake::new(Like::Aidbox, 50_500);
    let server = serve(&fake);
    let dir = tempfile::tempdir().unwrap();
    let a = export(&server, dir.path(), "aidbox", "1").success();
    assert!(
        stdout(&a).contains("_lastUpdated windows"),
        "{}",
        stdout(&a)
    );
    let got = ids(&dir.path().join("Observation.ndjson"));
    assert_eq!(got.iter().collect::<BTreeSet<_>>().len(), 50_500);
    // Each window's first page, by its bounds: none holds more than 50,000.
    let firsts: Vec<String> = page_queries(&fake)
        .into_iter()
        .filter(|q| q.contains("_lastUpdated") && !q.contains("_page="))
        .collect();
    assert!(firsts.len() >= 2, "{firsts:?}");
    for q in &firsts {
        let mut held: Vec<&(String, i64)> = fake.data.iter().collect();
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            if k == "_lastUpdated" {
                let bound = seconds(&v[2..]);
                if v.starts_with("ge") {
                    held.retain(|r| r.1 >= bound);
                } else {
                    held.retain(|r| r.1 < bound);
                }
            }
        }
        assert!(held.len() <= 50_000, "{q}: {}", held.len());
    }
}

#[test]
fn a_medplum_count_from_a_million_on_is_taken_as_an_estimate() {
    let server = Server::run();
    let dir = tempfile::tempdir().unwrap();
    let observation = |id: &str| json!({"resourceType": "Observation", "id": id, "meta": {"lastUpdated": "2026-10-01T00:00:00Z"}});
    std::fs::write(
        dir.path().join("Observation.ndjson"),
        format!("{}\n{}\n", observation("a"), observation("b")),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".export-state.json"),
        json!({"server": server.url_str("/fhir"), "types": {"Observation": {
            "watermark": "2026-10-01T00:00:00Z", "count": 2,
            "completed_at": "2026-10-01T00:00:00Z", "fhir_pipes_version": "0.5.0"}}})
        .to_string(),
    )
    .unwrap();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Observation"),
            request::query(url_decoded(contains(key("_lastUpdated")))),
            request::query(url_decoded(contains(("_sort", "_lastUpdated")))),
        ])
        .times(1)
        .respond_with(
            status_code(200).body(
                json!({"resourceType": "Bundle", "type": "searchset",
                    "entry": [{"resource": observation("a")}]})
                .to_string(),
            ),
        ),
    );
    // Not the file's 2, but an estimate: the type is not downloaded again.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Observation"),
            request::query(url_decoded(contains(("_summary", "count")))),
        ])
        .times(1)
        .respond_with(status_code(200).body(
            json!({"resourceType": "Bundle", "type": "searchset", "total": 1_500_000}).to_string(),
        )),
    );
    let a = fhir_pipes()
        .args([
            "export",
            "--incremental",
            "--server",
            &server.url_str("/fhir"),
        ])
        .args(["--type", "Observation", "--server-kind", "medplum"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    assert!(
        stderr(&a).contains("the server's count (1500000) is an estimate"),
        "{}",
        stderr(&a)
    );
    assert!(
        !stdout(&a).contains("downloading in full"),
        "{}",
        stdout(&a)
    );
    assert_eq!(ids(&dir.path().join("Observation.ndjson")).len(), 2);
}

#[test]
fn a_kick_off_without_bulk_storage_names_the_aidbox_setting() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/$export"))
            .times(..)
            .respond_with(status_code(500).body(r#"{"message":"storage-type not specified"}"#)),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = fhir_pipes()
        .args(["export", "--bulk", "--server", &server.url_str("/fhir")])
        .args(["--type", "Patient", "--retries", "1"])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .failure();
    let err = stderr(&a);
    assert!(err.contains("storage-type not specified"), "{err}");
    assert!(err.contains("BOX_FHIR_BULK_STORAGE_PROVIDER"), "{err}");
}

#[test]
fn medplum_bulk_files_are_binary_references_read_with_the_token() {
    // As Medplum 5 answers: each file is `Binary/<id>`, relative to the FHIR
    // base, and needs the token although requiresAccessToken is false.
    let server = Server::run();
    let bearer = || request::headers(contains(("authorization", "Bearer tok")));
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/$export"),
            bearer()
        ])
        .times(1)
        .respond_with(
            status_code(202).insert_header("Content-Location", "/fhir/bulkdata/export/job-1"),
        ),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/bulkdata/export/job-1"),
            bearer()
        ])
        .times(1)
        .respond_with(
            status_code(200).body(
                json!({"transactionTime": "2026-10-15T08:00:00Z", "requiresAccessToken": false,
                    "request": "x", "output": [{"type": "Patient", "url": "Binary/b-1"}],
                    "error": [], "deleted": []})
                .to_string(),
            ),
        ),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/fhir/Binary/b-1"),
            bearer()
        ])
        .times(1)
        .respond_with(status_code(200).body(format!(
            "{}\n",
            json!({"resourceType": "Patient", "id": "p1", "meta": {"lastUpdated": "2026-10-15T07:00:00Z"}})
        ))),
    );
    server.expect(
        Expectation::matching(request::method_path(
            "DELETE",
            "/fhir/bulkdata/export/job-1",
        ))
        .times(1)
        .respond_with(status_code(202)),
    );
    let dir = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args(["export", "--bulk", "--server", &server.url_str("/fhir")])
        .args([
            "--type",
            "Patient",
            "--server-kind",
            "medplum",
            "--token",
            "tok",
        ])
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success();
    assert_eq!(ids(&dir.path().join("Patient.ndjson")), ["p1"]);
}

#[test]
fn the_server_kind_is_one_of_four() {
    let a = fhir_pipes()
        .args(["export", "--server", "http://127.0.0.1:9/fhir"])
        .args([
            "--type",
            "Patient",
            "--server-kind",
            "fhirbase",
            "--out",
            "x",
        ])
        .assert()
        .code(2);
    let err = stderr(&a);
    for kind in ["generic", "hapi", "medplum", "aidbox"] {
        assert!(err.contains(kind), "{err}");
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn types(var: &str) -> Vec<String> {
    env(var)
        .unwrap_or_else(|| "Patient".into())
        .split(',')
        .map(|t| t.trim().to_string())
        .collect()
}

/// A real server's export, by search and then incrementally (nothing
/// should be downloaded in full), and by `$export` with `bulk`: the bulk
/// files must hold the same ids as the search's.
fn check_real_server(
    kind: &str,
    base: &str,
    auth: &dyn Fn(&mut Command),
    types: &[String],
    bulk: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let run = |out: &Path, extra: &[&str]| {
        let mut c = fhir_pipes();
        c.args(["export", "--server", base, "--server-kind", kind]);
        for t in types {
            c.args(["--type", t]);
        }
        c.args(extra).arg("--out").arg(out);
        auth(&mut c);
        let a = c.assert();
        let output = a.get_output();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        a.success()
    };
    let search = dir.path().join("search");
    run(&search, &["--incremental"]);
    let again = run(&search, &["--incremental"]);
    assert!(
        !stdout(&again).contains("downloading in full"),
        "{}",
        stdout(&again)
    );
    if !bulk {
        return;
    }
    let by_bulk = dir.path().join("bulk");
    run(&by_bulk, &["--bulk"]);
    for t in types {
        let file = format!("{t}.ndjson");
        let a: BTreeSet<String> = ids(&search.join(&file)).into_iter().collect();
        let b: BTreeSet<String> = ids(&by_bulk.join(&file)).into_iter().collect();
        assert_eq!(a, b, "{t}: search and $export differ");
    }
}

#[test]
fn a_real_medplum_server() {
    let Some(base) = env("FHIR_PIPES_TEST_MEDPLUM_URL") else {
        eprintln!("FHIR_PIPES_TEST_MEDPLUM_URL is not set; skipped");
        return;
    };
    let id = env("FHIR_PIPES_TEST_MEDPLUM_CLIENT_ID").expect("FHIR_PIPES_TEST_MEDPLUM_CLIENT_ID");
    let secret = env("FHIR_PIPES_TEST_MEDPLUM_CLIENT_SECRET")
        .expect("FHIR_PIPES_TEST_MEDPLUM_CLIENT_SECRET");
    let token_url = env("FHIR_PIPES_TEST_MEDPLUM_TOKEN_URL").unwrap_or_else(|| {
        let u = url::Url::parse(&base).unwrap();
        u.join("/oauth2/token").unwrap().to_string()
    });
    let auth = |c: &mut Command| {
        c.env("FHIR_PIPES_CLIENT_ID", &id)
            .env("FHIR_PIPES_CLIENT_SECRET", &secret)
            .env("FHIR_PIPES_TOKEN_URL", &token_url);
    };
    check_real_server(
        "medplum",
        &base,
        &auth,
        &types("FHIR_PIPES_TEST_MEDPLUM_TYPES"),
        true,
    );
}

#[test]
fn a_real_aidbox_server() {
    let Some(base) = env("FHIR_PIPES_TEST_AIDBOX_URL") else {
        eprintln!("FHIR_PIPES_TEST_AIDBOX_URL is not set; skipped");
        return;
    };
    let user = env("FHIR_PIPES_TEST_AIDBOX_USERNAME").expect("FHIR_PIPES_TEST_AIDBOX_USERNAME");
    let password = env("FHIR_PIPES_TEST_AIDBOX_PASSWORD").expect("FHIR_PIPES_TEST_AIDBOX_PASSWORD");
    let auth = |c: &mut Command| {
        c.env("FHIR_PIPES_USERNAME", &user)
            .env("FHIR_PIPES_PASSWORD", &password);
    };
    check_real_server(
        "aidbox",
        &base,
        &auth,
        &types("FHIR_PIPES_TEST_AIDBOX_TYPES"),
        env("FHIR_PIPES_TEST_AIDBOX_BULK").is_some_and(|v| v == "1"),
    );
}
