//! End-to-end runs of `fhir-pipes serve`: the command, its config, the API,
//! runs and their attempts, notify, the schedule, restart and shutdown.
#![cfg(unix)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::*;

/// A serve process, killed when dropped, with its stdout lines.
struct Serve {
    child: Child,
    url: String,
    lines: mpsc::Receiver<String>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve_command(config: &Path, listen: &str, env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(assert_cmd::cargo::cargo_bin("fhir-pipes"));
    c.current_dir(config.parent().unwrap());
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_SERVE_TOKEN",
        "FHIR_PIPES_SERVE_LISTEN",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_RESULT_FILE",
        "FHIR_PIPES_FAILURE_HOOKS",
        "FHIR_PIPES_RUN_ID",
    ] {
        c.env_remove(var);
    }
    c.envs(env.iter().copied())
        .args(["serve", "--config"])
        .arg(config)
        .args(["--listen", listen]);
    c
}

/// Start serve on a free loopback port and wait for the address it prints.
fn start_serve(config: &Path, env: &[(&str, &str)]) -> Serve {
    let mut child = serve_command(config, "127.0.0.1:0", env)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let url = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = lines.recv_timeout(left).expect("serve printed no address");
        if let Some(addr) = line.strip_prefix("listening on ") {
            break addr.trim().to_string();
        }
    };
    Serve { child, url, lines }
}

fn client() -> reqwest::blocking::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Run serve to its exit, as for a config error; (code, stderr).
fn serve_fails(config: &Path, listen: &str, env: &[(&str, &str)]) -> (Option<i32>, String) {
    let out = serve_command(config, listen, env).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

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

const TOKEN: &str = "serve-test-token";

impl Serve {
    fn post(&self, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut req = client()
            .post(format!("{}{path}", self.url))
            .bearer_auth(TOKEN);
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let r = req.send().unwrap();
        let status = r.status().as_u16();
        let text = r.text().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn get(&self, path: &str) -> (u16, String) {
        let r = client()
            .get(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    }

    fn run(&self, id: &str) -> Value {
        let (status, text) = self.get(&format!("/runs/{id}"));
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    /// Wait for run `id` to end; its record.
    fn finished(&self, id: &str) -> Value {
        let started = Instant::now();
        loop {
            let r = self.run(id);
            if !matches!(
                r["status"].as_str(),
                Some("accepted" | "running" | "waiting")
            ) {
                return r;
            }
            assert!(started.elapsed() < Duration::from_secs(90), "{r}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Start a run with `body`; its id.
    fn start_run(&self, body: &str) -> String {
        let (status, v) = self.post("/run", Some(body));
        assert_eq!(status, 202, "{v}");
        v["id"].as_str().unwrap().to_string()
    }
}

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
    wait_for(&pid_file);
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
