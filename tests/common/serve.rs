//! A `fhir-pipes serve` process for the serve tests: started on a free
//! loopback port, its stdout lines read, killed when dropped.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

/// A serve process, killed when dropped, with its stdout lines.
pub struct Serve {
    pub child: Child,
    pub url: String,
    pub lines: mpsc::Receiver<String>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn serve_command(config: &Path, listen: &str, env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(assert_cmd::cargo::cargo_bin("fhir-pipes"));
    c.current_dir(config.parent().unwrap());
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_SERVE_TOKEN",
        "FHIR_PIPES_QUERY_TOKEN",
        "FHIR_PIPES_SERVE_LISTEN",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_RESULT_FILE",
        "FHIR_PIPES_FAILURE_HOOKS",
        "FHIR_PIPES_RUN_ID",
    ]
    .iter()
    .chain(super::CLEARED)
    {
        c.env_remove(var);
    }
    c.envs(env.iter().copied())
        .args(["serve", "--config"])
        .arg(config)
        .args(["--listen", listen]);
    c
}

/// Start serve on a free loopback port and wait for the address it prints.
pub fn start_serve(config: &Path, env: &[(&str, &str)]) -> Serve {
    start_serve_with(config, env, &[])
}

pub fn start_serve_with(config: &Path, env: &[(&str, &str)], args: &[&str]) -> Serve {
    let mut child = serve_command(config, "127.0.0.1:0", env)
        .args(args)
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

pub fn client() -> reqwest::blocking::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Run serve to its exit, as for a config error; (code, stderr). One still
/// running after 20 s is killed, with no code.
pub fn serve_fails(config: &Path, listen: &str, env: &[(&str, &str)]) -> (Option<i32>, String) {
    use std::io::Read;
    let mut child = serve_command(config, listen, env)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    (code, stderr)
}

/// The serve token the helpers send.
pub const TOKEN: &str = "serve-test-token";

impl Serve {
    pub fn post(&self, path: &str, body: Option<&str>) -> (u16, Value) {
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

    pub fn get(&self, path: &str) -> (u16, String) {
        let r = client()
            .get(format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    }

    pub fn run(&self, id: &str) -> Value {
        let (status, text) = self.get(&format!("/runs/{id}"));
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    /// Wait for run `id` to end; its record.
    pub fn finished(&self, id: &str) -> Value {
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
    pub fn start_run(&self, body: &str) -> String {
        let (status, v) = self.post("/run", Some(body));
        assert_eq!(status, 202, "{v}");
        v["id"].as_str().unwrap().to_string()
    }
}
