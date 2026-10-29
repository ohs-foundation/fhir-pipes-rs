//! End-to-end runs of `fhir-pipes serve`: the command, its config, the API,
//! runs and their attempts, notify, the schedule, restart and shutdown.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::serve::*;
use common::*;

fn data(config: &Path) -> PathBuf {
    config.parent().unwrap().join("data")
}

#[test]
fn refresh_ignores_the_serve_section() {
    let (_dir, config) = setup_with_raw("\n[serve]\nretries = 1\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success();
}

#[test]
fn listen_on_port_0_prints_the_address() {
    let (_dir, config) = setup_with_raw("");
    let s = start_serve(&config, &[]);
    let port: u16 = s.url.rsplit(':').next().unwrap().parse().unwrap();
    assert_ne!(port, 0, "{}", s.url);
    assert!(s.url.starts_with("http://127.0.0.1:"), "{}", s.url);
}

#[test]
fn serve_creates_the_data_directory() {
    let (dir, config) = setup_with_raw("");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("data = \"data\"", "data = \"new/data\"");
    fs::write(&config, text).unwrap();
    let s = start_serve(&config, &[]);
    let r = client().get(format!("{}/health", s.url)).send().unwrap();
    assert_eq!(r.status(), 200);
    assert!(dir.path().join("new/data/.serve").is_dir());
}

#[test]
fn health_answers_without_a_token() {
    let (_dir, config) = setup_with_raw("");
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", "a-long-token")]);
    let c = client();
    assert_eq!(
        c.get(format!("{}/health", s.url)).send().unwrap().status(),
        200
    );
    assert_eq!(
        c.get(format!("{}/status", s.url)).send().unwrap().status(),
        401
    );
    assert_eq!(
        c.get(format!("{}/status", s.url))
            .bearer_auth("wrong-token")
            .send()
            .unwrap()
            .status(),
        401
    );
}

#[test]
fn no_token_and_a_public_address_refuses_to_start() {
    let (_dir, config) = setup_with_raw("");
    for env in [&[][..], &[("FHIR_PIPES_SERVE_TOKEN", "")][..]] {
        let (code, stderr) = serve_fails(&config, "0.0.0.0:0", env);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains("FHIR_PIPES_SERVE_TOKEN"), "{stderr}");
    }
}

#[test]
fn a_second_serve_on_the_warehouse_fails() {
    let (_dir, config) = setup_with_raw("");
    let _first = start_serve(&config, &[]);
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains(".serve/serve.lock"), "{stderr}");
}

#[test]
fn a_notify_url_with_a_password_is_a_config_error() {
    let (_dir, config) = setup_with_raw(
        "\n[[serve.notify]]\nname = \"openfn\"\nurl = \"https://user:pw@openfn.example.org/i/1\"\n",
    );
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("openfn"), "{stderr}");
    assert!(!stderr.contains("pw@"), "{stderr}");
}

#[test]
fn a_bad_cron_is_a_config_error() {
    let (_dir, config) =
        setup_with_raw("\n[[serve.schedule]]\ncron = \"every day\"\nmode = \"full\"\n");
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("every day"), "{stderr}");
}

// ---- The API, runs and notify (Task B6) ----

use httptest::{Expectation, Server as HttpServer, matchers::*, responders::*};
use serde_json::Value;

/// serve with the token, reading the fixture downloads (FHIR_PIPES_REUSE_RAW).
fn start_api(config: &Path, env: &[(&str, &str)]) -> Serve {
    let mut all = vec![
        ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
        ("FHIR_PIPES_REUSE_RAW", "true"),
    ];
    all.extend_from_slice(env);
    start_serve(config, &all)
}

fn notify_config(server: &HttpServer) -> String {
    format!(
        "\n[[serve.notify]]\nname = \"openfn\"\nurl = \"{}\"\n",
        server.url_str("/i")
    )
}

/// Wait until run `id` has ended and its destinations are no longer
/// pending.
fn delivered(s: &Serve, id: &str) -> Value {
    s.finished(id);
    let started = Instant::now();
    loop {
        let r = s.run(id);
        if r["notify"]
            .as_object()
            .is_none_or(|n| n.values().all(|d| d["state"] != "pending"))
        {
            return r;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "{r}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn post_run_answers_202_after_the_record_exists() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    let (status, v) = s.post("/run", Some(r#"{"only": ["views"]}"#));
    assert_eq!(status, 202, "{v}");
    let id = v["id"].as_str().unwrap();
    assert_eq!(v["status_url"], format!("/runs/{id}"));
    assert!(
        data(&config)
            .join(format!(".serve/runs/{id}/run.json"))
            .is_file()
    );
    let r = s.finished(id);
    assert_eq!(r["status"], "succeeded", "{r}");
    assert_eq!(r["attempts"].as_array().unwrap().len(), 1);
    assert!(
        data(&config)
            .join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
}

#[test]
fn an_empty_post_starts_an_incremental_run() {
    // The full refresh fails at locations (no server); not retried here.
    let (_dir, config) = setup_with_raw("\n[serve]\nretries = 0\n");
    let s = start_api(&config, &[]);
    let (status, v) = s.post("/run", None);
    assert_eq!(status, 202, "{v}");
    let r = s.finished(v["id"].as_str().unwrap());
    assert_eq!(r["request"]["mode"], "incremental", "{r}");
    assert_eq!(r["trigger"], "api");
}

#[test]
fn a_second_post_is_409_with_the_current_id() {
    let (dir, config) = setup_with_raw(
        "\n[[hooks]]\nname = \"slow\"\nrun = [\"sh\", \"-c\", \"touch started; sleep 3\"]\n",
    );
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views", "hooks"]}"#);
    wait_for(&dir.path().join("started"));
    let (status, v) = s.post("/run", Some("{}"));
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["current"], id);
    s.finished(&id);
}

#[test]
fn an_unknown_step_is_400() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    for body in [
        r#"{"only": ["vews"]}"#,
        r#"{"mode": "partial"}"#,
        r#"{"extra": 1}"#,
        "not json",
    ] {
        let (status, v) = s.post("/run", Some(body));
        assert_eq!(status, 400, "{body}: {v}");
    }
}

#[test]
fn an_unwritable_runs_directory_is_503_and_starts_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    let runs = data(&config).join(".serve/runs");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o500)).unwrap();
    let (status, _) = s.post("/run", Some("{}"));
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, 503);
    assert_eq!(fs::read_dir(&runs).unwrap().count(), 0);
}

#[test]
fn the_token_is_checked_before_the_body() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    let r = client()
        .post(format!("{}/run", s.url))
        .body("x".repeat(20_000))
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);
    let (status, _) = s.post(
        "/run",
        Some(&format!("{{\"only\": [\"{}\"]}}", "x".repeat(20_000))),
    );
    assert_eq!(status, 413);
}

#[test]
fn a_bad_run_id_is_404() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    for path in [
        "/runs/nonsense",
        "/runs/..%2F..%2Fetc",
        "/runs/20261021T020000Z-7f3a",
    ] {
        assert_eq!(s.get(path).0, 404, "{path}");
    }
}

#[test]
fn runs_and_log_are_served() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views"]}"#);
    s.finished(&id);
    let (status, text) = s.get("/runs");
    assert_eq!(status, 200);
    let runs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(runs[0]["id"], id);
    assert!(
        runs[0]["attempts"][0]
            .get("result")
            .is_none_or(Value::is_null),
        "{runs}"
    );
    let (status, log) = s.get(&format!("/runs/{id}/log"));
    assert_eq!(status, 200);
    assert!(log.contains("== views"), "{log}");
    let (_, tail) = s.get(&format!("/runs/{id}/log?tail=2"));
    assert_eq!(tail.lines().count(), 2, "{tail}");
    let (status, text) = s.get("/status");
    assert_eq!(status, 200);
    let st: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(st["last"]["id"], id);
    assert!(st["current"].is_null());
}

#[test]
fn cancel_stops_a_running_child_and_its_hook_writer() {
    let notify = HttpServer::run();
    notify.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/i"),
            request::body(json_decoded(json_value_status("cancelled"))),
        ])
        .times(1)
        .respond_with(status_code(200)),
    );
    let (dir, config) = setup_with_raw(&format!(
        "\n[[hooks]]\nname = \"w\"\nrun = [\"sh\", \"-c\", \"sleep 30 & echo $! > w.pid; wait\"]\n{}",
        notify_config(&notify)
    ));
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views", "hooks"]}"#);
    let pid_file = dir.path().join("w.pid");
    // The file appears before the shell writes the pid into it.
    let started = Instant::now();
    while fs::read_to_string(&pid_file).map_or(true, |p| p.trim().is_empty()) {
        assert!(started.elapsed() < Duration::from_secs(20), "no w.pid");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, _) = s.post(&format!("/runs/{id}/cancel"), None);
    assert_eq!(status, 200);
    let r = s.finished(&id);
    assert_eq!(r["status"], "cancelled", "{r}");
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the hook's writer outlived the cancel"
    );
    delivered(&s, &id);
    let (status, _) = s.post(&format!("/runs/{id}/cancel"), None);
    assert_eq!(status, 409);
}

/// Matches a notify body whose `status` is the one given.
struct StatusIs(&'static str);

impl httptest::matchers::Matcher<Value> for StatusIs {
    fn matches(&mut self, v: &Value, _: &mut httptest::matchers::ExecutionContext) -> bool {
        v["status"] == self.0
    }
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "status {}", self.0)
    }
}

/// Matches any JSON body and keeps it.
struct Keep(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl httptest::matchers::Matcher<Value> for Keep {
    fn matches(&mut self, v: &Value, _: &mut httptest::matchers::ExecutionContext) -> bool {
        self.0.lock().unwrap().push(v.to_string());
        true
    }
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "any body, kept")
    }
}

fn json_value_status(status: &'static str) -> StatusIs {
    StatusIs(status)
}

/// One notify per run, with the end it came to, however many attempts.
#[test]
fn one_notify_per_run_whatever_ended_it() {
    let notify = HttpServer::run();
    notify.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/i"),
            request::body(json_decoded(json_value_status("failed"))),
        ])
        .times(2)
        .respond_with(status_code(200)),
    );
    let (dir, config) = setup_with_raw(&format!(
        "\n[serve]\nretries = 2\nretry_delay = 0\n{}",
        notify_config(&notify)
    ));
    // Without the downloads: every attempt fails at views (exit 1).
    let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    let id = s.start_run(r#"{"only": ["views"], "mode": "full"}"#);
    let r = delivered(&s, &id);
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["attempts"].as_array().unwrap().len(), 3, "{r}");
    // A bad view: exit 2, one attempt.
    let views = dir.path().join("bad-views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("bad.json"),
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    let text = fs::read_to_string(&config).unwrap();
    let old_views = text
        .lines()
        .find(|l| l.starts_with("views = "))
        .unwrap()
        .to_string();
    fs::write(&config, text.replace(&old_views, "views = 'bad-views'")).unwrap();
    let id = s.start_run(r#"{"only": ["views"]}"#);
    let r = delivered(&s, &id);
    assert_eq!(r["attempts"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["attempts"][0]["exit_code"], 2, "{r}");
}

/// A run whose first hook wrote somewhere and whose second failed is not
/// retried: the write happens once.
#[test]
fn a_failing_hook_after_a_write_is_not_retried() {
    let (dir, config) = setup_with_raw(
        "\n[serve]\nretries = 2\nretry_delay = 0\n\n[[hooks]]\nname = \"write\"\nrun = [\"sh\", \"-c\", \"echo x >> writes.txt\"]\n\n[[hooks]]\nname = \"fails\"\nrun = [\"sh\", \"-c\", \"exit 1\"]\n",
    );
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views", "hooks"]}"#);
    let r = s.finished(&id);
    assert_eq!(r["status"], "failed");
    assert_eq!(r["attempts"].as_array().unwrap().len(), 1, "{r}");
    assert!(r["retry"].as_str().unwrap().contains("hooks"), "{r}");
    assert_eq!(
        fs::read_to_string(dir.path().join("writes.txt")).unwrap(),
        "x\n"
    );
}

#[test]
fn metrics_has_the_spec_names() {
    let (_dir, config) = setup_with_raw("");
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views"]}"#);
    s.finished(&id);
    let (status, text) = s.get("/metrics");
    assert_eq!(status, 200);
    for name in [
        "fhir_pipes_runs_total{trigger=\"api\",status=\"succeeded\"} 1",
        "fhir_pipes_run_running 0",
        "fhir_pipes_last_run_timestamp_seconds{status=\"succeeded\"}",
        "fhir_pipes_last_success_timestamp_seconds",
        "fhir_pipes_last_run_duration_seconds",
        "fhir_pipes_retries_total 0",
        "fhir_pipes_schedule_skipped_total 0",
        "fhir_pipes_record_write_failing 0",
        "fhir_pipes_log_lines_dropped_total",
        "fhir_pipes_table_rows{table=\"obs\"} 3",
    ] {
        assert!(text.contains(name), "{name} not in\n{text}");
    }
}

/// None of the secrets serve knows of shows in a record, a log, serve's
/// stdout or a notify body.
#[test]
fn no_secret_in_records_logs_stdout_or_notify() {
    const SECRETS: [&str; 3] = [
        "fhir-token-9z8y7x",
        "hapi-db-url-6w5v4u",
        "webhook-key-3t2s1r",
    ];
    let notify = HttpServer::run();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = bodies.clone();
    notify.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/i"),
            request::body(json_decoded(Keep(seen))),
        ])
        .times(1)
        .respond_with(status_code(200)),
    );
    let (_dir, config) = setup_with_raw(&format!(
        "\n[[serve.notify]]\nname = \"openfn\"\nurl = \"{}\"\nheader = \"x-api-key\"\nvalue_env = \"TEST_WEBHOOK_KEY\"\n\n[[hooks]]\nname = \"leak\"\nrun = [\"sh\", \"-c\", \"echo $FHIR_PIPES_TOKEN $FHIR_PIPES_HAPI_DATABASE_URL $TEST_WEBHOOK_KEY $FHIR_PIPES_SERVE_TOKEN; echo \\\"$FHIR_PIPES_TOKEN\\\" >&2; exit 1\"]\n",
        notify.url_str("/i")
    ));
    let s = start_api(
        &config,
        &[
            ("FHIR_PIPES_TOKEN", SECRETS[0]),
            ("FHIR_PIPES_HAPI_DATABASE_URL", SECRETS[1]),
            ("TEST_WEBHOOK_KEY", SECRETS[2]),
        ],
    );
    let id = s.start_run(r#"{"only": ["views", "hooks"]}"#);
    delivered(&s, &id);
    let (_, record) = s.get(&format!("/runs/{id}"));
    let (_, log) = s.get(&format!("/runs/{id}/log"));
    let on_disk =
        fs::read_to_string(data(&config).join(format!(".serve/runs/{id}/run.json"))).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let stdout: Vec<String> = s.lines.try_iter().collect();
    let body = bodies.lock().unwrap().join("\n");
    assert!(log.contains("***"), "{log}");
    for secret in SECRETS.iter().chain([&TOKEN]) {
        for (what, text) in [
            ("record", &record),
            ("log", &log),
            ("run.json", &on_disk),
            ("stdout", &stdout.join("\n")),
            ("notify", &body),
        ] {
            assert!(!text.contains(secret), "{secret} in {what}: {text}");
        }
    }
}

// ---- The schedule (Task B7) ----

#[test]
fn a_schedule_fires() {
    let (_dir, config) =
        setup_with_raw("\n[[serve.schedule]]\ncron = \"* * * * *\"\nmode = \"incremental\"\n");
    // One second before a minute: the schedule fires a second after start.
    let s = start_api(
        &config,
        &[("FHIR_PIPES_SERVE_TEST_NOW", "2026-10-21T02:00:59Z")],
    );
    let (_, text) = s.get("/status");
    let st: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(st["next_scheduled"], "2026-10-21T02:01:00Z", "{st}");
    let started = Instant::now();
    loop {
        let (_, text) = s.get("/runs");
        let runs: Value = serde_json::from_str(&text).unwrap();
        if let Some(r) = runs.as_array().unwrap().first() {
            assert_eq!(r["trigger"], "schedule", "{r}");
            assert_eq!(r["request"]["mode"], "incremental");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no scheduled run"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---- Restart and shutdown (Task B8) ----

fn read_pid(path: &Path) -> i32 {
    let started = Instant::now();
    loop {
        if let Ok(p) = fs::read_to_string(path)
            && let Ok(pid) = p.trim().parse()
        {
            return pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn a_killed_serve_marks_the_run_interrupted_and_notifies_at_start() {
    let notify = HttpServer::run();
    notify.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/i"),
            request::body(json_decoded(json_value_status("interrupted"))),
        ])
        .times(1)
        .respond_with(status_code(200)),
    );
    let (dir, config) = setup_with_raw(&format!(
        "\n[[hooks]]\nname = \"slow\"\nrun = [\"sh\", \"-c\", \"echo $$ > slow.pid; exec sleep 30\"]\n{}",
        notify_config(&notify)
    ));
    let mut first = start_api(&config, &[]);
    let id = first.start_run(r#"{"only": ["views", "hooks"]}"#);
    let hook = read_pid(&dir.path().join("slow.pid"));
    // SAFETY: kill(2) on the serve this test started.
    unsafe { libc::kill(first.child.id() as i32, libc::SIGKILL) };
    let _ = first.child.wait();
    let second = start_api(&config, &[]);
    let r = delivered(&second, &id);
    assert_eq!(r["status"], "interrupted", "{r}");
    assert_eq!(r["notify"]["openfn"]["state"], "delivered", "{r}");
    // The new serve signals nothing: the old refresh's hook still runs.
    assert!(pid_alive(hook), "the restart signalled an old process");
    // SAFETY: kill(2) on the hook this test's run started.
    unsafe { libc::kill(hook, libc::SIGKILL) };
}

#[test]
fn sigterm_stops_the_child_and_exits() {
    let (dir, config) = setup_with_raw(
        "\n[serve]\nstop_grace = 5\n\n[[hooks]]\nname = \"slow\"\nrun = [\"sh\", \"-c\", \"echo $$ > slow.pid; exec sleep 30\"]\n",
    );
    let mut s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views", "hooks"]}"#);
    let hook = read_pid(&dir.path().join("slow.pid"));
    // SAFETY: kill(2) on the serve this test started.
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    let started = Instant::now();
    let status = loop {
        if let Some(st) = s.child.try_wait().unwrap() {
            break st;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "serve still running"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
    assert!(!pid_alive(hook), "the hook outlived serve");
    let r: Value = serde_json::from_str(
        &fs::read_to_string(data(&config).join(format!(".serve/runs/{id}/run.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(r["status"], "interrupted", "{r}");
    assert!(r["finished_at"].is_string(), "{r}");
}

// ---- Fixes from the final review ----

/// --data wins over FHIR_PIPES_DATA for serve and for the refreshes it starts.
#[test]
fn data_flag_reaches_the_refreshes() {
    let (dir, config) = setup_with_raw("");
    let other = dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let data = data(&config);
    let s = start_serve_with(
        &config,
        &[
            ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
            ("FHIR_PIPES_REUSE_RAW", "true"),
            ("FHIR_PIPES_DATA", other.to_str().unwrap()),
        ],
        &["--data", data.to_str().unwrap()],
    );
    let id = s.start_run(r#"{"only": ["views"]}"#);
    let r = s.finished(&id);
    assert_eq!(r["status"], "succeeded", "{r}");
    assert!(data.join(format!(".serve/runs/{id}")).is_dir());
    assert!(
        data.join("parquet/obs/country=SLE/part-0.parquet")
            .is_file()
    );
    assert!(
        !other.join("parquet").exists(),
        "the refresh wrote FHIR_PIPES_DATA's warehouse"
    );
}

/// Once serve is stopping it accepts no new run, even while it waits to
/// send the last run's notify.
#[test]
fn no_run_is_accepted_once_serve_is_stopping() {
    let notify = HttpServer::run();
    notify.expect(
        Expectation::matching(any())
            .times(..)
            .respond_with(delay_and_then(Duration::from_secs(30), status_code(200))),
    );
    let (dir, config) = setup_with_raw(&format!(
        "\n[serve]\nstop_grace = 5\n\n[[hooks]]\nname = \"slow\"\nrun = [\"sh\", \"-c\", \"touch started; exec sleep 30\"]\n{}",
        notify_config(&notify)
    ));
    let mut s = start_api(&config, &[]);
    s.start_run(r#"{"only": ["views", "hooks"]}"#);
    wait_for(&dir.path().join("started"));
    // SAFETY: kill(2) on the serve this test started.
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    let started = Instant::now();
    while s.child.try_wait().unwrap().is_none() && started.elapsed() < Duration::from_secs(4) {
        if let Ok(r) = client()
            .post(format!("{}/run", s.url))
            .bearer_auth(TOKEN)
            .send()
        {
            assert_ne!(
                r.status(),
                202,
                "a run was accepted while serve was stopping"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A stdout nobody reads does not keep serve from stopping its run and
/// exiting.
#[test]
fn a_blocked_stdout_does_not_stop_the_shutdown() {
    use std::io::Read;
    let (dir, config) = setup_with_raw(
        "\n[serve]\nstop_grace = 1\n\n[[hooks]]\nname = \"noisy\"\nrun = [\"sh\", \"-c\", \"yes 0123456789012345678901234567890123456789 | head -c 20000000; touch printed; exec sleep 30\"]\n",
    );
    let mut child = serve_command(
        &config,
        "127.0.0.1:0",
        &[
            ("FHIR_PIPES_SERVE_TOKEN", TOKEN),
            ("FHIR_PIPES_REUSE_RAW", "true"),
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    // Read up to the address, then keep the pipe open and read no more.
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !String::from_utf8_lossy(&seen).contains('\n') {
        stdout.read_exact(&mut byte).unwrap();
        seen.push(byte[0]);
    }
    let url = String::from_utf8_lossy(&seen)
        .trim()
        .strip_prefix("listening on ")
        .unwrap()
        .to_string();
    let r = client()
        .post(format!("{url}/run"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(r#"{"only": ["views", "hooks"]}"#)
        .send()
        .unwrap();
    assert_eq!(r.status(), 202);
    wait_for(&dir.path().join("printed"));
    // SAFETY: kill(2) on the serve this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            panic!("serve did not stop");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdout);
}

/// A cancel reaches refresh, which holds it back through its publish: the
/// catalog.duckdb build under way finishes.
#[test]
fn a_cancel_during_the_publish_lets_it_finish() {
    use std::os::unix::fs::PermissionsExt;
    let tools = tempfile::tempdir().unwrap();
    let duckdb = tools.path().join("duckdb");
    fs::write(
        &duckdb,
        "#!/bin/sh\necho $$ > duckdb.pid\ncat > /dev/null\nsleep 2\n: > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&duckdb, fs::Permissions::from_mode(0o755)).unwrap();
    let (_dir, config) = setup_with_raw(&format!(
        "\n[serve]\nstop_grace = 10\n\n[tools]\nduckdb = '{}'\n\n[catalog]\nduckdb_file = true\n",
        duckdb.display()
    ));
    let s = start_api(&config, &[]);
    let id = s.start_run(r#"{"only": ["views"]}"#);
    let data = data(&config);
    wait_for(&data.join("duckdb.pid"));
    let (status, _) = s.post(&format!("/runs/{id}/cancel"), None);
    assert_eq!(status, 200);
    let r = s.finished(&id);
    assert_eq!(r["status"], "cancelled", "{r}");
    let result = &r["attempts"][0]["result"];
    assert_eq!(result["published"], true, "{r}");
    assert!(
        data.join("catalog.duckdb").is_file(),
        "the catalog build was stopped"
    );
}

/// A run left unfinished whose end cannot be recorded stops serve at start,
/// rather than being forgotten.
#[test]
fn an_unrecordable_recovery_stops_serve_at_start() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, config) = setup_with_raw("");
    let run = data(&config).join(".serve/runs/20261021T020000Z-7f3a");
    fs::create_dir_all(&run).unwrap();
    fs::write(
        run.join("run.json"),
        json_record("20261021T020000Z-7f3a", "running"),
    )
    .unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o500)).unwrap();
    let (code, stderr) = serve_fails(&config, "127.0.0.1:0", &[("FHIR_PIPES_SERVE_TOKEN", TOKEN)]);
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("20261021T020000Z-7f3a"), "{stderr}");
}

fn json_record(id: &str, status: &str) -> String {
    serde_json::json!({
        "id": id, "trigger": "api",
        "request": {"mode": "incremental", "only": [], "skip": [], "push": false},
        "status": status, "accepted_at": "t", "started_at": "t", "finished_at": null,
        "attempts": [], "retry": null, "notify": {},
    })
    .to_string()
}

/// Secrets the config names (a [database] url_env, a notify value_env) are
/// hidden by the refresh too, before it cuts an error short.
#[test]
fn configured_secrets_are_hidden_across_the_error_cut() {
    const SECRET: &str = "cfg-secret-0123456789abcdefghijk";
    for (extra, var) in [
        ("\n[database]\nurl_env = \"WAREHOUSE_DB\"\n", "WAREHOUSE_DB"),
        (
            "\n[[serve.notify]]\nname = \"n\"\nurl = \"http://127.0.0.1:9/i\"\nheader = \"x-api-key\"\nvalue_env = \"WEBHOOK_KEY\"\n",
            "WEBHOOK_KEY",
        ),
    ] {
        let fhir = HttpServer::run();
        fhir.expect(
            Expectation::matching(any())
                .times(..)
                .respond_with(status_code(401).body(format!("{}{SECRET}", "e".repeat(469)))),
        );
        let (_dir, config) = setup(
            &fhir.url_str("/fhir"),
            &format!("\n[serve]\nretries = 0\n{extra}"),
        );
        let s = start_serve(&config, &[("FHIR_PIPES_SERVE_TOKEN", TOKEN), (var, SECRET)]);
        let id = s.start_run(r#"{"only": ["views"]}"#);
        s.finished(&id);
        let (_, record) = s.get(&format!("/runs/{id}"));
        for n in 4..=SECRET.len() {
            assert!(
                !record.contains(&SECRET[..n]),
                "{var}: {n} characters shown: {record}"
            );
        }
    }
}
