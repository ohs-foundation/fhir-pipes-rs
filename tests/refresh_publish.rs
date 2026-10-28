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

/// SIGTERM during a success hook stops the hook; the warehouse it ran after
/// stays published.
#[test]
#[cfg(unix)]
fn sigterm_stops_a_running_hook_and_keeps_the_new_warehouse() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "slow"
run = ["sh", "-c", "touch started; sleep 30"]
"#,
    );
    let mut child = spawn_refresh(&config, &["--only", "views,hooks", "--reuse-raw"], &[]);
    wait_for(&dir.path().join("started"));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 15), Some(1));
    let data = dir.path().join("data");
    let r = last_run(&data);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["failed_step"], "hooks");
    assert!(
        r["error"].as_str().unwrap().contains("stopped by signal"),
        "{r}"
    );
    assert_eq!(r["published"], true);
    assert_eq!(r["committed"], false);
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert_nothing_staged(&data);
}

/// SIGTERM while a download is under way stops at the next page: the
/// previous warehouse stays as it was.
#[test]
#[cfg(unix)]
fn sigterm_while_downloading_leaves_the_previous_warehouse() {
    let (dir, config) = setup_with_raw("");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let before = warehouse(&data);
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Patient"))
            .times(..)
            .respond_with(status_code(200).body(bundle(vec![]))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/fhir/Observation"))
            .times(..)
            .respond_with(delay_and_then(
                std::time::Duration::from_secs(4),
                status_code(200).body(bundle(vec![])),
            )),
    );
    let base = server.url_str("/fhir");
    let mut child = spawn_refresh(
        &config,
        &["--only", "views", "--full", "--server", &base],
        &[],
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 10), Some(1));
    let r = last_run(&data);
    assert_eq!(r["failed_step"], "views", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("stopped by signal"),
        "{r}"
    );
    assert_eq!(r["committed"], false);
    assert_eq!(r["published"], false);
    assert!(warehouse(&data) == before, "the warehouse changed");
    assert_nothing_staged(&data);
}

/// A stop that comes once the publish is under way is held back until the
/// publish ends.
#[test]
#[cfg(unix)]
fn sigterm_before_publish_finishes_the_publish() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let mut child = spawn_refresh(
        &config,
        &["--only", "views", "--reuse-raw"],
        &[("FHIR_PIPES_TEST_PAUSE", "before-publish")],
    );
    wait_for(&data.join(".paused"));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 15), Some(1));
    let r = last_run(&data);
    assert!(
        r["error"].as_str().unwrap().contains("stopped by signal"),
        "{r}"
    );
    assert_eq!(r["published"], true);
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert_nothing_staged(&data);
}

/// A second signal does not wait for anything.
#[test]
#[cfg(unix)]
fn a_second_sigterm_exits_at_once() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let mut child = spawn_refresh(
        &config,
        &["--only", "views", "--reuse-raw"],
        &[("FHIR_PIPES_TEST_PAUSE", "before-publish")],
    );
    wait_for(&data.join(".paused"));
    sigterm(&child);
    std::thread::sleep(std::time::Duration::from_millis(100));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 1), Some(1));
}

/// `published` covers the catalog files, not only tables: a catalog-only
/// run whose catalog.duckdb fails has replaced catalog.sql.
#[test]
#[cfg(unix)]
fn only_catalog_failing_at_catalog_duckdb_is_published() {
    use std::os::unix::fs::PermissionsExt;
    let tools = tempfile::tempdir().unwrap();
    let duckdb = tools.path().join("duckdb");
    fs::write(&duckdb, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&duckdb, fs::Permissions::from_mode(0o755)).unwrap();
    let (dir, config) = setup_with_raw(&format!(
        "\n[tools]\nduckdb = '{}'\n\n[catalog]\nduckdb_file = true\n",
        duckdb.display()
    ));
    cmd(&config).args(["--only", "catalog"]).assert().code(1);
    let r = last_run(&dir.path().join("data"));
    assert_eq!(r["published"], true, "{r}");
    assert!(dir.path().join("data/catalog.sql").is_file());
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// FHIR_PIPES_RESULT_FILE gets the run's final record, whether it succeeded,
/// failed at a step, or failed at the lock (which leaves last-run.json to
/// the run that holds it).
#[test]
fn the_result_file_is_written_on_success_failure_and_lock() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    let ok = dir.path().join("ok.json");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .env("FHIR_PIPES_RESULT_FILE", &ok)
        .assert()
        .success();
    let r = read_json(&ok);
    assert_eq!(r["status"], "succeeded");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r, last_run(&data));

    let failed = dir.path().join("failed.json");
    cmd(&config)
        .args(["--only", "views", "--server", NOWHERE])
        .env("FHIR_PIPES_RESULT_FILE", &failed)
        .assert()
        .code(1);
    let r = read_json(&failed);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["failed_step"], "views");
    assert_eq!(r["exit_code"], 1);

    let locked = dir.path().join("locked.json");
    let held = fs::File::create(data.join(".refresh.lock")).unwrap();
    held.lock().unwrap();
    fs::write(data.join("last-run.json"), r#"{"status": "running"}"#).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--lock-timeout", "0"])
        .env("FHIR_PIPES_RESULT_FILE", &locked)
        .assert()
        .code(1);
    let r = read_json(&locked);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["failed_step"], "lock");
    assert_eq!(r["exit_code"], 1);
    assert_eq!(last_run(&data), json!({"status": "running"}));
}

/// FHIR_PIPES_FAILURE_HOOKS=off: no hook runs after a failure; success
/// hooks are as before.
#[test]
#[cfg(unix)]
fn failure_hooks_off_runs_no_hook_after_a_failure() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "ok"
run = ["sh", "-c", "touch ok.txt"]

[[hooks]]
name = "on-failure"
on = "failure"
run = ["sh", "-c", "touch on-failure.txt"]

[[hooks]]
name = "always"
on = "always"
run = ["sh", "-c", "touch always.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks", "--server", NOWHERE])
        .env("FHIR_PIPES_FAILURE_HOOKS", "off")
        .assert()
        .code(1)
        .stdout(has("failure hooks are off (FHIR_PIPES_FAILURE_HOOKS=off)"));
    assert!(!dir.path().join("on-failure.txt").exists());
    assert!(!dir.path().join("always.txt").exists());
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw"])
        .env("FHIR_PIPES_FAILURE_HOOKS", "off")
        .assert()
        .success();
    assert!(dir.path().join("ok.txt").exists());
    assert!(dir.path().join("always.txt").exists());
}

/// Hooks get RUN_ID from FHIR_PIPES_RUN_ID, empty without it.
#[test]
#[cfg(unix)]
fn hooks_get_run_id() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "id"
run = ["sh", "-c", "echo \"[$RUN_ID]\" > id.txt"]
"#,
    );
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw"])
        .env("FHIR_PIPES_RUN_ID", "20261021T020000Z-7f3a")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("id.txt"))
            .unwrap()
            .trim(),
        "[20261021T020000Z-7f3a]"
    );
    cmd(&config)
        .args(["--only", "views,hooks", "--reuse-raw"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("id.txt"))
            .unwrap()
            .trim(),
        "[]"
    );
}

#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An rclone stand-in that lists bucket `b`, then runs `body`.
#[cfg(unix)]
fn rclone(dir: &Path, body: &str) -> PathBuf {
    script(
        dir,
        "rclone",
        &format!(
            "if [ \"$1\" = lsd ]; then echo \"          -1 2026-01-01 00:00:00        -1 b\"; exit 0; fi\n{body}"
        ),
    )
}

#[cfg(unix)]
fn alive(pid: &str) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid.trim().parse().unwrap(), 0) == 0 }
}

/// A push config with a catalog.duckdb built during the push from a bucket
/// URL; `duckdb` and `rclone` are the tools to use, `extra` extra SQL.
#[cfg(unix)]
fn push_config(duckdb: &Path, rclone: &Path, extra: &str) -> String {
    format!(
        "\n[tools]\nduckdb = '{}'\nrclone = '{}'\n\n[push]\nremote = \"r\"\nbucket = \"b\"\n\n[catalog]\nbase_url = \"https://data.example.org/w\"\nduckdb_file = true\n{extra}",
        duckdb.display(),
        rclone.display()
    )
}

/// The catalog.duckdb build is registered before its input is written: a
/// stop reaches a duckdb that never reads it.
#[test]
#[cfg(unix)]
fn sigterm_stops_a_catalog_duckdb_that_does_not_read_its_input() {
    let tools = tempfile::tempdir().unwrap();
    let duckdb = script(
        tools.path(),
        "duckdb",
        "echo $$ > duckdb.pid; exec sleep 30",
    );
    let rclone = rclone(tools.path(), "exit 0");
    let (dir, config) = setup_with_raw(&push_config(&duckdb, &rclone, "extra = \"big.sql\"\n"));
    fs::write(dir.path().join("big.sql"), "-- padding\n".repeat(300_000)).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    let data = dir.path().join("data");
    let mut child = spawn_refresh(&config, &["--only", "push"], &[]);
    wait_for(&data.join("duckdb.pid"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 15), Some(1));
    let pid = fs::read_to_string(data.join("duckdb.pid")).unwrap();
    assert!(!alive(&pid), "duckdb {pid} outlived the run");
}

/// Failure hooks run after a stop: they are the alert.
#[test]
#[cfg(unix)]
fn failure_hooks_run_after_a_stop() {
    let (dir, config) = setup_with_raw(
        r#"
[[hooks]]
name = "slow"
run = ["sh", "-c", "touch started; sleep 30"]

[[hooks]]
name = "alert"
on = "failure"
run = ["sh", "-c", "sleep 1; touch alerted"]
"#,
    );
    let mut child = spawn_refresh(&config, &["--only", "views,hooks", "--reuse-raw"], &[]);
    wait_for(&dir.path().join("started"));
    sigterm(&child);
    assert_eq!(exit_within(&mut child, 15), Some(1));
    assert!(
        dir.path().join("alerted").exists(),
        "the alert hook was stopped"
    );
}

/// A stop reaches a run waiting for the warehouse's lock, with or without a
/// --lock-timeout; the run reports through its result file and leaves
/// last-run.json to the run that holds the lock.
#[test]
#[cfg(unix)]
fn sigterm_while_waiting_for_the_lock_stops() {
    for extra in [&[][..], &["--lock-timeout", "60"][..]] {
        let (dir, config) = setup_with_raw("");
        let data = dir.path().join("data");
        let held = fs::File::create(data.join(".refresh.lock")).unwrap();
        held.lock().unwrap();
        fs::write(data.join("last-run.json"), r#"{"status": "running"}"#).unwrap();
        let result = dir.path().join("result.json");
        let mut args = vec!["--only", "views", "--reuse-raw"];
        args.extend_from_slice(extra);
        let mut child = spawn_refresh(
            &config,
            &args,
            &[("FHIR_PIPES_RESULT_FILE", result.to_str().unwrap())],
        );
        std::thread::sleep(std::time::Duration::from_millis(700));
        sigterm(&child);
        assert_eq!(exit_within(&mut child, 5), Some(1), "{extra:?}");
        let r = read_json(&result);
        assert_eq!(r["failed_step"], "lock", "{extra:?} {r}");
        assert!(
            r["error"].as_str().unwrap().contains("stopped by signal"),
            "{r}"
        );
        assert_eq!(last_run(&data), json!({"status": "running"}));
        drop(held);
    }
}

/// A push that replaces catalog.duckdb has published, even when the upload
/// after it fails.
#[test]
#[cfg(unix)]
fn a_push_that_replaces_catalog_duckdb_is_published() {
    let tools = tempfile::tempdir().unwrap();
    let duckdb = script(tools.path(), "duckdb", "cat > /dev/null; : > \"$1\"");
    let rclone = rclone(
        tools.path(),
        "case \"$*\" in *catalog.duckdb*) exit 1;; esac; exit 0",
    );
    let (dir, config) = setup_with_raw(&push_config(&duckdb, &rclone, ""));
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
    cmd(&config).args(["--only", "push"]).assert().code(1);
    let data = dir.path().join("data");
    let r = last_run(&data);
    assert!(data.join("catalog.duckdb").is_file(), "{r}");
    assert_eq!(r["failed_step"], "push", "{r}");
    assert_eq!(r["published"], true, "{r}");
}

/// A publish whose first replacement fails has changed nothing live.
#[test]
#[cfg(unix)]
fn a_failed_first_replacement_is_not_published() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    fs::create_dir_all(data.join("manifest.json/x")).unwrap();
    cmd(&config).args(["--only", "manifest"]).assert().code(1);
    let r = last_run(&data);
    assert_eq!(r["failed_step"], "publish", "{r}");
    assert_eq!(r["published"], false, "{r}");
}

/// A run that cannot write last-run.json at its start still reports to its
/// supervisor.
#[test]
fn the_result_file_is_written_when_last_run_cannot_be() {
    let (dir, config) = setup_with_raw("");
    let data = dir.path().join("data");
    fs::create_dir_all(data.join("last-run.json/x")).unwrap();
    let result = dir.path().join("result.json");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .env("FHIR_PIPES_RESULT_FILE", &result)
        .assert()
        .code(1);
    let r = read_json(&result);
    assert_eq!(r["status"], "failed", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("last-run.json"),
        "{r}"
    );
}

/// A token the server echoes in an error body is hidden before the body is
/// cut to its 500 characters, so no part of it is left in the run's record.
#[test]
fn a_token_echoed_across_the_error_cut_is_hidden() {
    const TOKEN: &str = "tok-0123456789abcdefghijklmnopqrs";
    let server = Server::run();
    let body = format!("{}{TOKEN}", "e".repeat(469));
    server.expect(
        Expectation::matching(any())
            .times(..)
            .respond_with(status_code(401).body(body)),
    );
    let (dir, config) = setup(&server.url_str("/fhir"), "");
    cmd(&config)
        .args(["--only", "views"])
        .env("FHIR_PIPES_TOKEN", TOKEN)
        .assert()
        .code(1);
    let text = fs::read_to_string(dir.path().join("data/last-run.json")).unwrap();
    for n in 4..=TOKEN.len() {
        assert!(
            !text.contains(&TOKEN[..n]),
            "{} characters shown: {text}",
            n
        );
    }
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
            "location_hierarchy",
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
    assert_eq!(
        tables,
        ["location_hierarchy", "locations", "obs", "patient"]
    );
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
