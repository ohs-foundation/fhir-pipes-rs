//! Running an approved SQLQuery Library for `$sql-run`: compose its SQL with
//! each input read from the warehouse's Parquet, run it with the `duckdb`
//! command, and return its rows as NDJSON, JSON or CSV.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// quote the setup SQL's keys; `detail` is for serve's log: duckdb's
    /// error, with what it quotes of the setup SQL taken out as far as
    /// `without_setup` can.
    Sql {
        message: String,
        detail: String,
    },
    /// Every slot is running a query.
    Busy,
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
            #[cfg(test)]
            after_run: std::sync::Mutex::new(None),
            #[cfg(test)]
            last_pid: std::sync::Mutex::new(None),
        })
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
        let _slot = self.slot().ok_or(QueryError::Busy)?;
        let Root::Local(data) = &self.root else {
            return self.attempt(catalog, lib, format, limit);
        };
        for _ in 0..2 {
            let before = publish_state(data);
            let result = self.attempt(catalog, lib, format, limit);
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
        let mut script = self.setup.clone().unwrap_or_default();
        script.push_str(&format!(
            "\nCOPY (SELECT 1) TO {} (HEADER false);\n\
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

    /// Run `script` with duckdb in `dir`, killing it after the timeout.
    /// `set_up` is the file the script writes once the setup SQL has run.
    fn duckdb(&self, dir: &Path, script: &str, set_up: &Path) -> Result<(), QueryError> {
        let internal = |what: &str, e: std::io::Error| QueryError::Internal(format!("{what}: {e}"));
        // stderr goes to a file, so a long error cannot fill a pipe and
        // stall duckdb while it is waited for.
        let err_path = dir.join("stderr");
        let err_file = fs::File::create(&err_path).map_err(|e| internal("creating stderr", e))?;
        let mut child = Command::new(&self.duckdb)
            .args(["-init", NO_INIT, "-c", script])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(err_file)
            .spawn()
            .map_err(|e| internal(&format!("running {}", self.duckdb), e))?;
        let registered = crate::stop::register(child.id());
        #[cfg(test)]
        {
            *self.last_pid.lock().unwrap() = Some(child.id());
        }
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() >= self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(registered);
                    return Err(internal("waiting for duckdb", e));
                }
            }
        };
        drop(registered);
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
        // Told apart on duckdb's own text, before anything is taken out.
        let stderr = fs::read_to_string(&err_path).unwrap_or_default();
        let message = if self.setup.is_some() && !set_up.exists() {
            "the duckdb_setup SQL failed"
        } else if matches!(self.root, Root::Remote(_)) && stderr.contains("No files found") {
            return Err(QueryError::Unavailable {
                message: NOT_AVAILABLE.to_string(),
                detail: "an input table has no files yet".to_string(),
            });
        } else {
            "the query failed; the serve log has the details"
        };
        Err(QueryError::Sql {
            message: message.to_string(),
            detail: without_setup(stderr.trim(), self.setup.as_deref().unwrap_or_default()),
        })
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

/// The shortest run of a setup literal's characters that marks a line of an
/// error as quoting it.
const QUOTED_RUN: usize = 6;

/// `text` with no part of a string in `setup` (`'…'`, `E'…'`, `$$…$$` or
/// `$tag$…$tag$`), where a key would be: a line holding `QUOTED_RUN`
/// characters of a longer string in a row is dropped, since duckdb can cut a
/// long string short where it quotes it, and a shorter string is blanked.
fn without_setup(text: &str, setup: &str) -> String {
    let literals = setup_literals(setup);
    let quotes = |line: &str| {
        literals.iter().any(|l| {
            let chars: Vec<char> = l.chars().collect();
            chars.len() >= QUOTED_RUN
                && chars
                    .windows(QUOTED_RUN)
                    .any(|w| line.contains(&w.iter().collect::<String>()))
        })
    };
    let mut kept: Vec<String> = Vec::new();
    for line in text.lines() {
        if quotes(line) {
            continue;
        }
        let mut line = line.to_string();
        for l in &literals {
            if !l.trim().is_empty() {
                line = line.replace(l.as_str(), "***");
            }
        }
        kept.push(line);
    }
    let kept = kept.join("\n");
    if kept.trim().is_empty() {
        return "duckdb failed; its message quoted the duckdb_setup SQL".to_string();
    }
    kept
}

/// The contents of the string literals in `sql`: single-quoted (with `''`
/// for a quote, and `E'…'` strings, whose `\'` is a quote too) and
/// dollar-quoted.
fn setup_literals(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' => {
                let escapes = i > 0 && matches!(chars[i - 1], 'E' | 'e');
                let mut lit = String::new();
                i += 1;
                while i < chars.len() {
                    match chars[i] {
                        '\\' if escapes && i + 1 < chars.len() => {
                            lit.push(chars[i + 1]);
                            i += 2;
                        }
                        '\'' if chars.get(i + 1) == Some(&'\'') => {
                            lit.push('\'');
                            i += 2;
                        }
                        '\'' => break,
                        c => {
                            lit.push(c);
                            i += 1;
                        }
                    }
                }
                found.push(lit);
                i += 1;
            }
            '$' => {
                let tag_end = chars[i + 1..]
                    .iter()
                    .position(|c| !(c.is_alphanumeric() || *c == '_'))
                    .map(|n| i + 1 + n);
                match tag_end {
                    Some(end) if chars[end] == '$' => {
                        let tag: String = chars[i..=end].iter().collect();
                        let rest: String = chars[end + 1..].iter().collect();
                        match rest.find(&tag) {
                            Some(close) => {
                                found.push(rest[..close].to_string());
                                i = end + 1 + rest[..close].chars().count() + tag.chars().count();
                            }
                            None => {
                                found.push(rest);
                                i = chars.len();
                            }
                        }
                    }
                    _ => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    found
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
                Err(QueryError::Sql { message, .. }) => {
                    assert_eq!(message, says, "{setup}");
                    for secret in secrets {
                        assert!(!message.contains(secret), "{setup}: {message}");
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
        let long = "a-long-secret-".repeat(10);
        let setup = format!(
            "CREATE SECRET s (KEY_ID $k$AKIAXYZ$k$, SECRET E'it\\'s {long}', REGION 'eu');"
        );
        let text = "IO Error: HTTP 403\nLINE 1: ...ecret-a-long-secret-'\nkey AKIAXYZ refused\n\
                    region eu\nCandidate: nope";
        assert_eq!(
            without_setup(text, &setup),
            "IO Error: HTTP 403\nregion ***\nCandidate: nope"
        );
        assert_eq!(
            without_setup("key AKIAXYZ", &setup),
            "duckdb failed; its message quoted the duckdb_setup SQL"
        );
        assert_eq!(
            setup_literals("SELECT 'a''b', $$c$$, $1, E'd\\'e', $t$f$$g$t$"),
            ["a'b", "c", "d'e", "f$$g"]
        );
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
