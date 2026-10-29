//! Running an approved SQLQuery Library for `$sql-run`: compose its SQL with
//! each input read from the warehouse's Parquet, run it with the `duckdb`
//! command, and return its rows as NDJSON, JSON or CSV.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Context;

use super::config::{QueryConfig, Source};
use crate::library::compose::compose;
use crate::library::resolve::{Artifact, Catalog};
use crate::library::{Library, bind_table, input_table};
use crate::warehouse::sql::sql_path;
use crate::warehouse::stage::publish_state;

/// The format of a result's rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Ndjson,
    Json,
    Csv,
}

impl Format {
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Ndjson => "application/x-ndjson",
            Format::Json => "application/json",
            Format::Csv => "text/csv",
        }
    }

    /// The options of duckdb's `COPY … TO` for the format.
    fn copy_options(self) -> &'static str {
        match self {
            Format::Ndjson => "FORMAT json",
            Format::Json => "FORMAT json, ARRAY true",
            Format::Csv => "FORMAT csv, HEADER true",
        }
    }
}

/// A query's result: its rows in the format asked for, how many there are,
/// and `max_rows` when the rows were cut short there.
#[derive(Debug)]
pub struct Output {
    pub body: Vec<u8>,
    pub rows: usize,
    pub truncated_at: Option<usize>,
}

/// Why a query gave no result.
#[derive(Debug)]
pub enum QueryError {
    /// The data is not there to read yet, or is being replaced: an input
    /// table is missing, or a refresh published while the query ran.
    /// `message` is fixed text for the caller; `detail`, which can name
    /// tables, is for serve's log.
    Unavailable {
        message: String,
        detail: String,
    },
    /// duckdb refused or failed the query, or it ran past the timeout.
    /// `message` is fixed text for the caller, never duckdb's, which can
    /// quote the setup SQL's keys; `detail` is for serve's log: for a
    /// failed setup, only the kind of error duckdb gave; for a failed query,
    /// duckdb's error without the lines that quote the setup SQL (see
    /// `without_setup`).
    Sql {
        message: String,
        detail: String,
    },
    /// Every slot is running a query.
    Busy,
    /// serve is stopping: the query was not run, or was stopped.
    Stopping,
    Internal(String),
}

/// What a caller is told when an input table is not there to read.
const NOT_AVAILABLE: &str = "an input table is not available yet; retry later";

/// Where the warehouse's Parquet is read from.
#[derive(Debug)]
enum Root {
    /// The served warehouse directory.
    Local(PathBuf),
    /// A remote prefix, read with the setup SQL's secrets.
    Remote(String),
}

impl Root {
    /// The root as `read_table` takes it.
    fn prefix(&self) -> String {
        match self {
            Root::Local(p) => p.display().to_string(),
            Root::Remote(r) => r.clone(),
        }
    }
}

/// How long a duckdb has after SIGTERM, when serve stops, before SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// The running duckdb processes, and whether serve is stopping. A pid is
/// removed as its process is reaped, under the same lock, so a pid here is
/// never someone else's.
#[derive(Default)]
struct Children {
    stopping: bool,
    pids: BTreeSet<u32>,
}

fn lock(children: &Mutex<Children>) -> MutexGuard<'_, Children> {
    children.lock().unwrap_or_else(|e| e.into_inner())
}

/// Send `sig` to each of `pids`.
fn signal(pids: &BTreeSet<u32>, sig: i32) {
    for &pid in pids {
        // SAFETY: kill(2) on a child that is not yet reaped.
        unsafe {
            libc::kill(pid as libc::pid_t, sig);
        }
    }
}

/// Runs SQLQuery Libraries, `concurrency` at once.
pub struct Runner {
    duckdb: String,
    root: Root,
    /// The SQL run before each query, as `duckdb_setup` says.
    setup: Option<String>,
    strip_prefix: Vec<String>,
    timeout: Duration,
    max_rows: usize,
    concurrency: usize,
    /// How many queries are running.
    running: AtomicUsize,
    /// The duckdb processes running, which `stop` stops.
    children: Arc<Mutex<Children>>,
    /// The variables removed from duckdb's environment: serve's secrets,
    /// which an approved query could read with `getenv`.
    hidden_env: Vec<String>,
    /// Run once after the first run of a query, before the publish state is
    /// read again.
    #[cfg(test)]
    after_run: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The pid of the last duckdb run.
    #[cfg(test)]
    last_pid: std::sync::Mutex<Option<u32>>,
}

/// A running query's slot, given back when dropped.
pub(super) struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Runner {
    /// A runner for the query settings `cfg` over warehouse `data`, running
    /// the `duckdb` command. The setup SQL is read here, once.
    pub fn new(
        cfg: &QueryConfig,
        data: &Path,
        duckdb: &str,
        strip_prefix: &[String],
    ) -> anyhow::Result<Runner> {
        let setup = match &cfg.duckdb_setup {
            Some(p) => Some(
                fs::read_to_string(p)
                    .with_context(|| format!("reading duckdb_setup {}", p.display()))?,
            ),
            None => None,
        };
        let root = match &cfg.source {
            Source::Local => Root::Local(data.to_path_buf()),
            Source::Remote(prefix) => Root::Remote(prefix.clone()),
        };
        // duckdb runs in a temp directory, so a relative path to it is
        // resolved here, against the working directory, where refresh
        // finds it. A bare name is looked up on PATH.
        let duckdb = if Path::new(duckdb).is_relative() && duckdb.contains(['/', '\\']) {
            std::env::current_dir()
                .context("reading the current directory")?
                .join(duckdb)
                .display()
                .to_string()
        } else {
            duckdb.to_string()
        };
        Ok(Runner {
            duckdb,
            root,
            setup,
            strip_prefix: strip_prefix.to_vec(),
            timeout: cfg.timeout,
            max_rows: cfg.max_rows,
            concurrency: cfg.concurrency,
            running: AtomicUsize::new(0),
            children: Arc::default(),
            hidden_env: crate::redact::SECRET_VARS
                .iter()
                .map(|v| v.to_string())
                .collect(),
            #[cfg(test)]
            after_run: std::sync::Mutex::new(None),
            #[cfg(test)]
            last_pid: std::sync::Mutex::new(None),
        })
    }

    /// Remove the variables `names` from duckdb's environment too.
    pub fn hide_env(&mut self, names: impl IntoIterator<Item = String>) {
        self.hidden_env.extend(names);
    }

    /// serve is stopping: refuse queries from now on, and SIGTERM the
    /// running duckdb processes, then SIGKILL those still running after
    /// `grace`. Returns without waiting.
    pub fn stop(&self, grace: Duration) {
        {
            let mut c = lock(&self.children);
            c.stopping = true;
            signal(&c.pids, libc::SIGTERM);
        }
        let children = self.children.clone();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            signal(&lock(&children).pids, libc::SIGKILL);
        });
    }

    fn stopping(&self) -> bool {
        lock(&self.children).stopping
    }

    /// Run `lib`, at most `limit` rows and never more than `max_rows`.
    /// Over a local warehouse, a query that a publish may have overlapped is
    /// run once more.
    pub fn run(
        &self,
        catalog: &Catalog,
        lib: &Library,
        format: Format,
        limit: Option<usize>,
    ) -> Result<Output, QueryError> {
        if self.stopping() {
            return Err(QueryError::Stopping);
        }
        let _slot = self.slot().ok_or(QueryError::Busy)?;
        let Root::Local(data) = &self.root else {
            return self.attempt(catalog, lib, format, limit);
        };
        for _ in 0..2 {
            let before = publish_state(data);
            let result = self.attempt(catalog, lib, format, limit);
            if matches!(result, Err(QueryError::Stopping)) {
                return result;
            }
            #[cfg(test)]
            if let Some(hook) = self.after_run.lock().unwrap().take() {
                hook();
            }
            let after = publish_state(data);
            if !before.publishing && !after.publishing && before.generation == after.generation {
                return result;
            }
        }
        let message = "a refresh is publishing; retry later".to_string();
        Err(QueryError::Unavailable {
            detail: message.clone(),
            message,
        })
    }

    /// A slot to run a query in, or `None` when all are taken.
    pub(super) fn slot(&self) -> Option<Slot<'_>> {
        self.running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.concurrency).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(&self.running))
    }

    /// Run `lib` once, at most `limit` rows and never more than `max_rows`.
    fn attempt(
        &self,
        catalog: &Catalog,
        lib: &Library,
        format: Format,
        limit: Option<usize>,
    ) -> Result<Output, QueryError> {
        let cap = limit.unwrap_or(usize::MAX).min(self.max_rows);
        if let Root::Local(data) = &self.root {
            self.check_inputs(data, catalog, lib)?;
        }
        let root = self.root.prefix();
        let bind = |a: &Artifact| Ok(bind_table(&root, &self.strip_prefix, a));
        let sql = compose(lib, "duckdb", catalog, &bind)
            .map_err(|e| QueryError::Internal(format!("{e:#}")))?;
        let tmp = tempfile::tempdir()
            .map_err(|e| QueryError::Internal(format!("creating a temp directory: {e}")))?;
        let count = tmp.path().join("n.csv");
        let out = tmp.path().join("out");
        // duckdb stops at the first error, so without this file the setup
        // is what failed.
        let set_up = tmp.path().join("setup.ok");
        // The setup ends with its own line and a `;`, so one whose last
        // statement has no `;`, or that ends in a `--` comment, still ends
        // there; an empty statement is nothing to duckdb.
        let mut script = match &self.setup {
            Some(setup) => format!("{setup}\n;\n"),
            None => String::new(),
        };
        script.push_str(&format!(
            "COPY (SELECT 1) TO {} (HEADER false);\n\
             CREATE TEMP TABLE r AS SELECT * FROM ({sql}) AS q LIMIT {};\n\
             COPY (SELECT count(*) FROM r) TO {} (HEADER false);\n\
             COPY (SELECT * FROM r LIMIT {cap}) TO {} ({});\n",
            sql_path(&set_up),
            cap.saturating_add(1),
            sql_path(&count),
            sql_path(&out),
            format.copy_options()
        ));
        self.duckdb(tmp.path(), &script, &set_up)?;
        let read = |p: &Path| {
            fs::read(p).map_err(|e| QueryError::Internal(format!("reading {}: {e}", p.display())))
        };
        let n: usize = String::from_utf8_lossy(&read(&count)?)
            .trim()
            .parse()
            .map_err(|e| QueryError::Internal(format!("reading the row count: {e}")))?;
        let rows = n.min(cap);
        let mut body = read(&out)?;
        // duckdb writes an array of no rows over several lines.
        if format == Format::Json && rows == 0 {
            body = b"[]".to_vec();
        }
        Ok(Output {
            body,
            rows,
            truncated_at: (n > cap && limit.is_none_or(|l| l > self.max_rows))
                .then_some(self.max_rows),
        })
    }

    /// Every table `lib` reads, through the SQLViews it inlines, has a
    /// directory in local warehouse `data`, as the libraries step checks.
    fn check_inputs(
        &self,
        data: &Path,
        catalog: &Catalog,
        lib: &Library,
    ) -> Result<(), QueryError> {
        for dep in &lib.depends_on {
            let input = catalog
                .get(&dep.canonical, dep.version.as_deref())
                .map_err(|e| QueryError::Internal(format!("{e:#}")))?;
            match (input_table(input, &self.strip_prefix), input) {
                (None, Artifact::Library(view)) => self.check_inputs(data, catalog, view)?,
                (None, Artifact::View(_)) => {}
                (Some(table), _) => {
                    if !has_parquet(&data.join("parquet").join(&table)) {
                        return Err(QueryError::Unavailable {
                            message: NOT_AVAILABLE.to_string(),
                            detail: format!(
                                "Library {} input {} has no table {table} yet",
                                lib.name, dep.label
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Run `script` with duckdb in `dir`, killing it after the timeout or
    /// when serve stops. `set_up` is the file the script writes once the
    /// setup SQL has run.
    fn duckdb(&self, dir: &Path, script: &str, set_up: &Path) -> Result<(), QueryError> {
        let internal = |what: &str, e: std::io::Error| QueryError::Internal(format!("{what}: {e}"));
        // stderr goes to a file, so a long error cannot fill a pipe and
        // stall duckdb while it is waited for.
        let err_path = dir.join("stderr");
        let err_file = fs::File::create(&err_path).map_err(|e| internal("creating stderr", e))?;
        // The script goes on stdin, not in the arguments, which anyone on
        // the machine can read (`ps`) and which have a size limit. `-bail`
        // stops duckdb at the first error, as `-c` did.
        let mut command = Command::new(&self.duckdb);
        command
            .args(["-init", NO_INIT, "-bail"])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(err_file);
        for name in &self.hidden_env {
            command.env_remove(name);
        }
        let mut child = command
            .spawn()
            .map_err(|e| internal(&format!("running {}", self.duckdb), e))?;
        let pid = child.id();
        {
            let mut c = lock(&self.children);
            if c.stopping {
                drop(c);
                let _ = child.kill();
                let _ = child.wait();
                return Err(QueryError::Stopping);
            }
            c.pids.insert(pid);
        }
        #[cfg(test)]
        {
            *self.last_pid.lock().unwrap() = Some(pid);
        }
        // Written on a thread of its own: duckdb reads the script as it
        // runs it, so a long one could block the write past the timeout.
        // The write fails once duckdb has exited.
        let writer = child.stdin.take().map(|mut stdin| {
            let script = script.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(script.as_bytes());
            })
        });
        let start = Instant::now();
        let status = loop {
            match self.try_reap(&mut child) {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() >= self.timeout => {
                    self.kill(&mut child);
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    self.kill(&mut child);
                    return Err(internal("waiting for duckdb", e));
                }
            }
        };
        if let Some(w) = writer {
            let _ = w.join();
        }
        let Some(status) = status else {
            let message = format!("the query timed out after {} s", self.timeout.as_secs());
            return Err(QueryError::Sql {
                detail: message.clone(),
                message,
            });
        };
        if status.success() {
            return Ok(());
        }
        if self.stopping() {
            return Err(QueryError::Stopping);
        }
        // Told apart on duckdb's own text, before anything is taken out.
        let stderr = fs::read_to_string(&err_path).unwrap_or_default();
        if self.setup.is_some() && !set_up.exists() {
            return Err(QueryError::Sql {
                message: SETUP_FAILED.to_string(),
                detail: setup_failure_detail(&stderr),
            });
        }
        if matches!(self.root, Root::Remote(_)) && stderr.contains("No files found") {
            return Err(QueryError::Unavailable {
                message: NOT_AVAILABLE.to_string(),
                detail: "an input table has no files yet".to_string(),
            });
        }
        Err(QueryError::Sql {
            message: "the query failed; the serve log has the details".to_string(),
            detail: without_setup(stderr.trim(), self.setup.as_deref().unwrap_or_default()),
        })
    }

    /// `child`'s status once it has exited, reaped under the lock that
    /// `stop` signals under, so its pid is never signalled once reaped.
    fn try_reap(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        let mut c = lock(&self.children);
        let status = child.try_wait();
        if !matches!(status, Ok(None)) {
            c.pids.remove(&child.id());
        }
        status
    }

    /// Kill `child` and reap it.
    fn kill(&self, child: &mut Child) {
        let mut c = lock(&self.children);
        let _ = child.kill();
        let _ = child.wait();
        c.pids.remove(&child.id());
    }
}

/// What a caller is told when the setup SQL failed.
const SETUP_FAILED: &str = "the duckdb_setup SQL failed";

/// The log's detail for a failed setup: only the kind of error duckdb gave
/// (`Conversion Error`, `IO Error`, …). The rest of the message can hold a
/// value the setup computed from its keys, such as an `E'\x..'` string
/// decoded, which no match against the setup's text can find.
fn setup_failure_detail(stderr: &str) -> String {
    let first = stderr.trim().lines().next().unwrap_or_default();
    match first.split_once(": ") {
        Some((kind, _))
            if kind.ends_with(" Error")
                && kind.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') =>
        {
            format!("{SETUP_FAILED}: {kind}")
        }
        _ => SETUP_FAILED.to_string(),
    }
}

/// Whether `dir` has a Parquet file in it, at any depth.
fn has_parquet(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        if path.is_dir() {
            has_parquet(&path)
        } else {
            path.extension().is_some_and(|x| x == "parquet")
        }
    })
}

/// The shortest run of the setup's characters that marks a line of an
/// error as quoting it.
const QUOTED_RUN: usize = 6;

/// `s` with each run of whitespace made one space.
fn collapsed(s: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            if out.last() != Some(&' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` without any line that holds `QUOTED_RUN` characters of `setup` in
/// a row, wherever they are in it (a string, a comment, a keyword), with
/// whitespace runs made one space in both. The setup is not parsed: a key
/// can be in any kind of string, and duckdb can quote a long one cut short.
/// A run of only whitespace and punctuation (`...`, `');  `) is no reason
/// to drop a line. What is left has the setup's single-quoted strings
/// blanked, for a key shorter than the run.
fn without_setup(text: &str, setup: &str) -> String {
    let chars = collapsed(setup);
    let runs: HashSet<&[char]> = chars
        .windows(QUOTED_RUN)
        .filter(|w| w.iter().any(|c| c.is_alphanumeric()))
        .collect();
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            !collapsed(line)
                .windows(QUOTED_RUN)
                .any(|w| runs.contains(w))
        })
        .collect();
    let kept = kept.join("\n");
    if kept.trim().is_empty() {
        return "duckdb failed; its message quoted the duckdb_setup SQL".to_string();
    }
    crate::redact::sql_strings(&kept, setup)
}

/// A file with no SQL, given to duckdb's `-init`, so the user's
/// `~/.duckdbrc` does not run first.
const NO_INIT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::query::testing::{Warehouse, have_duckdb, lines, rows};
    use serde_json::Value;

    #[test]
    fn runs_through_a_sqlview_in_order() {
        if !have_duckdb("runs_through_a_sqlview_in_order") {
            return;
        }
        let w = Warehouse::new();
        let out = w.run(&w.runner(100, None), Format::Ndjson, None);
        assert_eq!(lines(&out.body), rows());
        assert_eq!(out.rows, 3);
        assert_eq!(out.truncated_at, None);
    }

    #[test]
    fn formats() {
        if !have_duckdb("formats") {
            return;
        }
        let w = Warehouse::new();
        let r = w.runner(100, None);
        let json: Value = serde_json::from_slice(&w.run(&r, Format::Json, None).body).unwrap();
        assert_eq!(json, Value::Array(rows()));
        let csv = String::from_utf8(w.run(&r, Format::Csv, None).body).unwrap();
        assert_eq!(csv.lines().next(), Some("id,n"), "{csv}");
        assert_eq!(csv.lines().count(), 4, "{csv}");
        assert_eq!(Format::Csv.media_type(), "text/csv");
    }

    #[test]
    fn no_rows_is_an_empty_array_or_body() {
        if !have_duckdb("no_rows_is_an_empty_array_or_body") {
            return;
        }
        let w = Warehouse::new();
        let r = w.runner(100, None);
        let out = w.run(&r, Format::Json, Some(0));
        assert_eq!((out.body.as_slice(), out.rows), (&b"[]"[..], 0));
        assert!(w.run(&r, Format::Ndjson, Some(0)).body.is_empty());
        let csv = w.run(&r, Format::Csv, Some(0)).body;
        assert_eq!(String::from_utf8(csv).unwrap().trim(), "id,n");
    }

    #[test]
    fn truncation_is_reported_only_past_max_rows() {
        if !have_duckdb("truncation_is_reported_only_past_max_rows") {
            return;
        }
        let w = Warehouse::new();
        let out = w.run(&w.runner(3, None), Format::Ndjson, None);
        assert_eq!((out.rows, out.truncated_at), (3, None));
        let r = w.runner(2, None);
        let out = w.run(&r, Format::Ndjson, None);
        assert_eq!((out.rows, out.truncated_at), (2, Some(2)));
        assert_eq!(lines(&out.body), rows()[..2]);
        let out = w.run(&r, Format::Ndjson, Some(1));
        assert_eq!((out.rows, out.truncated_at), (1, None));
        assert_eq!(lines(&out.body).len(), 1);
        // The caller's _limit cut it, not max_rows, even at max_rows.
        let out = w.run(&r, Format::Ndjson, Some(2));
        assert_eq!((out.rows, out.truncated_at), (2, None));
    }

    #[test]
    fn a_table_with_no_parquet_files_is_unavailable() {
        if !have_duckdb("a_table_with_no_parquet_files_is_unavailable") {
            return;
        }
        let w = Warehouse::new();
        fs::remove_file(w.data().join("parquet/task/country=UGA/part-0.parquet")).unwrap();
        match w
            .runner(100, None)
            .run(&w.catalog, w.lib("Q"), Format::Ndjson, None)
        {
            Err(QueryError::Unavailable { message, detail }) => {
                assert_eq!(message, "an input table is not available yet; retry later");
                assert!(detail.contains("no table task yet"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_query_past_the_timeout_is_stopped() {
        if !have_duckdb("a_query_past_the_timeout_is_stopped") {
            return;
        }
        let w = Warehouse::new();
        let mut cfg = w.config(100, None);
        cfg.timeout = Duration::from_millis(1);
        let r = Runner::new(&cfg, &w.data(), "duckdb", &[]).unwrap();
        match r.run(&w.catalog, w.lib("Slow"), Format::Ndjson, None) {
            Err(QueryError::Sql { message, .. }) => {
                assert_eq!(message, "the query timed out after 0 s")
            }
            other => panic!("{other:?}"),
        }
        // Killed and reaped: a zombie would still take a signal.
        let pid = r.last_pid.lock().unwrap().unwrap();
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "duckdb {pid} is still there");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_duckdb_is_found_from_the_working_directory() {
        if !have_duckdb("a_relative_duckdb_is_found_from_the_working_directory") {
            return;
        }
        let w = Warehouse::new();
        // A link to duckdb, named by a path relative to the working
        // directory.
        let link = w.dir.path().join("tools/duckdb");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|d| d.join("duckdb"))
            .find(|p| p.is_file())
            .unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = format!("{up}{}", link.display().to_string().trim_start_matches('/'));
        let r = Runner::new(&w.config(100, None), &w.data(), &relative, &[]).unwrap();
        assert!(Path::new(&r.duckdb).is_absolute(), "{}", r.duckdb);
        assert_eq!(w.run(&r, Format::Ndjson, None).rows, 3);
        let bare = Runner::new(&w.config(100, None), &w.data(), "duckdb", &[]).unwrap();
        assert_eq!(bare.duckdb, "duckdb");
    }

    #[test]
    fn a_missing_table_is_unavailable() {
        if !have_duckdb("a_missing_table_is_unavailable") {
            return;
        }
        let w = Warehouse::new();
        fs::remove_dir_all(w.data().join("parquet/task")).unwrap();
        match w
            .runner(100, None)
            .run(&w.catalog, w.lib("Q"), Format::Ndjson, None)
        {
            Err(QueryError::Unavailable { message, detail }) => {
                assert_eq!(message, "an input table is not available yet; retry later");
                assert!(detail.contains("no table task yet"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_sql_is_a_sql_error() {
        if !have_duckdb("bad_sql_is_a_sql_error") {
            return;
        }
        let w = Warehouse::new();
        match w
            .runner(100, None)
            .run(&w.catalog, w.lib("Bad"), Format::Ndjson, None)
        {
            Err(QueryError::Sql { message, detail }) => {
                assert_eq!(message, "the query failed; the serve log has the details");
                assert!(detail.contains("nope"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setup_secrets_are_redacted_from_errors() {
        if !have_duckdb("setup_secrets_are_redacted_from_errors") {
            return;
        }
        let w = Warehouse::new();
        let long = "a-long-secret-".repeat(10);
        let secrets = ["AKIAXYZ", "shh", "supersecretkey", long.as_str()];
        let failed = "the query failed; the serve log has the details";
        let setup_failed = "the duckdb_setup SQL failed";
        // The caller's message is fixed text whatever duckdb says. A bad
        // setup, which duckdb quotes keys and all (a long string cut short),
        // says only that; a setup that runs, with keys duckdb can still
        // quote in the query's error, gives the query's failure.
        for (setup, lib, says) in [
            (
                "CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET 'shh');".to_string(),
                "Bad",
                failed,
            ),
            (
                "CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET 'shh' BOGUS);".to_string(),
                "Q",
                setup_failed,
            ),
            (
                "CREATE SECRET s (TYPE s3, KEY_ID $$AKIAXYZ$$, SECRET $$supersecretkey$$ BOGUS);"
                    .to_string(),
                "Q",
                setup_failed,
            ),
            (
                format!("CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET '{long}' BOGUS);"),
                "Q",
                setup_failed,
            ),
            // Keys the redaction of the log detail can miss: an escaped
            // E'' string, a literal after a quote in a comment, a literal
            // over several lines.
            (
                "SET VARIABLE v = E'\\x73\\x68\\x68';\nSELECT CAST(getvariable('v') AS INTEGER);"
                    .to_string(),
                "Q",
                setup_failed,
            ),
            (
                "SET VARIABLE v = E'\\x73\\x68\\x68';".to_string(),
                "Bad",
                failed,
            ),
            (
                "-- it's\nSET VARIABLE v = 'supersecretkey';\n\
                 SELECT CAST(getvariable('v') AS INTEGER);"
                    .to_string(),
                "Q",
                setup_failed,
            ),
            (
                "SET VARIABLE v = 'supersecretkey\nAKIAXYZ';\n\
                 SELECT CAST(getvariable('v') AS INTEGER);"
                    .to_string(),
                "Q",
                setup_failed,
            ),
        ] {
            let r = w.runner(100, Some(&setup));
            match r.run(&w.catalog, w.lib(lib), Format::Ndjson, None) {
                Err(QueryError::Sql { message, detail }) => {
                    assert_eq!(message, says, "{setup}");
                    for secret in secrets {
                        assert!(!message.contains(secret), "{setup}: {message}");
                        assert!(!detail.contains(secret), "{setup}: {detail}");
                    }
                    if says == failed {
                        assert!(detail.contains("nope"), "{setup}: {detail}");
                    } else {
                        assert!(detail.starts_with(setup_failed), "{setup}: {detail}");
                    }
                }
                other => panic!("{setup}: {other:?}"),
            }
        }
    }

    #[test]
    fn no_files_at_a_remote_source_is_unavailable() {
        if !have_duckdb("no_files_at_a_remote_source_is_unavailable") {
            return;
        }
        let w = Warehouse::new();
        // duckdb reads a path as it reads a remote prefix; this one has no
        // files, and the setup names it, so the line saying so quotes it.
        let empty = w.dir.path().join("nothing-here");
        let mut cfg = w.config(
            100,
            Some(&format!("SET VARIABLE p = '{}';", empty.display())),
        );
        cfg.source = Source::Remote(empty.display().to_string());
        let r = Runner::new(&cfg, &w.data(), "duckdb", &[]).unwrap();
        match r.run(&w.catalog, w.lib("Q"), Format::Ndjson, None) {
            Err(QueryError::Unavailable { message, detail }) => {
                assert_eq!(message, "an input table is not available yet; retry later");
                assert_eq!(detail, "an input table has no files yet");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_query_error_drops_lines_that_quote_the_setup() {
        // An apostrophe in a comment, a literal over several lines, and a
        // run of dots that is no reason to drop a line.
        let setup = "-- it's mine\nSET VARIABLE v = 'supersecretkey\n   AKIAXYZ'; -- ......";
        let text = "Conversion Error: could not convert\nstring 'supersecretkey  AKIAXYZ'\n\
                    near XYZ'; -- and\nkey AKIAXY refused\nCandidate: ...... nope";
        assert_eq!(
            without_setup(text, setup),
            "Conversion Error: could not convert\nCandidate: ...... nope"
        );
        assert_eq!(
            without_setup("key AKIAXYZ", setup),
            "duckdb failed; its message quoted the duckdb_setup SQL"
        );
        // A quoted literal of the setup shorter than the window is blanked.
        assert_eq!(
            without_setup("zone eu1 refused", "SET s3_region = 'eu1';"),
            "zone *** refused"
        );
        assert_eq!(without_setup("Binder Error: x", ""), "Binder Error: x");
    }

    #[test]
    fn a_failed_setup_logs_only_the_kind_of_error() {
        assert_eq!(
            setup_failure_detail(
                "Conversion Error: Could not convert string 'shh' to INT32\nLINE 1: x"
            ),
            "the duckdb_setup SQL failed: Conversion Error"
        );
        assert_eq!(
            setup_failure_detail("shh: not an error type"),
            "the duckdb_setup SQL failed"
        );
    }

    /// A fake duckdb that writes its arguments to `args` and runs the real
    /// one; with `ignore_term`, SIGTERM does not stop it.
    #[cfg(unix)]
    fn recording_duckdb(w: &Warehouse, args: &Path, ignore_term: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = w.dir.path().join("fake-duckdb");
        let trap = if ignore_term { "trap '' TERM\n" } else { "" };
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\n{trap}printf '%s\\n' \"$@\" > '{}'\nexec duckdb \"$@\"\n",
                args.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    #[cfg(unix)]
    #[test]
    fn the_script_goes_on_stdin_not_in_the_arguments() {
        if !have_duckdb("the_script_goes_on_stdin_not_in_the_arguments") {
            return;
        }
        let w = Warehouse::new();
        let args = w.dir.path().join("args");
        let fake = recording_duckdb(&w, &args, false);
        // Setups with no final `;`, one ending in a comment.
        for setup in [
            "SET VARIABLE marker = 'setup-text-in-argv'",
            "SET VARIABLE marker = 'setup-text-in-argv' -- no semicolon",
        ] {
            let cfg = w.config(100, Some(setup));
            let r = Runner::new(&cfg, &w.data(), fake.to_str().unwrap(), &[]).unwrap();
            assert_eq!(w.run(&r, Format::Ndjson, None).rows, 3, "{setup}");
            let argv = fs::read_to_string(&args).unwrap();
            assert!(!argv.contains("setup-text-in-argv"), "{argv}");
            assert!(!argv.contains("COPY"), "{argv}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stopping_ends_running_queries_and_refuses_new_ones() {
        if !have_duckdb("stopping_ends_running_queries_and_refuses_new_ones") {
            return;
        }
        let w = Warehouse::new();
        // A duckdb that SIGTERM does not stop: SIGKILL does, after the grace.
        let fake = recording_duckdb(&w, &w.dir.path().join("args"), true);
        let mut cfg = w.config(100, None);
        cfg.timeout = Duration::from_secs(3600);
        let r = Runner::new(&cfg, &w.data(), fake.to_str().unwrap(), &[]).unwrap();
        std::thread::scope(|s| {
            let running = s.spawn(|| r.run(&w.catalog, w.lib("Slow"), Format::Ndjson, None));
            let started = Instant::now();
            while r.last_pid.lock().unwrap().is_none() {
                assert!(started.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(200));
            let stopped = Instant::now();
            r.stop(Duration::from_millis(500));
            match running.join().unwrap() {
                Err(QueryError::Stopping) => {}
                other => panic!("{other:?}"),
            }
            assert!(stopped.elapsed() < Duration::from_secs(5));
        });
        assert!(matches!(
            r.run(&w.catalog, w.lib("Q"), Format::Ndjson, None),
            Err(QueryError::Stopping)
        ));
    }

    #[test]
    fn a_publish_during_the_query_runs_it_again() {
        if !have_duckdb("a_publish_during_the_query_runs_it_again") {
            return;
        }
        let w = Warehouse::new();
        let marker = w.data().join(".staging/.publishing");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "").unwrap();
        match w
            .runner(100, None)
            .run(&w.catalog, w.lib("Q"), Format::Ndjson, None)
        {
            Err(QueryError::Unavailable { message, .. }) => {
                assert_eq!(message, "a refresh is publishing; retry later")
            }
            other => panic!("{other:?}"),
        }
        fs::remove_file(&marker).unwrap();

        let r = w.runner(100, None);
        let generation = w.data().join(".publish-generation");
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = runs.clone();
        *r.after_run.lock().unwrap() = Some(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            fs::write(&generation, "1\n").unwrap();
        }));
        let out = w.run(&r, Format::Ndjson, None);
        assert_eq!(lines(&out.body), rows());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(publish_state(&w.data()).generation, 1);
    }

    #[test]
    fn busy_when_every_slot_is_taken() {
        if !have_duckdb("busy_when_every_slot_is_taken") {
            return;
        }
        let w = Warehouse::new();
        let r = w.runner(100, None);
        let held = r.slot().unwrap();
        assert!(matches!(
            r.run(&w.catalog, w.lib("Q"), Format::Ndjson, None),
            Err(QueryError::Busy)
        ));
        drop(held);
        assert_eq!(w.run(&r, Format::Ndjson, None).rows, 3);
    }
}
