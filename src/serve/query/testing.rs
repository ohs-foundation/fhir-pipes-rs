//! A small warehouse for the query API's tests.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::config::{Approve, QueryConfig, Source};
use super::run::{Format, Output, Runner};
use crate::library::resolve::{Artifact, Catalog};
use crate::library::{Library, SQLQUERY_PROFILE, SQLVIEW_PROFILE};
use crate::warehouse::config::Settings;
use crate::warehouse::sql::sql_path;

pub fn have_duckdb(skipped: &str) -> bool {
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
pub fn library(name: &str, view: bool, deps: &[(&str, &str)], sql: &str) -> Value {
    let related: Vec<Value> = deps
        .iter()
        .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
        .collect();
    json!({
        "resourceType": "Library",
        "meta": {"profile": [if view { SQLVIEW_PROFILE } else { SQLQUERY_PROFILE }]},
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
/// SQLQuery `Bad`, which names a column that is not there. Every Library
/// is a draft.
pub struct Warehouse {
    pub dir: tempfile::TempDir,
    /// The catalog as it was made; `settings` reads `defs/` again.
    pub catalog: Catalog,
}

impl Warehouse {
    pub fn new() -> Warehouse {
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
        let slow = library(
            "Slow",
            false,
            &[],
            "SELECT sum(range) FROM range(100000000000)",
        );
        let mut w = Warehouse {
            dir,
            catalog: Catalog::default(),
        };
        for (f, v) in [
            ("v", view),
            ("big", big),
            ("q", q),
            ("bad", bad),
            ("slow", slow),
        ] {
            w.write(f, &v);
        }
        let sources = [w.defs().display().to_string()];
        w.catalog = Catalog::load(&sources, w.dir.path(), &[]).unwrap();
        w
    }

    pub fn data(&self) -> PathBuf {
        self.dir.path().join("wh")
    }

    pub fn defs(&self) -> PathBuf {
        self.dir.path().join("defs")
    }

    /// Write `v` to `defs/<file>.json`.
    pub fn write(&self, file: &str, v: &Value) {
        fs::create_dir_all(self.defs()).unwrap();
        fs::write(self.defs().join(format!("{file}.json")), v.to_string()).unwrap();
    }

    /// Give the Library in `defs/<file>.json` status `status`.
    pub fn set_status(&self, file: &str, status: &str) {
        let path = self.defs().join(format!("{file}.json"));
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v["status"] = json!(status);
        self.write(file, &v);
    }

    /// The warehouse settings of a config naming `wh/` and `defs/`.
    pub fn settings(&self) -> Settings {
        let file = toml::from_str(&format!(
            "data = {:?}\nviews = {:?}\n",
            self.data().display().to_string(),
            self.defs().display().to_string()
        ))
        .unwrap();
        crate::warehouse::config::settings(file, self.dir.path().to_path_buf(), Default::default())
            .unwrap()
    }

    pub fn lib(&self, name: &str) -> &Library {
        match self
            .catalog
            .get(&format!("https://example.org/Library/{name}"), None)
            .unwrap()
        {
            Artifact::Library(l) => l,
            Artifact::View(_) => panic!("{name} is a view"),
        }
    }

    pub fn runner(&self, max_rows: usize, setup: Option<&str>) -> Runner {
        Runner::new(&self.config(max_rows, setup), &self.data(), "duckdb", &[]).unwrap()
    }

    pub fn config(&self, max_rows: usize, setup: Option<&str>) -> QueryConfig {
        let duckdb_setup = setup.map(|sql| {
            let p = self.dir.path().join("setup.sql");
            fs::write(&p, sql).unwrap();
            p
        });
        QueryConfig {
            approve: Approve::All,
            source: Source::Local,
            duckdb_setup,
            timeout: Duration::from_secs(30),
            max_rows,
            concurrency: 1,
        }
    }

    pub fn run(&self, runner: &Runner, format: Format, limit: Option<usize>) -> Output {
        match runner.run(&self.catalog, self.lib("Q"), format, limit) {
            Ok(o) => o,
            Err(e) => panic!("{e:?}"),
        }
    }
}

pub fn lines(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// `Q`'s rows.
pub fn rows() -> Vec<Value> {
    vec![
        json!({"id": "d", "n": 4}),
        json!({"id": "c", "n": 3}),
        json!({"id": "b", "n": 2}),
    ]
}
