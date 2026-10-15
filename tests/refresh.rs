//! End-to-end runs of `fhir-pipes refresh` against fixtures, with a fake FHIR
//! server serving the fixture NDJSON.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use predicates::str::contains as has;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

fn read_ndjson(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn bundle(resources: Vec<Value>) -> String {
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Serve each fixture type, once, as a single search page.
fn serve_types(server: &Server, types: &[&str]) {
    for t in types {
        let resources = read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")));
        server.expect(
            Expectation::matching(request::method_path("GET", format!("/fhir/{t}")))
                .times(1)
                .respond_with(status_code(200).body(bundle(resources))),
        );
    }
}

/// A warehouse directory with the fixture locations table and a config file
/// pointing at `base`. Returns (tempdir, config path).
fn setup(base: &str, extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let loc = root.join("data/parquet/locations/country=SLE");
    fs::create_dir_all(&loc).unwrap();
    fs::copy(
        fixtures().join("parquet/locations/country=SLE/part-0.parquet"),
        loc.join("part-0.parquet"),
    )
    .unwrap();
    let config = root.join("fhir-pipes.toml");
    fs::write(
        &config,
        format!(
            r#"# Literal strings: a Windows path has backslashes.
data = "data"
views = '{views}'
profile = '{profile}'

[server]
base = "{base}"
name = "test"

[tables]
strip_prefix = ["icr_"]

[[partition.lookup]]
key = "location_id"
table = "locations"
table_key = "id"
{extra_config}"#,
            views = fixtures().join("views").display(),
            profile = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/profiles/icr.toml")
                .display()
        ),
    )
    .unwrap();
    (dir, config)
}

/// A server address nothing listens on, for runs that must not reach a server.
const NOWHERE: &str = "http://127.0.0.1:9/fhir";

fn cmd(config: &Path) -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.current_dir(config.parent().unwrap());
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_VIEWS",
        "FHIR_PIPES_SERVER",
        "FHIR_PIPES_SERVER_NAME",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
        "FHIR_PIPES_PROFILE",
    ] {
        c.env_remove(var);
    }
    c.arg("refresh").arg("--config").arg(config);
    c
}

/// Rows of a parquet file as (column → display string) maps.
fn rows(path: &Path) -> Vec<Vec<(String, String)>> {
    let reader = SerializedFileReader::new(fs::File::open(path).unwrap()).unwrap();
    reader
        .get_row_iter(None)
        .unwrap()
        .map(|r| {
            r.unwrap()
                .get_column_iter()
                .map(|(name, field)| {
                    let v = match field {
                        Field::Str(s) => s.clone(),
                        Field::Null => "null".into(),
                        other => other.to_string(),
                    };
                    (name.clone(), v)
                })
                .collect()
        })
        .collect()
}

fn get<'a>(row: &'a [(String, String)], col: &str) -> &'a str {
    &row.iter().find(|(n, _)| n == col).unwrap().1
}

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
// The hooks run through sh.
#[cfg(unix)]
fn hooks_run_after_their_step_only() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
        r#"
[[hooks]]
name = "always"
run = ["sh", "-c", "echo \"$DATA $SERVER_NAME $REFRESHED_AT\" > always.txt"]

[[hooks]]
name = "after-locations"
after = "locations"
run = ["sh", "-c", "touch after-locations.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks"])
        .assert()
        .success();
    let always = fs::read_to_string(dir.path().join("always.txt")).unwrap();
    assert!(always.contains("data test 20"), "{always}");
    assert!(!dir.path().join("after-locations.txt").exists());
}

#[test]
fn missing_tools_fail_before_anything_runs() {
    let (dir, config) = setup(NOWHERE, "\n[tools]\nduckdb = \"/no/such/duckdb\"\n");
    cmd(&config)
        .args(["--only", "views,tiles"])
        .assert()
        .failure()
        .stderr(has("not found: /no/such/duckdb (for tiles)"));
    assert!(!dir.path().join("data/parquet/obs").exists());
}

#[test]
fn push_needs_a_push_section() {
    let (_dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--only", "manifest", "--push"])
        .assert()
        .failure()
        .stderr(has("[push]"));
}

#[test]
fn a_broken_view_stops_the_run() {
    let (dir, config) = setup(NOWHERE, "");
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("bad.json"),
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--views"])
        .arg(&views)
        .assert()
        .failure()
        .stderr(has("bad.json"));
}

/// The fixture resources of `t`, each with a meta.lastUpdated.
fn stamped(t: &str, at: &str) -> Vec<Value> {
    read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")))
        .into_iter()
        .map(|mut r| {
            r["meta"] = json!({"lastUpdated": at});
            r
        })
        .collect()
}

fn full_search(
    t: &str,
) -> impl httptest::matchers::Matcher<httptest::http::Request<httptest::bytes::Bytes>> + use<> {
    all_of![
        request::method_path("GET", format!("/fhir/{t}")),
        request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        request::query(url_decoded(not(contains(key("_summary"))))),
    ]
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

/// Whether the duckdb command is installed; checks that need it are skipped
/// without it.
fn have_duckdb() -> bool {
    std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// One value from a duckdb query, run in `dir`.
fn duckdb_value(dir: &Path, db: Option<&Path>, sql: &str) -> String {
    let mut c = std::process::Command::new("duckdb");
    if let Some(db) = db {
        c.arg(db);
    }
    let mut child = c
        .args(["-noheader", "-list"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), sql.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_catalog_has_a_view_per_table_and_the_extra_views() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
        "\n[catalog]\nextra = \"extra.sql\"\n",
    );
    fs::write(
        dir.path().join("extra.sql"),
        "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n",
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("catalog.sql: 3 table view(s)"));
    let data = dir.path().join("data");
    let sql = fs::read_to_string(data.join("catalog.sql")).unwrap();
    for table in ["locations", "obs", "patient"] {
        assert!(
            sql.contains(&format!(
                "CREATE OR REPLACE VIEW \"{table}\" AS\n  SELECT * FROM read_parquet('parquet/{table}/country=*/**/*.parquet'"
            )),
            "{sql}"
        );
    }
    assert!(sql.contains("CREATE OR REPLACE VIEW obs_sle"), "{sql}");
    if have_duckdb() {
        let n = duckdb_value(
            &data,
            None,
            ".read catalog.sql\nSELECT (SELECT count(*) FROM obs) || ',' || (SELECT count(*) FROM obs_sle);",
        );
        assert_eq!(n, "3,2");
    }
}

#[test]
fn without_a_catalog_section_there_is_no_catalog() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    cmd(&config).args(["--only", "views"]).assert().success();
    assert!(!dir.path().join("data/catalog.sql").exists());
    cmd(&config)
        .args(["--only", "catalog"])
        .assert()
        .failure()
        .stderr(has("--only catalog needs a [catalog] section"));
}

#[test]
fn a_local_catalog_duckdb_is_built_in_the_catalog_step() {
    if !have_duckdb() {
        return;
    }
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(
        &server.url_str("/fhir"),
        "\n[catalog]\nduckdb_file = true\n",
    );
    cmd(&config)
        .args(["--only", "views"])
        .assert()
        .success()
        .stdout(has("   catalog.duckdb"));
    let data = dir.path().join("data");
    assert!(!data.join("catalog.duckdb.tmp").exists());
    // Relative paths: read from the warehouse directory.
    assert_eq!(
        duckdb_value(
            &data,
            Some(Path::new("catalog.duckdb")),
            "SELECT count(*) FROM patient;"
        ),
        "2"
    );
}

/// A fake rclone that copies into $MIRROR as if it were the bucket, and logs
/// each call to $LOG.
#[cfg(unix)]
fn fake_rclone(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rclone");
    fs::write(
        &path,
        r#"#!/bin/sh
echo "$1 $(eval echo \${$#})" >> "$LOG"
if [ "$1" = lsd ]; then echo "          -1 2026-01-01 00:00:00        -1 bkt"; exit 0; fi
for a; do src=$dst; dst=$a; done
rest=${dst#*:}
rest=${rest#*/}
case "$1" in
  sync) mkdir -p "$MIRROR/$rest" && cp -R "$src/." "$MIRROR/$rest/" ;;
  copyto) mkdir -p "$(dirname "$MIRROR/$rest")" && cp "$src" "$MIRROR/$rest" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// With a base_url, catalog.duckdb is built during the push, after the tables
/// are uploaded, and its views read the uploaded copy.
#[test]
#[cfg(unix)]
fn the_pushed_catalog_reads_the_uploaded_tables() {
    let server = Server::run();
    serve_types(&server, &["Observation", "Patient"]);
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let duckdb_file = have_duckdb();
    let (root, config) = setup(
        &server.url_str("/fhir"),
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"manifest.json\", \"catalog.sql\", \"catalog.duckdb\"]\n\n[catalog]\nbase_url = '{}'\nduckdb_file = {duckdb_file}\n",
            rclone.display(),
            mirror.display()
        ),
    );
    let log = dir.path().join("rclone.log");
    cmd(&config)
        .args(["--only", "views", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", &log)
        .assert()
        .success();
    let calls: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let at = |what: &str| {
        calls
            .iter()
            .position(|c| c.ends_with(what))
            .unwrap_or_else(|| panic!("{what} not pushed: {calls:#?}"))
    };
    assert!(at("r:bkt/parquet") < at("r:bkt/catalog.sql"), "{calls:#?}");
    let sql = fs::read_to_string(mirror.join("catalog.sql")).unwrap();
    assert!(
        sql.contains(&format!(
            "read_parquet('{}/parquet/obs/country=*/**/*.parquet'",
            mirror.display()
        )),
        "{sql}"
    );
    if duckdb_file {
        assert!(
            at("r:bkt/catalog.sql") < at("r:bkt/catalog.duckdb"),
            "{calls:#?}"
        );
        // The local tables are gone: the views read the mirror.
        fs::remove_dir_all(root.path().join("data/parquet")).unwrap();
        assert_eq!(
            duckdb_value(
                dir.path(),
                Some(&mirror.join("catalog.duckdb")),
                "SELECT count(*) FROM obs;"
            ),
            "3"
        );
    }
}

/// A relative --data, and a table named after a SQL keyword.
#[test]
fn the_catalog_takes_a_relative_data_dir_and_keyword_table_names() {
    if !have_duckdb() {
        return;
    }
    let (dir, config) = setup(NOWHERE, "\n[catalog]\nduckdb_file = true\n");
    let data = dir.path().join("data");
    let group = data.join("parquet/group/country=SLE");
    fs::create_dir_all(&group).unwrap();
    fs::copy(
        data.join("parquet/locations/country=SLE/part-0.parquet"),
        group.join("part-0.parquet"),
    )
    .unwrap();
    // cmd runs in the config's directory, where data/ is.
    cmd(&config)
        .args(["--data", "data", "--only", "catalog"])
        .assert()
        .success()
        .stdout(has("   catalog.duckdb"));
    assert_eq!(
        duckdb_value(
            &data,
            Some(Path::new("catalog.duckdb")),
            "SELECT (SELECT count(*) FROM \"group\") || ',' || (SELECT count(*) FROM locations);"
        ),
        "2,2"
    );
}

/// Without duckdb_file, a catalog.duckdb listed in push.files (built by a
/// hook, say) is uploaded as any other listed file.
#[test]
#[cfg(unix)]
fn a_listed_catalog_duckdb_is_pushed_when_fhir_pipes_does_not_build_it() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let (root, config) = setup(
        NOWHERE,
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"catalog.duckdb\"]\n\n[catalog]\n",
            rclone.display()
        ),
    );
    fs::write(root.path().join("data/catalog.duckdb"), "built by a hook").unwrap();
    cmd(&config)
        .args(["--only", "catalog", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", dir.path().join("rclone.log"))
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(mirror.join("catalog.duckdb")).unwrap(),
        "built by a hook"
    );
    assert!(mirror.join("catalog.sql").is_file());
}

/// A warehouse with the fixture downloads in raw/, so `--reuse-raw` runs the
/// views without a server.
fn setup_with_raw(extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = setup(NOWHERE, extra_config);
    let raw = dir.path().join("data/raw/test");
    fs::create_dir_all(&raw).unwrap();
    for t in ["Observation", "Patient"] {
        fs::copy(
            fixtures().join(format!("raw/{t}.ndjson")),
            raw.join(format!("{t}.ndjson")),
        )
        .unwrap();
    }
    (dir, config)
}

#[test]
#[cfg(not(feature = "postgres"))]
fn the_database_step_needs_the_postgres_feature() {
    let (_dir, config) = setup_with_raw("\n[database]\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stderr(has("--features postgres"));
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--skip", "database"])
        .assert()
        .success();
}

/// Runs against the PostgreSQL in $FHIR_PIPES_TEST_DATABASE_URL, each test in
/// a schema of its own; skipped without it.
#[cfg(feature = "postgres")]
mod database {
    use super::*;
    use sqlx_core::connection::Connection;
    use sqlx_core::row::Row;
    use sqlx_core::sql_str::AssertSqlSafe;

    const URL: &str = "FHIR_PIPES_TEST_DATABASE_URL";

    fn url() -> Option<String> {
        std::env::var(URL).ok().filter(|u| !u.is_empty())
    }

    /// A schema name no other test uses.
    fn schema(test: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("t_{test}_{}_{nanos}", std::process::id())
    }

    /// Run SQL statements; the rows of the last one, every value as text.
    fn sql(statements: &[&str]) -> Vec<Vec<Option<String>>> {
        let url = url().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut conn = sqlx_postgres::PgConnection::connect(&url).await.unwrap();
                let mut rows = Vec::new();
                for s in statements {
                    rows = sqlx_core::query::query(AssertSqlSafe(s.to_string()))
                        .fetch_all(&mut conn)
                        .await
                        .unwrap_or_else(|e| panic!("{s}: {e}"));
                }
                rows.iter()
                    .map(|r| {
                        (0..r.len())
                            .map(|i| r.try_get::<Option<String>, _>(i).unwrap())
                            .collect()
                    })
                    .collect()
            })
    }

    fn one(statement: &str) -> String {
        sql(&[statement])[0][0].clone().unwrap_or_default()
    }

    #[test]
    fn view_tables_are_loaded_and_replaced() {
        if url().is_none() {
            return;
        }
        let s = schema("load");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("obs → {s}.obs (3 rows)")))
            .stdout(has(format!("patient → {s}.patient (2 rows)")));
        // Only view tables: not locations.
        assert_eq!(
            one(&format!(
                "SELECT string_agg(table_name::text, ',' ORDER BY table_name) FROM information_schema.tables WHERE table_schema = '{s}'"
            )),
            "obs,patient"
        );
        // Types from the Parquet files, values as written.
        assert_eq!(
            one(&format!(
                "SELECT data_type::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'patient' AND column_name = 'born'"
            )),
            "date"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || '=' || born::text || '@' || coalesce(country, 'null'), ',' ORDER BY id) FROM \"{s}\".patient"
            )),
            "p1=2020-05-01@null,p2=2021-01-01@null"
        );

        // A view someone built on a table survives the next run, which
        // replaces the rows.
        sql(&[&format!(
            "CREATE VIEW \"{s}\".patient_ids AS SELECT id FROM \"{s}\".patient"
        )]);
        let raw = dir.path().join("data/raw/test/Patient.ndjson");
        let first = fs::read_to_string(&raw)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        fs::write(&raw, format!("{first}\n")).unwrap();
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("patient → {s}.patient (1 rows)")));
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',') FROM \"{s}\".patient_ids"
            )),
            "p1"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn a_table_with_other_columns_is_recreated() {
        if url().is_none() {
            return;
        }
        let s = schema("recreate");
        sql(&[
            &format!("CREATE SCHEMA \"{s}\""),
            &format!("CREATE TABLE \"{s}\".obs (stale text)"),
        ]);
        let (_dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\ntables = [\"obs\"]\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'obs' AND column_name = 'stale'"
            )),
            "0"
        );
        // `tables` limited the load to obs.
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.tables WHERE table_schema = '{s}' AND table_name = 'patient'"
            )),
            "0"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn database_settings_are_checked() {
        let (_dir, config) = setup_with_raw(
            "\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\ntables = [\"nope\"]\n",
        );
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .failure()
            .stderr(has("`nope` is not a view table (obs, patient)"));
        let (_dir, config) = setup_with_raw("\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\n");
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .env_remove("FHIR_PIPES_NO_SUCH_URL")
            .assert()
            .failure()
            .stderr(has("$FHIR_PIPES_NO_SUCH_URL"));
    }
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
