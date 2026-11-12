//! The tables of a run: each view's rows and each SQLQuery Library's result
//! as one Parquet file, `<work>/tables/<table>.parquet`, named as the
//! warehouse names them (`StgTask` → `stg_task`). Libraries and data tests
//! run in duckdb over these files.
//!
//! Every duckdb script starts with `SET TimeZone = 'UTC'`, so a TIMESTAMPTZ
//! is written the same on every machine. duckdb's results are read back from
//! the files it writes, never from its standard output, whose format the
//! user's environment can change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fhir_pipes_sof::ViewResult;
use fhir_pipes_sof::output::OutputWriter;
use serde_json::Value;

use super::plan::PlannedCheck;
use crate::error::{PipesError, usage};
use crate::library::Library;
use crate::library::compose::{Bind, compose, strip_terminator};
use crate::library::resolve::{Artifact, Catalog};
use crate::views::run::View;
use crate::warehouse::partition::{open_parquet, parquet_writer};
use crate::warehouse::sql::{ident, sql_path};
use crate::warehouse::tools;

/// How many failing rows a data test reports.
pub const TEST_EXAMPLES: usize = 5;

pub struct Tables {
    dir: PathBuf,
    duckdb: String,
    /// Each table written, with its file.
    written: BTreeMap<String, PathBuf>,
}

/// What a data test found: how many rows fail, and the first few.
pub struct Failures {
    pub count: u64,
    pub examples: Vec<Value>,
}

impl Tables {
    pub fn new(work: &Path, duckdb: &str) -> Result<Self> {
        let dir = work.join("tables");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            duckdb: duckdb.to_string(),
            written: BTreeMap::new(),
        })
    }

    fn path(&self, table: &str) -> PathBuf {
        self.dir.join(format!("{table}.parquet"))
    }

    /// Writes a view's rows as its table. Returns the row count.
    pub fn write_view(&mut self, view: &View, result: ViewResult) -> Result<usize> {
        let path = self.path(&view.table);
        let rows = result.data.len();
        let mut file = crate::fs::create_private(&path).map_err(|e| PipesError::io(&path, e))?;
        parquet_writer()
            .write_with_summary(&result, &mut file)
            .with_context(|| format!("writing {}", path.display()))?;
        self.written.insert(view.table.clone(), path);
        Ok(rows)
    }

    /// Runs a SQLQuery Library over the tables written so far and writes its
    /// result as its table. Returns the row count.
    pub fn run_query(&mut self, lib: &Library, catalog: &Catalog) -> Result<u64> {
        let bind = |a: &Artifact| -> Result<Option<Bind>> {
            if let Artifact::Library(l) = a
                && l.kind == crate::library::Kind::SqlView
            {
                return Ok(None);
            }
            let table = a.table(&[]);
            let path = self
                .written
                .get(&table)
                .ok_or_else(|| anyhow!("{} has no table {table} in this run", a.url()))?;
            Ok(Some(Bind::Relation(format!(
                "read_parquet({})",
                sql_path(path)
            ))))
        };
        let sql = compose(lib, "duckdb", catalog, &bind).map_err(usage)?;
        let table = lib.table(&[]);
        let path = self.path(&table);
        self.script(&format!(
            "COPY ({sql}\n) TO {} (FORMAT PARQUET);",
            sql_path(&path)
        ))
        .with_context(|| format!("running Library {}", lib.name))?;
        private(&path)?;
        let rows = open_parquet(&path)?.metadata().file_metadata().num_rows();
        self.written.insert(table, path);
        Ok(u64::try_from(rows).unwrap_or(0))
    }

    /// A table's rows as NDJSON at `out`, in the order they are in the table.
    pub fn export_rows(&self, table: &str, out: &Path) -> Result<()> {
        let path = self
            .written
            .get(table)
            .ok_or_else(|| anyhow!("no table {table} in this run"))?;
        self.script(&format!(
            "COPY (SELECT * FROM read_parquet({})) TO {} (FORMAT JSON);",
            sql_path(path),
            sql_path(out)
        ))
        .with_context(|| format!("reading the rows of {table}"))?;
        private(out)
    }

    /// The column names of a table.
    fn columns(&self, table: &str) -> Result<Vec<String>> {
        let path = self
            .written
            .get(table)
            .ok_or_else(|| anyhow!("no table {table} in this run"))?;
        Ok(open_parquet(path)?
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect())
    }

    /// Runs one data test: how many rows fail, and the first few.
    pub fn test(&self, check: &PlannedCheck, index: usize) -> Result<Failures> {
        let failing = match check {
            PlannedCheck::Unique { table, column } | PlannedCheck::NotNull { table, column } => {
                if !self.columns(table)?.iter().any(|c| c == column) {
                    return Err(usage(anyhow!(
                        "table {table} has no column {column}; its columns are: {}",
                        self.columns(table)?.join(", ")
                    )));
                }
                let (t, c) = (ident(table), ident(column));
                if matches!(check, PlannedCheck::Unique { .. }) {
                    format!(
                        "SELECT {c}, count(*) AS \"rows\" FROM {t} WHERE {c} IS NOT NULL \
                         GROUP BY {c} HAVING count(*) > 1 ORDER BY 2 DESC, 1"
                    )
                } else {
                    // The row number counts from 1, as `row N` in messages.
                    let file = sql_path(&self.written[table]);
                    format!(
                        "SELECT file_row_number + 1 AS \"row\" FROM \
                         read_parquet({file}, file_row_number = true) WHERE {c} IS NULL ORDER BY 1"
                    )
                }
            }
            PlannedCheck::Sql(sql) => strip_terminator(sql).to_string(),
        };
        let count_file = self.dir.join(format!(".test-{index}.count.json"));
        let rows_file = self.dir.join(format!(".test-{index}.rows.json"));
        let mut script = String::new();
        for (table, path) in &self.written {
            script.push_str(&format!(
                "CREATE VIEW {} AS SELECT * FROM read_parquet({});\n",
                ident(table),
                sql_path(path)
            ));
        }
        script.push_str(&format!(
            "COPY (SELECT count(*) AS n FROM (\n{failing}\n) AS q) TO {} (FORMAT JSON);\n\
             COPY (SELECT * FROM (\n{failing}\n) AS q LIMIT {TEST_EXAMPLES}) TO {} (FORMAT JSON);",
            sql_path(&count_file),
            sql_path(&rows_file)
        ));
        self.script(&script)?;
        let count_text = std::fs::read_to_string(&count_file)
            .with_context(|| format!("reading {}", count_file.display()))?;
        let count = serde_json::from_str::<Value>(count_text.trim())
            .ok()
            .and_then(|v| v["n"].as_u64())
            .ok_or_else(|| anyhow!("{}: no count", count_file.display()))?;
        private(&rows_file)?;
        let examples = std::fs::read_to_string(&rows_file)
            .with_context(|| format!("reading {}", rows_file.display()))?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let _ = std::fs::remove_file(&count_file);
        Ok(Failures { count, examples })
    }

    fn script(&self, sql: &str) -> Result<()> {
        let script = format!("SET TimeZone = 'UTC';\n{sql}");
        tools::output(tools::duckdb_command(&self.duckdb).args(["-c", &script])).map(|_| ())
    }
}

/// A file duckdb wrote, made readable by its owner only, as the files
/// fhir-pipes writes itself are. The work folder is the owner's only too.
fn private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| PipesError::io(path, e))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
