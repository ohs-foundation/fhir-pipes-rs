//! End-to-end runs of `fhir-pipes refresh`: staging and publishing a run, the
//! tools it needs, hooks, and what a failure leaves behind.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use httptest::{Expectation, Server, matchers::*, responders::*};
use predicates::str::contains as has;
use serde_json::{Value, json};

use common::*;

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

/// Drop the last Observation, so the next run would write a different obs table.
fn drop_an_observation(root: &Path) {
    let raw = root.join("data/raw/test/Observation.ndjson");
    let text = fs::read_to_string(&raw).unwrap();
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    fs::write(&raw, kept[..kept.len() - 1].join("\n") + "\n").unwrap();
}

/// A view whose table fails to write (here its lookup table cannot be read)
/// leaves the tables written before it, the manifest and the catalog as the
/// previous run left them.
#[test]
fn a_failing_view_leaves_every_table_as_it_was() {
    let (dir, config) =
        setup_with_raw("\n[[partition.lookup]]\nkey = \"id\"\ntable = \"bad\"\n\n[catalog]\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let before = warehouse(&data);
    assert!(before.contains_key("parquet/obs/country=SLE/part-0.parquet"));

    // obs is written first and would change; patient then looks its partition
    // up in a table that cannot be read.
    drop_an_observation(dir.path());
    fs::create_dir_all(data.join("parquet/bad/country=SLE")).unwrap();
    fs::write(
        data.join("parquet/bad/country=SLE/part-0.parquet"),
        "not parquet",
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stdout(has("parquet/obs (2 rows)"))
        .stderr(has("reading lookup table bad"));
    let mut after = warehouse(&data);
    after.remove("parquet/bad/country=SLE/part-0.parquet");
    assert_eq!(paths(&after), paths(&before));
    assert!(after == before, "the warehouse changed");
    assert_nothing_staged(&data);
}

/// A catalog step that fails (here `extra` names a missing file) leaves the
/// tables, the manifest and catalog.sql as the previous run left them.
#[test]
fn a_failing_catalog_leaves_the_tables_and_manifest_as_they_were() {
    let (dir, config) = setup_with_raw("\n[catalog]\nextra = \"extra.sql\"\n");
    let extra = dir.path().join("extra.sql");
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let before = warehouse(&data);
    assert!(before.contains_key("manifest.json"));
    assert!(before.contains_key("catalog.sql"));

    drop_an_observation(dir.path());
    fs::remove_file(&extra).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stdout(has("parquet/obs (2 rows)"))
        .stderr(has("extra.sql"));
    let after = warehouse(&data);
    assert_eq!(paths(&after), paths(&before));
    assert!(after == before, "the warehouse changed");
    assert_nothing_staged(&data);

    // Once the catalog can be written, the run publishes the new tables.
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 2);
}

/// Hooks run after the swap, on the published warehouse: a hook that fails
/// stops the run before the push, with the new tables, manifest and catalog
/// in place.
#[test]
#[cfg(unix)]
fn a_failing_hook_sees_the_published_warehouse_and_stops_the_push() {
    let tools = tempfile::tempdir().unwrap();
    let rclone = fake_rclone(tools.path());
    let log = tools.path().join("rclone.log");
    let (dir, config) = setup_with_raw(&format!(
        r#"
[tools]
rclone = '{}'

[push]
remote = "r"
bucket = "bkt"

[catalog]

[[hooks]]
name = "fails"
run = ["sh", "-c", "cp \"$DATA/manifest.json\" seen.json; exit 3"]
"#,
        rclone.display()
    ));
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw", "--push"])
        .env("MIRROR", tools.path().join("mirror"))
        .env("LOG", &log)
        .assert()
        .failure()
        .stderr(has("hook fails"));
    let data = dir.path().join("data");
    let seen = fs::read_to_string(dir.path().join("seen.json")).unwrap();
    assert_eq!(
        seen,
        fs::read_to_string(data.join("manifest.json")).unwrap()
    );
    let manifest: Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert!(data.join("catalog.sql").is_file());
    assert!(!log.exists(), "pushed after a failing hook");
    assert_nothing_staged(&data);
}

fn last_run(data: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(data.join("last-run.json")).unwrap()).unwrap()
}

/// A run that succeeds records it in last-run.json, and its hooks get
/// STATUS; hooks with `on = "failure"` do not run.
#[test]
#[cfg(unix)]
fn a_run_that_succeeds_is_recorded_in_last_run() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "ok"
run = ["sh", "-c", "echo \"$STATUS $REFRESHED_AT\" > ok.txt"]

[[hooks]]
name = "on-failure"
on = "failure"
run = ["sh", "-c", "touch on-failure.txt"]

[[hooks]]
name = "missing"
on = "failure"
run = ["/no/such/alert"]
"#,
    );
    // A failure hook's program is not needed for a run that succeeds.
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw"])
        .assert()
        .success();
    let r = last_run(&dir.path().join("data"));
    assert_eq!(r["status"], "succeeded");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["server_name"], "test");
    assert_eq!(r["steps"], json!(["views", "manifest", "hooks"]));
    assert!(r["timings"]["views"].is_number(), "{r}");
    assert!(r.get("failed_step").is_none());
    let ok = fs::read_to_string(dir.path().join("ok.txt")).unwrap();
    assert_eq!(
        ok.trim(),
        format!("succeeded {}", r["refreshed_at"].as_str().unwrap())
    );
    assert!(!dir.path().join("on-failure.txt").exists());
}

/// A run that fails records the step and the error in last-run.json, and
/// runs the hooks with `on = "failure"` or `"always"` (not the others),
/// which get FAILED_STEP, ERROR and EXIT_CODE. A failure hook that fails is
/// a warning: the run still fails with its own error.
#[test]
#[cfg(unix)]
fn a_run_that_fails_is_recorded_and_runs_the_failure_hooks() {
    let (dir, config) = setup(
        NOWHERE,
        r#"
[[hooks]]
name = "on-success"
run = ["sh", "-c", "touch on-success.txt"]

[[hooks]]
name = "on-failure"
on = "failure"
run = ["sh", "-c", "echo \"$STATUS|$FAILED_STEP|$EXIT_CODE|$ERROR\" > on-failure.txt; cp \"$DATA/last-run.json\" seen.json"]

[[hooks]]
name = "broken"
on = "failure"
run = ["sh", "-c", "exit 4"]

[[hooks]]
name = "missing"
on = "failure"
run = ["/no/such/alert"]

[[hooks]]
name = "always"
on = "always"
run = ["sh", "-c", "echo \"$STATUS\" > always.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks"])
        .assert()
        .code(1)
        .stderr(has("warning: hook broken"))
        .stderr(has("warning: hook missing"))
        .stderr(has("fhir-pipes: views"));
    let r = last_run(&dir.path().join("data"));
    assert_eq!(r["status"], "failed");
    assert_eq!(r["failed_step"], "views");
    assert_eq!(r["exit_code"], 1);
    let error = r["error"].as_str().unwrap();
    assert!(error.starts_with("views: "), "{error}");
    let seen = fs::read_to_string(dir.path().join("on-failure.txt")).unwrap();
    assert_eq!(seen.trim(), format!("failed|views|1|{error}"));
    // The hook ran while the run still held the lock, so the record it read
    // was this run's.
    let copied: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("seen.json")).unwrap()).unwrap();
    assert_eq!(copied, r);
    assert_eq!(
        fs::read_to_string(dir.path().join("always.txt"))
            .unwrap()
            .trim(),
        "failed"
    );
    assert!(!dir.path().join("on-success.txt").exists());
    assert_nothing_staged(&dir.path().join("data"));
}

/// last-run.json names the server as the manifest does, without a query
/// string that could carry a key.
#[test]
fn last_run_redacts_the_server_url() {
    let (dir, config) = setup(&format!("{NOWHERE}?api_key=secret"), "");
    cmd(&config).args(["--only", "hooks"]).assert().success();
    let text = fs::read_to_string(dir.path().join("data/last-run.json")).unwrap();
    assert!(!text.contains("secret"), "{text}");
    assert_eq!(last_run(&dir.path().join("data"))["status"], "succeeded");
}

/// With --lock-timeout, a run that cannot get the warehouse's lock fails,
/// leaves last-run.json to the run that holds it, and runs the failure
/// hooks with FAILED_STEP=lock.
#[test]
#[cfg(unix)]
fn a_run_that_cannot_get_the_lock_in_time_fails() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "on-failure"
on = "failure"
run = ["sh", "-c", "echo \"$FAILED_STEP\" > on-failure.txt"]
"#,
    );
    let data = dir.path().join("data");
    let held = fs::File::create(data.join(".refresh.lock")).unwrap();
    held.lock().unwrap();
    fs::write(data.join("last-run.json"), r#"{"status": "running"}"#).unwrap();
    cmd(&config)
        .args([
            "--only",
            "views,hooks",
            "--reuse-raw",
            "--lock-timeout",
            "0",
        ])
        .assert()
        .code(1)
        .stderr(has("still holds .refresh.lock after 0 s (--lock-timeout)"));
    assert_eq!(last_run(&data), json!({"status": "running"}));
    assert_eq!(
        fs::read_to_string(dir.path().join("on-failure.txt"))
            .unwrap()
            .trim(),
        "lock"
    );
    drop(held);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--lock-timeout", "0"])
        .assert()
        .success();
    assert_eq!(last_run(&data)["status"], "succeeded");
}

/// A successful run leaves the files it always did, and nothing staged.
#[test]
fn a_successful_run_publishes_every_table_and_leaves_nothing_staged() {
    let server = Server::run();
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot");
    let locations: Vec<Value> = read_ndjson(&snapshot.join("locations.ndjson"))
        .into_iter()
        .map(|mut l| {
            l["meta"] = json!({"lastUpdated": "2026-01-01T00:00:00Z"});
            l
        })
        .collect();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Location"))
            .times(1)
            .respond_with(status_code(200).body(bundle(locations))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Organization"))
            .times(1)
            .respond_with(status_code(200).body(bundle(vec![]))),
    );
    serve_types(&server, &["Observation", "Patient"]);
    let (dir, config) = setup(&server.url_str("/fhir"), "\n[catalog]\n");
    cmd(&config)
        .args(["--only", "locations,views", "--full"])
        .assert()
        .success()
        .stdout(has("== publish"));
    let data = dir.path().join("data");
    let mut files = warehouse(&data);
    // The one new file: the lock a run holds on the warehouse.
    assert!(files.remove(".refresh.lock").is_some());
    let tops: std::collections::BTreeSet<&str> = files
        .keys()
        .map(|k| {
            let mut it = k.split('/');
            match it.next().unwrap() {
                "parquet" => k.splitn(3, '/').take(2).last().unwrap(),
                top => top,
            }
        })
        .collect();
    assert_eq!(
        tops.into_iter().collect::<Vec<_>>(),
        [
            "_report.json",
            "catalog.sql",
            "locations",
            "manifest.json",
            "obs",
            "patient",
            "views"
        ],
        "{:#?}",
        paths(&files)
    );
    assert!(
        files
            .keys()
            .all(|k| !k.split('/').any(|c| c.starts_with('.'))),
        "{:#?}",
        paths(&files)
    );
    // The fixture's country=SLE table was replaced by the extracted registry.
    assert!(!data.join("parquet/locations/country=SLE").exists());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    let tables: Vec<&String> = manifest["tables"].as_object().unwrap().keys().collect();
    assert_eq!(tables, ["locations", "obs", "patient"]);
    assert!(manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 2);
    let sql = fs::read_to_string(data.join("catalog.sql")).unwrap();
    assert!(
        sql.contains("read_parquet('parquet/locations/country=*/**/*.parquet'"),
        "{sql}"
    );
    assert!(!sql.contains(".staging"), "{sql}");
}

/// What a killed run left behind is cleaned up at the start of the next: its
/// staging directories, and a table a swap left only as `.<table>.old`.
#[test]
fn a_killed_run_is_cleaned_up_by_the_next() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let junk = data.join("parquet/.staging/obs/country=XXX");
    fs::create_dir_all(&junk).unwrap();
    fs::write(junk.join("part-0.parquet"), "half written").unwrap();
    fs::create_dir_all(data.join(".staging")).unwrap();
    fs::write(data.join(".staging/manifest.json"), "{}").unwrap();
    fs::rename(
        data.join("parquet/locations"),
        data.join("parquet/.locations.old"),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has("restored parquet/locations"));
    assert_nothing_staged(&data);
    assert!(!data.join("parquet/.locations.old").exists());
    // The restored registry was there for the views' partition lookups.
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert!(!data.join("parquet/obs/country=XXX").exists());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["locations"]["rows"], 2);
    assert_eq!(manifest["tables"]["obs"]["rows"], 3);
}

/// A relative --data is resolved against the current directory, and the hooks,
/// which run in the config file's directory, get it as an absolute DATA.
#[test]
#[cfg(unix)]
fn hooks_get_an_absolute_data_directory() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "seen"
run = ["sh", "-c", "echo \"$DATA\" > data.txt; cp \"$DATA/manifest.json\" seen.json"]
"#,
    );
    let root = dir.path();
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    cmd(&config)
        .current_dir(&work)
        .args(["--data", "../data", "--only", "views,hooks", "--reuse-raw"])
        .assert()
        .success();
    let data = fs::read_to_string(root.join("data.txt")).unwrap();
    assert!(Path::new(data.trim()).is_absolute(), "{data}");
    assert_eq!(
        fs::read_to_string(root.join("seen.json")).unwrap(),
        fs::read_to_string(root.join("data/manifest.json")).unwrap()
    );
}

/// A hook's `after` names a step in any case; `push` and `hooks`, which no
/// hook can run after, are refused before anything runs.
#[test]
#[cfg(unix)]
fn a_hooks_after_matches_its_step_in_any_case() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "after-views"
after = "Views"
run = ["sh", "-c", "touch after-views.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw"])
        .assert()
        .success();
    assert!(dir.path().join("after-views.txt").exists());

    for step in ["push", "hooks"] {
        let (dir, config) = setup_with_raw(&format!(
            "\n[[hooks]]\nname = \"h\"\nafter = \"{step}\"\nrun = [\"true\"]\n"
        ));
        cmd(&config)
            .args(["--only", "views,hooks", "--reuse-raw"])
            .assert()
            .failure()
            .stderr(has(format!(
                "`after = \"{step}\"`: a hook runs after one of"
            )));
        assert!(!dir.path().join("data/parquet/obs").exists());
    }
}

/// A fake duckdb that writes one feature to the file a COPY names, and a
/// fake tippecanoe that writes $TILES to its output.
#[cfg(unix)]
fn fake_tile_tools(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let duckdb = dir.join("duckdb");
    fs::write(
        &duckdb,
        r#"#!/bin/sh
file=$(printf '%s\n' "$2" | sed -n "s/.*TO '\([^']*\)'.*/\1/p" | tail -n 1)
[ -n "$file" ] && echo '{"type":"Feature"}' > "$file"
exit 0
"#,
    )
    .unwrap();
    let tippecanoe = dir.join("tippecanoe");
    fs::write(
        &tippecanoe,
        r#"#!/bin/sh
while [ $# -gt 0 ]; do [ "$1" = -o ] && out=$2; shift; done
printf '%s' "$TILES" > "$out"
"#,
    )
    .unwrap();
    for p in [&duckdb, &tippecanoe] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (duckdb, tippecanoe)
}

/// tiles/ is staged and replaced with the tables: a run that fails after the
/// tiles step keeps the previous tiles.
#[test]
#[cfg(unix)]
fn tiles_are_swapped_in_with_the_tables() {
    let tools = tempfile::tempdir().unwrap();
    let (duckdb, tippecanoe) = fake_tile_tools(tools.path());
    let (dir, config) = setup_with_raw(&format!(
        "\n[tools]\nduckdb = '{}'\ntippecanoe = '{}'\n\n[catalog]\nextra = \"extra.sql\"\n",
        duckdb.display(),
        tippecanoe.display()
    ));
    let data = dir.path().join("data");
    let admin = data.join("parquet/locations/country=SLE/geom_type=polygon/type=admin-unit");
    fs::create_dir_all(&admin).unwrap();
    // The tiles step lists the files it reads (the fake duckdb reads none);
    // the views step's lookup reads this one too, so it is a real table.
    fs::copy(
        data.join("parquet/locations/country=SLE/part-0.parquet"),
        admin.join("part-0.parquet"),
    )
    .unwrap();
    fs::create_dir_all(data.join("tiles")).unwrap();
    fs::write(data.join("tiles/admin.pmtiles"), "old").unwrap();
    fs::write(data.join("tiles/catchments.pmtiles"), "old").unwrap();

    // The catalog step fails after the tiles are built.
    cmd(&config)
        .args(["--only", "views,tiles", "--reuse-raw"])
        .env("TILES", "new")
        .assert()
        .failure()
        .stdout(has("→ admin.pmtiles"))
        .stderr(has("extra.sql"));
    assert_eq!(
        fs::read_to_string(data.join("tiles/admin.pmtiles")).unwrap(),
        "old"
    );
    assert!(data.join("tiles/catchments.pmtiles").is_file());
    assert_nothing_staged(&data);

    fs::write(dir.path().join("extra.sql"), "").unwrap();
    cmd(&config)
        .args(["--only", "views,tiles", "--reuse-raw"])
        .env("TILES", "new")
        .assert()
        .success()
        .stdout(has(", tiles/, views/, then the manifest"));
    assert_eq!(
        fs::read_to_string(data.join("tiles/admin.pmtiles")).unwrap(),
        "new"
    );
    // The registry has no catchments now: the old archive is gone with the
    // rest of the old tiles/.
    assert!(!data.join("tiles/catchments.pmtiles").exists());
    assert!(!data.join(".tiles.old").exists());
    assert_nothing_staged(&data);
}

/// The counts are printed before the hooks run, so a failing hook does not
/// hide them once the tables with the nulls are published. last-refresh.json
/// is written only by a run that finishes, as before.
#[test]
#[cfg(unix)]
fn a_failing_hook_does_not_hide_the_mismatch_counts() {
    let (dir, config) = setup(
        NOWHERE,
        r#"
[[hooks]]
name = "fails"
after = "views"
run = ["sh", "-c", "exit 1"]
"#,
    );
    let root = dir.path();
    let (views, input) = mismatch_setup(root);
    cmd(&config)
        .args(["--only", "views,hooks", "--input"])
        .arg(&input)
        .arg("--views")
        .arg(&views)
        .assert()
        .failure()
        .stdout(has("== publish"))
        .stdout(has(
            "measures.n: 2 values did not fit integer and were written as null",
        ))
        .stdout(has(
            "measures.period_start: 2 dateTime values written as their date",
        ));
    // The tables were published before the hook failed.
    let rows = table_rows(&root.join("data/parquet/measures"));
    assert_eq!(rows.len(), 3);
    assert!(!root.join("data/raw/test/last-refresh.json").exists());
}
