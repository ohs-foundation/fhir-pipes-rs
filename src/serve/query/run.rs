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
// Read once the query API is wired in.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Ndjson,
    Json,
    Csv,
}

impl Format {
    // Read once the query API is wired in.
    #[allow(dead_code)]
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
// Read once the query API is wired in.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Output {
    pub body: Vec<u8>,
    pub rows: usize,
    pub truncated_at: Option<usize>,
}

/// Why a query gave no result.
// Read once the query API is wired in.
#[allow(dead_code)]
#[derive(Debug)]
pub enum QueryError {
    /// The data is not there to read yet, or is being replaced: an input
    /// table is missing, or a refresh published while the query ran.
    Unavailable(String),
    /// duckdb refused or failed the query, or it ran past the timeout.
    Sql(String),
    /// Every slot is running a query.
    Busy,
    Internal(String),
}

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
// Read once the query API is wired in.
#[allow(dead_code)]
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
}

/// A running query's slot, given back when dropped.
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

// Used once the query API is wired in.
#[allow(dead_code)]
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
        Ok(Runner {
            duckdb: duckdb.to_string(),
            root,
            setup,
            strip_prefix: strip_prefix.to_vec(),
            timeout: cfg.timeout,
            max_rows: cfg.max_rows,
            concurrency: cfg.concurrency,
            running: AtomicUsize::new(0),
            #[cfg(test)]
            after_run: std::sync::Mutex::new(None),
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
        let cap = limit.unwrap_or(usize::MAX).min(self.max_rows);
        let Root::Local(data) = &self.root else {
            return self.attempt(catalog, lib, format, cap);
        };
        for _ in 0..2 {
            let before = publish_state(data);
            let result = self.attempt(catalog, lib, format, cap);
            #[cfg(test)]
            if let Some(hook) = self.after_run.lock().unwrap().take() {
                hook();
            }
            let after = publish_state(data);
            if !before.publishing && !after.publishing && before.generation == after.generation {
                return result;
            }
        }
        Err(QueryError::Unavailable(
            "a refresh is publishing; retry".to_string(),
        ))
    }

    /// A slot to run a query in, or `None` when all are taken.
    fn slot(&self) -> Option<Slot<'_>> {
        self.running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.concurrency).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(&self.running))
    }

    /// Run `lib` once, at most `cap` rows.
    fn attempt(
        &self,
        catalog: &Catalog,
        lib: &Library,
        format: Format,
        cap: usize,
    ) -> Result<Output, QueryError> {
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
        let mut script = self.setup.clone().unwrap_or_default();
        script.push_str(&format!(
            "\nCREATE TEMP TABLE r AS SELECT * FROM ({sql}) AS q LIMIT {};\n\
             COPY (SELECT count(*) FROM r) TO {} (HEADER false);\n\
             COPY (SELECT * FROM r LIMIT {cap}) TO {} ({});\n",
            cap.saturating_add(1),
            sql_path(&count),
            sql_path(&out),
            format.copy_options()
        ));
        self.duckdb(tmp.path(), &script)?;
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
            truncated_at: (n > cap && cap == self.max_rows).then_some(self.max_rows),
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
                    if !data.join("parquet").join(&table).is_dir() {
                        return Err(QueryError::Unavailable(format!(
                            "Library {} input {} has no table {table} yet",
                            lib.name, dep.label
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Run `script` with duckdb in `dir`, killing it after the timeout.
    fn duckdb(&self, dir: &Path, script: &str) -> Result<(), QueryError> {
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
            return Err(QueryError::Sql(format!(
                "timed out after {} s",
                self.timeout.as_secs()
            )));
        };
        if status.success() {
            return Ok(());
        }
        let stderr = fs::read_to_string(&err_path).unwrap_or_default();
        let message =
            crate::redact::sql_strings(stderr.trim(), self.setup.as_deref().unwrap_or_default());
        if matches!(self.root, Root::Remote(_)) && message.contains("No files found") {
            return Err(QueryError::Unavailable(message));
        }
        Err(QueryError::Sql(message))
    }
}

/// A file with no SQL, given to duckdb's `-init`, so the user's
/// `~/.duckdbrc` does not run first.
const NO_INIT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::resolve::Artifact;
    use crate::serve::query::config::Approve;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use std::io::Write as _;

    const SQLVIEW: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";
    const SQLQUERY: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";

    fn have_duckdb(skipped: &str) -> bool {
        let found = Command::new("duckdb")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
        }
        found
    }

    /// A SQLView (`view`) or SQLQuery Library named `name`; `deps` are
    /// (label, canonical) pairs.
    fn library(name: &str, view: bool, deps: &[(&str, &str)], sql: &str) -> Value {
        let related: Vec<Value> = deps
            .iter()
            .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
            .collect();
        json!({
            "resourceType": "Library",
            "meta": {"profile": [if view { SQLVIEW } else { SQLQUERY }]},
            "url": format!("https://example.org/Library/{name}"),
            "name": name,
            "status": "draft",
            "relatedArtifact": related,
            "content": [{"contentType": "application/sql", "data": STANDARD.encode(sql)}],
        })
    }

    /// A warehouse in a temp dir: table `task` (`id`, `n` from 1 to 4) in
    /// `wh/`, and in `defs/` its ViewDefinition, the SQLView `TaskBig`
    /// (`n > 1`), the SQLQuery `Q` over it, highest `n` first, and the
    /// SQLQuery `Bad`, which names a column that is not there.
    struct Warehouse {
        dir: tempfile::TempDir,
        catalog: Catalog,
    }

    impl Warehouse {
        fn new() -> Warehouse {
            let dir = tempfile::tempdir().unwrap();
            let part = dir.path().join("wh/parquet/task/country=UGA");
            fs::create_dir_all(&part).unwrap();
            let out = Command::new("duckdb")
                .args(["-init", "/dev/null", "-c"])
                .arg(format!(
                    "COPY (SELECT * FROM (VALUES ('a', 1), ('b', 2), ('c', 3), ('d', 4)) \
                     AS t(id, n)) TO {} (FORMAT parquet);",
                    sql_path(&part.join("part-0.parquet"))
                ))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let defs = dir.path().join("defs");
            fs::create_dir_all(&defs).unwrap();
            let view = json!({
                "resourceType": "ViewDefinition",
                "url": "https://example.org/ViewDefinition/Task", "name": "Task",
                "resource": "Task", "status": "active",
                "select": [{"column": [{"name": "id", "path": "id"}]}],
            });
            let big = library(
                "TaskBig",
                true,
                &[("task", "https://example.org/ViewDefinition/Task")],
                "SELECT * FROM task WHERE n > 1",
            );
            let q = library(
                "Q",
                false,
                &[("big", "https://example.org/Library/TaskBig")],
                "SELECT id, n FROM big ORDER BY n DESC",
            );
            let bad = library(
                "Bad",
                false,
                &[("task", "https://example.org/ViewDefinition/Task")],
                "SELECT nope FROM task",
            );
            for (f, v) in [("v", view), ("big", big), ("q", q), ("bad", bad)] {
                fs::write(defs.join(format!("{f}.json")), v.to_string()).unwrap();
            }
            let sources = [defs.display().to_string()];
            let catalog = Catalog::load(&sources, dir.path(), &[]).unwrap();
            Warehouse { dir, catalog }
        }

        fn data(&self) -> PathBuf {
            self.dir.path().join("wh")
        }

        fn lib(&self, name: &str) -> &Library {
            match self
                .catalog
                .get(&format!("https://example.org/Library/{name}"), None)
                .unwrap()
            {
                Artifact::Library(l) => l,
                Artifact::View(_) => panic!("{name} is a view"),
            }
        }

        fn runner(&self, max_rows: usize, setup: Option<&str>) -> Runner {
            let duckdb_setup = setup.map(|sql| {
                let p = self.dir.path().join("setup.sql");
                fs::write(&p, sql).unwrap();
                p
            });
            let cfg = QueryConfig {
                approve: Approve::All,
                source: Source::Local,
                duckdb_setup,
                timeout: Duration::from_secs(30),
                max_rows,
                concurrency: 1,
            };
            Runner::new(&cfg, &self.data(), "duckdb", &[]).unwrap()
        }

        fn run(&self, runner: &Runner, format: Format, limit: Option<usize>) -> Output {
            match runner.run(&self.catalog, self.lib("Q"), format, limit) {
                Ok(o) => o,
                Err(e) => panic!("{e:?}"),
            }
        }
    }

    fn lines(body: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(body)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn rows() -> Vec<Value> {
        vec![
            json!({"id": "d", "n": 4}),
            json!({"id": "c", "n": 3}),
            json!({"id": "b", "n": 2}),
        ]
    }

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
            Err(QueryError::Unavailable(m)) => assert!(m.contains("no table task yet"), "{m}"),
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
            Err(QueryError::Sql(m)) => assert!(m.contains("nope"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setup_secrets_are_redacted_from_errors() {
        if !have_duckdb("setup_secrets_are_redacted_from_errors") {
            return;
        }
        let w = Warehouse::new();
        // A bad query, and a bad setup, whose error duckdb prints with the
        // line that holds the keys.
        for (setup, lib, quoted) in [
            (
                "CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET 'shh');",
                "Bad",
                "nope",
            ),
            (
                "CREATE SECRET s (TYPE s3, KEY_ID 'AKIAXYZ', SECRET 'shh' BOGUS);",
                "Q",
                "KEY_ID '***'",
            ),
        ] {
            let r = w.runner(100, Some(setup));
            match r.run(&w.catalog, w.lib(lib), Format::Ndjson, None) {
                Err(QueryError::Sql(m)) => {
                    assert!(!m.contains("AKIAXYZ") && !m.contains("shh"), "{m}");
                    assert!(m.contains(quoted), "{m}");
                }
                other => panic!("{other:?}"),
            }
        }
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
            Err(QueryError::Unavailable(m)) => assert!(m.contains("publishing"), "{m}"),
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
