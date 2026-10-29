//! `$sql-run` in a running `fhir-pipes serve`: the `[serve.query]` section,
//! which token opens which path, and the catalog reloaded after a run. The
//! warehouse is built by `fhir-pipes refresh` from files; each test that
//! runs a query is skipped without duckdb.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use common::libraries::*;
use common::serve::*;
use common::*;

const QUERY_TOKEN: &str = "query-test-token";
const RANK: &str = "https://example.org/Library/ReadingRank";
const TOTALS: &str = "https://example.org/Library/ReadingTotals";
const QUERY_SECTION: &str = "\n[serve.query]\napprove = \"all\"\n";

/// A warehouse whose config reads `input/` and names `libs/` (holding
/// ReadingRank) as a library source, with `serve_section` after it.
/// Returns (dir, config).
fn warehouse(serve_section: &str) -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = setup(
        NOWHERE,
        &format!("\n[source]\nfiles = [\"input\"]\n{serve_section}"),
    );
    let root = dir.path();
    views(root);
    let text = fs::read_to_string(&config).unwrap();
    let views_line = text.lines().find(|l| l.starts_with("views = ")).unwrap();
    let text = text.replace(views_line, "views = 'my-views'");
    fs::write(&config, format!("libraries = ['libs']\n{text}")).unwrap();
    fs::create_dir_all(root.join("libs")).unwrap();
    let rank = library(
        "ReadingRank",
        false,
        &[("r", READING)],
        "SELECT id, value FROM r ORDER BY value DESC, id",
    );
    fs::write(root.join("libs/rank.json"), rank.to_string()).unwrap();
    inputs(
        root,
        &[(
            "a.ndjson",
            lines(&[
                obs("o1", 3, None),
                obs("o2", 50, None),
                obs("o3", 7, None),
                obs("o4", 50, None),
                obs("o5", 1, None),
            ]),
        )],
    );
    (dir, config)
}

/// The warehouse with `[serve.query] approve = "all"`, refreshed.
fn refreshed() -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = warehouse(QUERY_SECTION);
    cmd(&config)
        .args(["--only", "views,libraries"])
        .assert()
        .success();
    (dir, config)
}

/// GET `path` with `token`, if any; (status, body).
fn get(s: &Serve, path: &str, token: Option<&str>) -> (u16, String) {
    let mut req = client().get(format!("{}{path}", s.url));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().unwrap();
    (r.status().as_u16(), r.text().unwrap())
}

fn sql_run(s: &Serve, canonical: &str, token: Option<&str>) -> (u16, String) {
    get(
        s,
        &format!("/fhir/$sql-run?subjectCanonical={canonical}"),
        token,
    )
}

fn ndjson(body: &str) -> Vec<Value> {
    body.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Wait until serve has no current run: the run's thread is done,
/// the catalog reloaded.
fn idle(s: &Serve) {
    let started = Instant::now();
    loop {
        let (status, text) = s.get("/status");
        assert_eq!(status, 200, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["current"].is_null() {
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "{v}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_query_returns_the_refreshs_rows() {
    if !have_duckdb("a_query_returns_the_refreshs_rows") {
        return;
    }
    let (_dir, config) = refreshed();
    let s = start_serve(&config, &[]);
    let (status, body) = sql_run(&s, RANK, None);
    assert_eq!(status, 200, "{body}");
    let got = ndjson(&body);
    let table: Vec<Value> = duckdb_value(
        &config.parent().unwrap().join("data"),
        "SELECT to_json(t) FROM read_parquet('parquet/reading_rank/**/*.parquet', \
         hive_partitioning = true) AS t",
    )
    .lines()
    .map(|l| serde_json::from_str(l).unwrap())
    .collect();
    assert_eq!(got, table);
    let ids: Vec<&str> = got.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["o2", "o4", "o3", "o1", "o5"]);
}

#[test]
fn the_query_token_only_opens_fhir() {
    let (_dir, config) = warehouse(QUERY_SECTION);
    let s = start_serve(
        &config,
        &[
            ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
            ("FHIR_PIPES_QUERY_TOKEN", QUERY_TOKEN),
        ],
    );
    assert_eq!(get(&s, "/fhir/metadata", Some(QUERY_TOKEN)).0, 200);
    for path in ["/status", "/runs", "/metrics"] {
        assert_eq!(get(&s, path, Some(QUERY_TOKEN)).0, 401, "{path}");
    }
    let r = client()
        .post(format!("{}/run", s.url))
        .bearer_auth(QUERY_TOKEN)
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);
    for path in ["/fhir/metadata", "/status"] {
        assert_eq!(get(&s, path, Some(TOKEN)).0, 200, "{path}");
    }
    for token in [None, Some("wrong-token")] {
        assert_eq!(get(&s, "/fhir/metadata", token).0, 401, "{token:?}");
    }
    assert_eq!(get(&s, "/health", None).0, 200);
}

#[test]
fn the_serve_token_alone_guards_fhir() {
    let (_dir, config) = warehouse(QUERY_SECTION);
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    assert_eq!(get(&s, "/fhir/metadata", Some(TOKEN)).0, 200);
    for token in [None, Some(QUERY_TOKEN)] {
        assert_eq!(get(&s, "/fhir/metadata", token).0, 401, "{token:?}");
    }
}

#[test]
fn without_a_query_section_fhir_is_404() {
    let (_dir, config) = warehouse("");
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    // The token is checked first: an unauthenticated caller cannot tell.
    assert_eq!(get(&s, "/fhir/metadata", None).0, 401);
    for path in ["/fhir/metadata", "/fhir/$sql-run", "/fhir"] {
        assert_eq!(get(&s, path, Some(TOKEN)).0, 404, "{path}");
    }
    drop(s);
    let open = start_serve(&config, &[]);
    assert_eq!(get(&open, "/fhir/metadata", None).0, 404);
}

#[test]
fn a_non_loopback_listen_needs_the_query_token() {
    let (_dir, config) = warehouse(QUERY_SECTION);
    let (code, stderr) = serve_fails(&config, "0.0.0.0:0", &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("FHIR_PIPES_QUERY_TOKEN"), "{stderr}");
}

#[test]
fn a_catalog_that_does_not_load_stops_serve() {
    let (_dir, config) = warehouse(
        "\n[serve.query]\napprove = \"listed\"\nqueries = [\"https://example.org/Library/Nope\"]\n",
    );
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("https://example.org/Library/Nope"),
        "{stderr}"
    );
}

#[test]
fn a_new_query_is_served_after_a_run() {
    if !have_duckdb("a_new_query_is_served_after_a_run") {
        return;
    }
    let (dir, config) = refreshed();
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    assert_eq!(sql_run(&s, TOTALS, Some(TOKEN)).0, 404);
    let totals = library(
        "ReadingTotals",
        false,
        &[("r", READING)],
        "SELECT count(*) AS n, sum(value) AS total FROM r",
    );
    fs::write(dir.path().join("libs/totals.json"), totals.to_string()).unwrap();
    let id = s.start_run(r#"{"only": ["libraries"]}"#);
    let r = s.finished(&id);
    assert_eq!(r["status"], "succeeded", "{r}");
    idle(&s);
    let (status, body) = sql_run(&s, TOTALS, Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    let row = &ndjson(&body)[0];
    assert_eq!(
        (row["n"].as_i64(), row["total"].as_i64()),
        (Some(5), Some(111))
    );
}

#[test]
fn a_catalog_that_does_not_reload_is_kept() {
    if !have_duckdb("a_catalog_that_does_not_reload_is_kept") {
        return;
    }
    let (dir, config) = refreshed();
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    // A url $sql-run could not name: the catalog is refused.
    let mut bar = library("Bar", false, &[], "SELECT 1 AS x");
    bar["url"] = "https://example.org/Library/Bar|1".into();
    fs::write(dir.path().join("libs/bar.json"), bar.to_string()).unwrap();
    let id = s.start_run(r#"{"only": ["libraries"]}"#);
    s.finished(&id);
    idle(&s);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = s.lines.recv_timeout(left).expect("no reload error printed");
        if line.starts_with("query catalog not reloaded: ") {
            break;
        }
    }
    assert_eq!(sql_run(&s, RANK, Some(TOKEN)).0, 200);
}

/// The query API's settings are read as refresh reads them, the
/// environment included: a server password from FHIR_PIPES_PASSWORD.
#[test]
fn a_config_refresh_accepts_starts_the_query_api() {
    let (_dir, config) = warehouse(QUERY_SECTION);
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("[source]\nfiles = [\"input\"]\n", "")
        .replace("name = \"test\"\n", "name = \"test\"\nusername = \"me\"\n");
    assert!(text.contains("username = \"me\"") && !text.contains("[source]"));
    fs::write(&config, text).unwrap();
    let s = start_serve(&config, &[("FHIR_PIPES_PASSWORD", "pw")]);
    assert_eq!(get(&s, "/fhir/metadata", None).0, 200);
    drop(s);
    // Without it, serve stops as refresh would.
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("FHIR_PIPES_PASSWORD"), "{stderr}");
}

/// A refresh started by serve is not given the query token, and a run
/// that fails leaves the catalog as it was.
#[test]
fn a_failed_run_keeps_the_catalog_and_never_sees_the_query_token() {
    if !have_duckdb("a_failed_run_keeps_the_catalog_and_never_sees_the_query_token") {
        return;
    }
    let (dir, config) = warehouse(&format!(
        "\n[[hooks]]\nname = \"leak\"\nrun = [\"sh\", \"-c\", \"echo q=$FHIR_PIPES_QUERY_TOKEN; exit 1\"]\n\
         \n[serve]\nretries = 0\n{QUERY_SECTION}"
    ));
    cmd(&config)
        .args(["--only", "views,libraries"])
        .assert()
        .success();
    let s = start_serve(
        &config,
        &[
            ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
            ("FHIR_PIPES_QUERY_TOKEN", QUERY_TOKEN),
        ],
    );
    let totals = library(
        "ReadingTotals",
        false,
        &[("r", READING)],
        "SELECT count(*) AS n FROM r",
    );
    fs::write(dir.path().join("libs/totals.json"), totals.to_string()).unwrap();
    let id = s.start_run(r#"{"only": ["libraries", "hooks"]}"#);
    let r = s.finished(&id);
    assert_eq!(r["status"], "failed", "{r}");
    idle(&s);
    let (_, log) = s.get(&format!("/runs/{id}/log"));
    assert!(log.contains("q="), "{log}");
    assert!(!log.contains(QUERY_TOKEN), "{log}");
    assert_eq!(sql_run(&s, TOTALS, Some(TOKEN)).0, 404);
}

/// A Library source's URL can carry a token: the error that stops serve
/// does not show it.
#[test]
fn a_library_url_token_is_not_printed() {
    let (_dir, config) = warehouse(QUERY_SECTION);
    let text = fs::read_to_string(&config).unwrap().replacen(
        "libraries = ['libs']",
        "libraries = ['libs', 'https://u:k1(user)@127.0.0.1:9/p(v1).tgz?token=k2(query)']",
        1,
    );
    fs::write(&config, text).unwrap();
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("https://127.0.0.1:9/p(v1).tgz"), "{stderr}");
    for secret in ["k1", "user", "k2", "query)"] {
        assert!(!stderr.contains(secret), "{secret}: {stderr}");
    }
}
