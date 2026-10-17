//! End-to-end runs of `fhir-pipes refresh`: the catalog step (catalog.sql,
//! catalog.duckdb, the Spark catalog) and the push.

mod common;

use std::fs;
use std::io::Write as _;
use std::path::Path;

use httptest::Server;
use predicates::str::contains as has;

use common::*;

#[test]
fn push_needs_a_push_section() {
    let (_dir, config) = setup(NOWHERE, "");
    cmd(&config)
        .args(["--only", "manifest", "--push"])
        .assert()
        .failure()
        .stderr(has("[push]"));
}

/// Whether the duckdb command is installed. Without it the checks that need
/// it are skipped, and `skipped` (the test, and the part of it when only a
/// part needs duckdb) is named on stderr. The line is written to stderr
/// directly, which the test harness does not capture, so a plain `cargo
/// test` shows it.
fn have_duckdb(skipped: &str) -> bool {
    let found = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
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
    if have_duckdb("the_catalog_has_a_view_per_table_and_the_extra_views: the query of the views") {
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
fn a_local_catalog_duckdb_is_built_once_the_tables_are_in_place() {
    if !have_duckdb("a_local_catalog_duckdb_is_built_once_the_tables_are_in_place") {
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
    let duckdb_file =
        have_duckdb("the_pushed_catalog_reads_the_uploaded_tables: the pushed catalog.duckdb");
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
    if !have_duckdb("the_catalog_takes_a_relative_data_dir_and_keyword_table_names") {
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

/// With spark = true, catalog.spark.sql has a table for every table, typed
/// from its files, empty ones and keyword names included.
#[test]
fn the_spark_catalog_has_a_table_per_table() {
    let (dir, config) =
        setup_with_raw("\n[catalog]\nspark = true\nspark_extra = \"spark-extra.sql\"\n");
    fs::write(
        dir.path().join("spark-extra.sql"),
        "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n",
    )
    .unwrap();
    let views = views_with(dir.path(), &["IcrGroup"], true);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&views)
        .assert()
        .success()
        .stdout(has("catalog.spark.sql: 5 table(s) in database fhir"));
    let data = dir.path().join("data");
    let sql = fs::read_to_string(data.join("catalog.spark.sql")).unwrap();
    assert!(
        sql.contains("CREATE DATABASE IF NOT EXISTS `fhir`;"),
        "{sql}"
    );
    for table in ["empty", "group", "locations", "obs", "patient"] {
        assert!(
            sql.contains(&format!(
                "DROP TABLE IF EXISTS `fhir`.`{table}`;\nCREATE TABLE `fhir`.`{table}` ("
            )),
            "{sql}"
        );
    }
    // The empty view's one empty file still gives its columns.
    assert!(
        sql.contains(
            "CREATE TABLE `fhir`.`empty` (\n  `id` STRING,\n  `born` DATE,\n  `country` STRING\n)"
        ),
        "{sql}"
    );
    assert!(sql.contains("  `issued` TIMESTAMP,"), "{sql}");
    // The default location is the warehouse directory (the config's
    // directory resolved, as `data` is relative to it).
    #[cfg(unix)]
    assert!(
        sql.contains(&format!(
            "LOCATION 'file://{}/parquet/obs';",
            data.canonicalize().unwrap().display()
        )),
        "{sql}"
    );
    assert!(
        sql.ends_with(
            "CREATE OR REPLACE VIEW obs_sle AS SELECT * FROM obs WHERE country = 'SLE';\n"
        ),
        "{sql}"
    );
}

/// catalog.spark.sql is pushed with the other catalog files, after the
/// tables, once even when push.files lists it.
#[test]
#[cfg(unix)]
fn the_spark_catalog_is_pushed_after_the_tables() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    let rclone = fake_rclone(dir.path());
    let (_root, config) = setup(
        NOWHERE,
        &format!(
            "\n[tools]\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"bkt\"\nfiles = [\"manifest.json\", \"catalog.spark.sql\"]\n\n[catalog]\nspark = true\nspark_location = \"s3a://bkt\"\nspark_database = \"analytics\"\n",
            rclone.display()
        ),
    );
    let log = dir.path().join("rclone.log");
    cmd(&config)
        .args(["--only", "catalog", "--push"])
        .env("MIRROR", &mirror)
        .env("LOG", &log)
        .assert()
        .success()
        .stdout(has("catalog.spark.sql: 1 table(s) in database analytics"));
    let calls: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let pushed: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ends_with("r:bkt/catalog.spark.sql"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(pushed.len(), 1, "{calls:#?}");
    let at = |what: &str| calls.iter().position(|c| c.ends_with(what)).unwrap();
    assert!(at("r:bkt/parquet") < pushed[0], "{calls:#?}");
    assert!(at("r:bkt/catalog.sql") < pushed[0], "{calls:#?}");
    let sql = fs::read_to_string(mirror.join("catalog.spark.sql")).unwrap();
    assert!(
        sql.contains("LOCATION 's3a://bkt/parquet/locations';"),
        "{sql}"
    );
}
