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
    /// Read by the tests of later tasks.
    #[allow(dead_code)]
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

#[allow(dead_code)]
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
