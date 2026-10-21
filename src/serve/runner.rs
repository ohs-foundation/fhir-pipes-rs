//! One run: its attempts, each a child `fhir-pipes refresh` in a process
//! group of its own, what each attempt's exit means, and whether to try
//! again.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::config::{Mode, ServeConfig};
use super::store::{Attempt, Delivery, DeliveryState, RunRecord, RunRequest, RunStatus, Store};

/// How long `serve` waits for an attempt's process group to empty after
/// killing it, before going on with a warning.
const GROUP_WAIT: Duration = Duration::from_secs(30);
/// How often a failed write of a run's end is tried again.
const SAVE_RETRY: Duration = Duration::from_secs(30);

/// The failed steps after which an attempt is tried again: they change
/// nothing outside the warehouse's staging and raw downloads.
pub const RETRYABLE: [&str; 9] = [
    "lock",
    "tools",
    "start",
    "files",
    "locations",
    "views",
    "tiles",
    "manifest",
    "catalog",
];

/// What starts each attempt.
pub struct Job {
    /// The program run as `refresh` (the fhir-pipes binary; a stand-in in
    /// the unit tests).
    pub program: PathBuf,
    /// Arguments before the run's own: `refresh --config <file>` and
    /// `--data` when serve was given one.
    pub base_args: Vec<String>,
    pub cfg: ServeConfig,
    pub store: Arc<Store>,
    pub redactor: Arc<super::secrets::Redactor>,
    /// serve's stdout, where every attempt's output also goes.
    pub stdout: Arc<super::output::StdoutQueue>,
    /// Set while the end of a run cannot be written: no run is accepted
    /// meanwhile.
    pub write_failing: Arc<std::sync::atomic::AtomicBool>,
}

/// Asks a run to stop. Shared between the run and whoever cancels it.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Cancel>);

#[derive(Default)]
struct Cancel {
    state: Mutex<CancelState>,
    changed: Condvar,
}

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    /// The stop came from serve stopping, not from a cancel.
    interrupted: bool,
    /// The running attempt's process group, while its leader is not reaped:
    /// until then the id cannot be anyone else's.
    pgid: Option<i32>,
}

impl CancelToken {
    /// Stop the run: SIGTERM to the running attempt's process group, then
    /// SIGKILL after `grace` if it is still running; a run waiting between
    /// attempts ends at once.
    pub fn cancel(&self, grace: Duration) {
        let pgid = {
            let mut s = self.lock();
            s.cancelled = true;
            s.pgid
        };
        self.0.changed.notify_all();
        if let Some(pgid) = pgid {
            self.signal_group(libc::SIGTERM);
            let token = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(grace);
                // Only the same attempt, if it is still not reaped.
                if token.lock().pgid == Some(pgid) {
                    token.signal_group(libc::SIGKILL);
                }
            });
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Stop the run because serve is stopping: as `cancel`, but the run
    /// ends `interrupted`.
    pub fn interrupt(&self, grace: Duration) {
        self.lock().interrupted = true;
        self.cancel(grace);
    }

    fn stopped_status(&self) -> RunStatus {
        if self.lock().interrupted {
            RunStatus::Interrupted
        } else {
            RunStatus::Cancelled
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CancelState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Send `sig` to the running attempt's group, under the lock that keeps
    /// the group id from being reaped meanwhile.
    fn signal_group(&self, sig: i32) {
        let s = self.lock();
        if let Some(pgid) = s.pgid {
            // SAFETY: killpg(2) on a group whose leader is not yet reaped.
            unsafe {
                libc::killpg(pgid, sig);
            }
        }
    }

    /// Wait up to `d`; true when cancelled meanwhile.
    fn wait(&self, d: Duration) -> bool {
        let deadline = std::time::Instant::now() + d;
        let mut s = self.lock();
        loop {
            if s.cancelled {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            s = self
                .0
                .changed
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// How an attempt's child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Code(i32),
    Signal(i32),
}

impl From<ExitStatus> for ExitKind {
    fn from(s: ExitStatus) -> ExitKind {
        use std::os::unix::process::ExitStatusExt;
        match (s.code(), s.signal()) {
            (Some(c), _) => ExitKind::Code(c),
            (None, Some(sig)) => ExitKind::Signal(sig),
            (None, None) => ExitKind::Code(1),
        }
    }
}

/// The attempt's result: its result file when that is a final record, with
/// the exit status overriding what it says; otherwise what can be told from
/// the exit and the last lines of stderr.
pub fn result_of(exit: ExitKind, file: &Path, stderr_tail: &str) -> Value {
    let recorded = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|r| matches!(r["status"].as_str(), Some("succeeded" | "failed")));
    let tail = stderr_tail.trim();
    match exit {
        ExitKind::Code(0) => {
            let mut r = recorded.unwrap_or_else(|| json!({}));
            if let Some(o) = r.as_object_mut() {
                o.remove("failed_step");
                o.remove("error");
            }
            r["status"] = json!("succeeded");
            r["exit_code"] = json!(0);
            r
        }
        ExitKind::Code(code) => match recorded {
            Some(mut r) if r["status"] == "failed" => {
                r["exit_code"] = json!(code);
                r
            }
            _ => json!({
                "status": "failed",
                "failed_step": "unknown",
                "exit_code": code,
                "error": if tail.is_empty() { format!("exit {code}") } else { tail.to_string() },
            }),
        },
        ExitKind::Signal(sig) => json!({
            "status": "failed",
            "failed_step": "unknown",
            "error": format!("killed by signal {sig}"),
            "committed": "unknown",
            "published": "unknown",
        }),
    }
}

/// `Ok` when the attempt should be tried again; otherwise why not.
pub fn retry_decision(
    result: &Value,
    exit: ExitKind,
    cancelled: bool,
    attempts: u32,
    retries: u32,
) -> Result<(), String> {
    if cancelled {
        return Err("not retried: cancelled".into());
    }
    match exit {
        ExitKind::Signal(sig) => return Err(format!("not retried: killed by signal {sig}")),
        ExitKind::Code(2) => return Err("not retried: exit 2 is a config error".into()),
        ExitKind::Code(1) => {}
        ExitKind::Code(code) => return Err(format!("not retried: exit {code}")),
    }
    let step = result["failed_step"].as_str().unwrap_or("unknown");
    if !RETRYABLE.contains(&step) {
        return Err(format!("not retried: failed at {step}"));
    }
    if attempts > retries {
        return Err(format!("not retried: {retries} retries used"));
    }
    Ok(())
}

pub fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// The arguments for one attempt of `req`.
fn args(job: &Job, req: &RunRequest) -> Vec<String> {
    let mut a = job.base_args.clone();
    a.extend(["--lock-timeout".into(), "0".into()]);
    if req.mode == Mode::Full {
        a.push("--full".into());
    }
    if !req.only.is_empty() {
        a.extend(["--only".into(), req.only.join(",")]);
    }
    if !req.skip.is_empty() {
        a.extend(["--skip".into(), req.skip.join(",")]);
    }
    if req.push {
        a.push("--push".into());
    }
    a
}

/// The other members of process group `pgid`.
#[cfg(target_os = "linux")]
fn group_members(pgid: i32) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != pgid)
        .filter(|pid| {
            // /proc/<pid>/stat: pid (comm) state ppid pgrp …; comm may hold
            // spaces and parentheses, so the fields are read after the last ')'.
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    let rest = &s[s.rfind(')')? + 1..];
                    rest.split_whitespace().nth(2)?.parse::<i32>().ok()
                })
                == Some(pgid)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn group_members(pgid: i32) -> Vec<i32> {
    let mut pids = vec![0 as libc::pid_t; 4096];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer is `size` bytes of pid_t.
    let got = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
    if got <= 0 {
        return Vec::new();
    }
    let n = got as usize / std::mem::size_of::<libc::pid_t>();
    pids[..n.min(pids.len())]
        .iter()
        .copied()
        .filter(|&p| p != pgid && p > 0)
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn group_members(_pgid: i32) -> Vec<i32> {
    Vec::new()
}

/// Block until child `pid` has exited, leaving it unreaped.
fn wait_exited(pid: i32) {
    loop {
        // SAFETY: a zeroed siginfo_t for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) on our own child, without reaping it (WNOWAIT).
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// One attempt: (how the child ended, its result).
fn attempt(job: &Job, rec: &RunRecord, n: u32, cancel: &CancelToken) -> (ExitKind, Value) {
    use std::os::unix::process::CommandExt;
    let dir = job.store.run_dir(&rec.id);
    let result_file = dir.join(format!("attempt-{n}.json"));
    let log = match super::output::RunLog::open(&dir.join("log"), job.cfg.max_log_bytes) {
        Ok(log) => Arc::new(Mutex::new(log)),
        Err(e) => {
            return (
                ExitKind::Code(1),
                json!({"status": "failed", "failed_step": "unknown", "exit_code": 1,
                       "error": format!("opening the run's log: {e}")}),
            );
        }
    };
    let started = std::process::Command::new(&job.program)
        .args(args(job, &rec.request))
        .env("FHIR_PIPES_RESULT_FILE", &result_file)
        .env("FHIR_PIPES_FAILURE_HOOKS", "off")
        .env("FHIR_PIPES_RUN_ID", rec.id.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match started {
        Ok(c) => c,
        Err(e) => {
            let exit = ExitKind::Code(1);
            return (
                exit,
                json!({"status": "failed", "failed_step": "unknown", "exit_code": 1,
                       "error": format!("starting {}: {e}", job.program.display())}),
            );
        }
    };
    let capture = super::output::Capture::start(
        child.stdout.take().expect("piped stdout"),
        child.stderr.take().expect("piped stderr"),
        log.clone(),
        job.stdout.clone(),
        job.redactor.clone(),
        rec.id.as_str().to_string(),
    );
    let pid = child.id() as i32;
    let cancelled = {
        let mut s = cancel.lock();
        s.pgid = Some(pid);
        s.cancelled
    };
    if cancelled {
        cancel.signal_group(libc::SIGTERM);
    }
    wait_exited(pid);
    // The leader has exited and is not reaped, so the group id is still
    // this attempt's: whatever it left running is stopped before the
    // attempt is over.
    let deadline = std::time::Instant::now() + GROUP_WAIT;
    loop {
        cancel.signal_group(libc::SIGKILL);
        if group_members(pid).is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "fhir-pipes: warning: run {}: processes of attempt {n} still running after SIGKILL",
                rec.id.as_str()
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = {
        let mut s = cancel.lock();
        let status = child.wait();
        s.pgid = None;
        status
    };
    // Every process that held the pipes is gone, so both streams end.
    let stderr_tail = capture.finish();
    log.lock().unwrap_or_else(|e| e.into_inner()).close();
    let exit = status.map(ExitKind::from).unwrap_or(ExitKind::Code(1));
    let mut result = result_of(exit, &result_file, &stderr_tail);
    job.redactor.redact_json(&mut result);
    (exit, result)
}

/// Write `rec`; a write that fails is reported and does not stop the run.
fn save(job: &Job, rec: &RunRecord) {
    if let Err(e) = job.store.save(rec) {
        eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
    }
}

/// Run `record` to its end: its attempts, the waits between them, its end
/// written with every destination pending.
pub fn run(job: &Job, record: RunRecord, cancel: &CancelToken) -> RunRecord {
    let mut rec = record;
    rec.status = RunStatus::Running;
    rec.started_at = Some(now());
    save(job, &rec);
    let mut n = 1;
    loop {
        if cancel.is_cancelled() {
            rec.status = cancel.stopped_status();
            break;
        }
        let started_at = now();
        let (exit, result) = attempt(job, &rec, n, cancel);
        let step = result["failed_step"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        rec.attempts.push(Attempt {
            started_at,
            finished_at: Some(now()),
            exit_code: match exit {
                ExitKind::Code(c) => Some(c),
                ExitKind::Signal(_) => None,
            },
            result: Some(result.clone()),
        });
        if exit == ExitKind::Code(0) {
            rec.status = RunStatus::Succeeded;
            rec.retry = None;
            break;
        }
        match retry_decision(&result, exit, cancel.is_cancelled(), n, job.cfg.retries) {
            Ok(()) => {
                rec.retry = Some(format!("retried: attempt {n} failed at {step}"));
                rec.status = RunStatus::Waiting;
                save(job, &rec);
                if cancel.wait(job.cfg.retry_delay) {
                    rec.status = cancel.stopped_status();
                    rec.retry = Some("not retried: cancelled".into());
                    break;
                }
                rec.status = RunStatus::Running;
                save(job, &rec);
                n += 1;
            }
            Err(why) => {
                rec.retry = Some(why);
                rec.status = if cancel.is_cancelled() {
                    cancel.stopped_status()
                } else {
                    RunStatus::Failed
                };
                break;
            }
        }
    }
    finish(job, &mut rec);
    rec
}

/// Write the run's end with every destination pending; nothing is sent
/// before that write succeeds, so it is tried again until it does.
pub fn finish(job: &Job, rec: &mut RunRecord) {
    rec.finished_at = Some(now());
    for d in &job.cfg.notify {
        rec.notify.insert(
            d.name.clone(),
            Delivery {
                state: DeliveryState::Pending,
                tries: 0,
                last_error: None,
            },
        );
    }
    while let Err(e) = job.store.save(rec) {
        job.write_failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        eprintln!(
            "fhir-pipes: error: {}; trying again in {} s",
            crate::error::message(&e),
            SAVE_RETRY.as_secs()
        );
        std::thread::sleep(SAVE_RETRY);
    }
    job.write_failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::store::{RunId, Trigger};
    use std::os::unix::fs::PermissionsExt;

    fn failed(step: &str, code: i32) -> Value {
        json!({"status": "failed", "failed_step": step, "exit_code": code})
    }

    #[test]
    fn retry_only_before_the_database() {
        for step in RETRYABLE {
            assert_eq!(
                retry_decision(&failed(step, 1), ExitKind::Code(1), false, 1, 2),
                Ok(()),
                "{step}"
            );
        }
        for step in ["database", "publish", "hooks", "push", "finish", "unknown"] {
            let why = retry_decision(&failed(step, 1), ExitKind::Code(1), false, 1, 2).unwrap_err();
            assert!(why.contains(step), "{why}");
        }
        let why = retry_decision(&failed("views", 2), ExitKind::Code(2), false, 1, 2).unwrap_err();
        assert!(why.contains("exit 2"), "{why}");
        let why =
            retry_decision(&failed("views", 1), ExitKind::Signal(9), false, 1, 2).unwrap_err();
        assert!(why.contains("signal 9"), "{why}");
        let why = retry_decision(&failed("views", 1), ExitKind::Code(1), true, 1, 2).unwrap_err();
        assert!(why.contains("cancelled"), "{why}");
        let why = retry_decision(&failed("views", 1), ExitKind::Code(1), false, 3, 2).unwrap_err();
        assert!(why.contains("2 retries"), "{why}");
    }

    #[test]
    fn exit_0_without_a_result_file_is_success() {
        let r = result_of(ExitKind::Code(0), Path::new("/no/such/file.json"), "");
        assert_eq!(r["status"], "succeeded");
        assert_eq!(r["exit_code"], 0);
    }

    #[test]
    fn exit_0_wins_over_the_result_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.json");
        std::fs::write(&file, failed("views", 1).to_string()).unwrap();
        let r = result_of(ExitKind::Code(0), &file, "");
        assert_eq!(r["status"], "succeeded");
        assert_eq!(r["exit_code"], 0);
    }

    #[test]
    fn a_signal_is_failed_without_exit_code() {
        let r = result_of(ExitKind::Signal(9), Path::new("/no/such/file.json"), "");
        assert_eq!(r["status"], "failed");
        assert_eq!(r["error"], "killed by signal 9");
        assert!(r.get("exit_code").is_none(), "{r}");
        assert_eq!(r["failed_step"], "unknown");
    }

    #[test]
    fn an_unreadable_result_file_uses_the_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.json");
        std::fs::write(&file, "{\"status\": \"runn").unwrap();
        let r = result_of(ExitKind::Code(1), &file, "fhir-pipes: views: down\n");
        assert_eq!(r["status"], "failed");
        assert_eq!(r["failed_step"], "unknown");
        assert_eq!(r["exit_code"], 1);
        assert!(r["error"].as_str().unwrap().contains("views: down"), "{r}");
    }

    /// A stand-in for `refresh`: a shell script run with the run's
    /// environment.
    fn job(dir: &Path, script: &str, retries: u32) -> Job {
        let program = dir.join("refresh");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = crate::serve::config::parse(None).unwrap();
        cfg.retries = retries;
        cfg.retry_delay = Duration::ZERO;
        cfg.stop_grace = Duration::from_secs(2);
        Job {
            program,
            base_args: vec![],
            cfg,
            store: Arc::new(Store::open(dir.join("runs")).unwrap()),
            redactor: Arc::default(),
            stdout: crate::serve::output::StdoutQueue::start(Box::new(std::io::sink())),
            write_failing: Arc::default(),
        }
    }

    fn start(job: &Job) -> RunRecord {
        let mut r = RunRecord::new(
            RunId::new(chrono::Utc::now()),
            Trigger::Api,
            RunRequest::default(),
            "t".into(),
        );
        job.store.create(&mut r).unwrap();
        r
    }

    fn alive(pid_file: &Path) -> bool {
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_failure_at_views_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let job = job(
            dir.path(),
            r#"echo '{"status":"failed","failed_step":"views","exit_code":1}' > "$FHIR_PIPES_RESULT_FILE"; exit 1"#,
            1,
        );
        let r = run(&job, start(&job), &CancelToken::default());
        assert_eq!(r.status, RunStatus::Failed);
        assert_eq!(r.attempts.len(), 2);
        assert!(
            r.retry.as_deref().unwrap().contains("1 retries"),
            "{:?}",
            r.retry
        );
        assert_eq!(job.store.load(&r.id).unwrap(), r);
    }

    #[test]
    fn a_bad_view_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let job = job(
            dir.path(),
            r#"echo '{"status":"failed","failed_step":"views","exit_code":2}' > "$FHIR_PIPES_RESULT_FILE"; exit 2"#,
            2,
        );
        let r = run(&job, start(&job), &CancelToken::default());
        assert_eq!(r.status, RunStatus::Failed);
        assert_eq!(r.attempts.len(), 1);
    }

    #[test]
    fn a_failure_after_the_publish_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let job = job(
            dir.path(),
            r#"echo '{"status":"failed","failed_step":"hooks","exit_code":1}' > "$FHIR_PIPES_RESULT_FILE"; exit 1"#,
            2,
        );
        let r = run(&job, start(&job), &CancelToken::default());
        assert_eq!(r.attempts.len(), 1);
        assert!(
            r.retry.as_deref().unwrap().contains("hooks"),
            "{:?}",
            r.retry
        );
    }

    #[test]
    fn a_run_that_succeeds_has_its_environment_and_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = job(
            dir.path(),
            r#"[ "$FHIR_PIPES_FAILURE_HOOKS" = off ] && [ -n "$FHIR_PIPES_RUN_ID" ] && echo "$@" > "$(dirname "$FHIR_PIPES_RESULT_FILE")/args""#,
            0,
        );
        job.cfg.notify = crate::serve::config::parse(Some(
            toml::from_str("[[notify]]\nname = \"openfn\"\nurl = \"https://x.example/i\"").unwrap(),
        ))
        .unwrap()
        .notify;
        let mut rec = start(&job);
        rec.request = RunRequest {
            mode: Mode::Full,
            only: vec!["views".into(), "hooks".into()],
            skip: vec!["tiles".into()],
            push: true,
        };
        let r = run(&job, rec, &CancelToken::default());
        assert_eq!(r.status, RunStatus::Succeeded, "{r:?}");
        let args = std::fs::read_to_string(job.store.run_dir(&r.id).join("args")).unwrap();
        assert_eq!(
            args.trim(),
            "--lock-timeout 0 --full --only views,hooks --skip tiles --push"
        );
        assert!(r.notify["openfn"].state == crate::serve::store::DeliveryState::Pending);
        assert!(r.finished_at.is_some());
    }

    #[test]
    fn killing_refresh_alone_stops_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("tool.pid");
        let job = job(
            dir.path(),
            &format!("sleep 30 & echo $! > {}; kill -9 $$", pid.display()),
            0,
        );
        let r = run(&job, start(&job), &CancelToken::default());
        assert_eq!(r.status, RunStatus::Failed);
        assert_eq!(
            r.attempts[0].result.as_ref().unwrap()["error"],
            "killed by signal 9"
        );
        assert!(!alive(&pid), "the tool outlived its refresh");
    }

    #[test]
    fn cancel_stops_the_child_and_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("tool.pid");
        let job = job(
            dir.path(),
            &format!("sleep 30 & echo $! > {}; wait", pid.display()),
            2,
        );
        let cancel = CancelToken::default();
        let rec = start(&job);
        let c = cancel.clone();
        let p = pid.clone();
        std::thread::spawn(move || {
            while !p.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            c.cancel(Duration::from_secs(2));
        });
        let r = run(&job, rec, &cancel);
        assert_eq!(r.status, RunStatus::Cancelled);
        assert_eq!(r.attempts.len(), 1);
        assert!(!alive(&pid), "the tool outlived the cancel");
    }

    #[test]
    fn an_outside_refresh_does_not_change_the_attempts_result() {
        let dir = tempfile::tempdir().unwrap();
        let job = job(
            dir.path(),
            r#"echo '{"status":"failed","failed_step":"views","exit_code":1}' > "$FHIR_PIPES_RESULT_FILE"; echo '{"status":"succeeded"}' > "$(dirname "$FHIR_PIPES_RESULT_FILE")/last-run.json"; exit 1"#,
            0,
        );
        let r = run(&job, start(&job), &CancelToken::default());
        assert_eq!(r.status, RunStatus::Failed);
        assert_eq!(
            r.attempts[0].result.as_ref().unwrap()["failed_step"],
            "views"
        );
    }
}
