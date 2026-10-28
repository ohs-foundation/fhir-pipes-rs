//! The libraries step: run each SQLQuery Library over this run's tables with
//! duckdb and write its result as a warehouse table, staged and published
//! with the rest.
//!
//! The Libraries come from the views directory and the `libraries` sources
//! (library/resolve.rs). Each SQLQuery runs after the Libraries it depends
//! on. Its inputs are read where this run reads tables (`Stage::read`): a
//! view's table, `location_hierarchy` for the built-in LocationHierarchy, and
//! an earlier SQLQuery's result; a SQLView's SQL is inlined. SQLViews write
//! no table here. A result with a `country` column is partitioned by it, as
//! the views' tables are (`<table>/country=<value>/part-0.parquet`, the
//! column in the files as well, as the database step and Spark read it);
//! any other is one file, `<table>/part-0.parquet`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};

use crate::error::usage;
use crate::library::compose::{Bind, compose};
use crate::library::resolve::{Artifact, Catalog, HIERARCHY_TABLE, HIERARCHY_URL};
use crate::library::{Dependency, Kind, Library};

use super::config::Settings;
use super::partition::{NULL_PARTITION, partition_key, replace, table_files};
use super::sql::sql_path;
use super::{manifest, stage, tools};

/// The column a result is partitioned by when it has one.
const PARTITION_COLUMN: &str = "country";

/// Run every SQLQuery Library into the staging directory. Returns the
/// tables written.
pub(super) fn libraries_step(s: &Settings, stage: &stage::Stage) -> Result<BTreeSet<String>> {
    println!("== libraries");
    let catalog = catalog(s)?;
    let queries: Vec<&Library> = catalog
        .order()?
        .into_iter()
        .filter(|l| l.kind == Kind::SqlQuery)
        .collect();
    if queries.is_empty() {
        println!("   no SQLQuery Libraries");
    }
    let taken = other_tables(s)?;
    for lib in &queries {
        let table = lib.table(&s.strip_prefix);
        if let Some(by) = taken.get(&table) {
            return Err(usage(anyhow!(
                "Library {} would write table {table}, which is {by}; rename the Library \
                 or change strip_prefix",
                lib.name
            )));
        }
    }
    let mut written = BTreeSet::new();
    for lib in queries {
        crate::stop::check()?;
        let t = Instant::now();
        let table = lib.table(&s.strip_prefix);
        check_inputs(s, stage, &catalog, lib, lib)?;
        let bind = |a: &Artifact| bind(s, stage, a);
        let sql = compose(lib, "duckdb", &catalog, &bind).map_err(usage)?;
        let dir = stage.table(&table);
        write(s, &sql, &dir).with_context(|| format!("Library {}", lib.name))?;
        let rows = manifest::rows(&dir, PARTITION_COLUMN)?;
        println!(
            "   {} → parquet/{table} ({rows} rows, {:.1}s)",
            lib.name,
            t.elapsed().as_secs_f64()
        );
        written.insert(table);
    }
    Ok(written)
}

/// The Libraries and views of the views directory and the `libraries`
/// sources.
pub(super) fn catalog(s: &Settings) -> Result<Catalog> {
    let mut sources: Vec<String> = s.views.iter().map(|p| p.display().to_string()).collect();
    sources.extend(s.libraries.iter().cloned());
    Catalog::load(&sources, &s.base_dir, &s.strip_prefix)
}

/// The tables other steps write, which no result may replace, each with what
/// writes it: every view in the views directory (with a url or without),
/// the lookup tables, `locations` and `location_hierarchy`.
fn other_tables(s: &Settings) -> Result<BTreeMap<String, String>> {
    let mut taken = BTreeMap::new();
    for t in ["locations", HIERARCHY_TABLE] {
        taken.insert(t.to_string(), "written by the locations step".to_string());
    }
    for l in &s.lookups {
        taken.insert(l.table.clone(), "a [[partition.lookup]] table".to_string());
    }
    if let Some(dir) = &s.views {
        for v in crate::views::run::discover(dir, &s.strip_prefix).map_err(usage)? {
            taken.insert(v.table, format!("the table of view {}", v.name));
        }
    }
    Ok(taken)
}

/// The table an input is read from, or `None` for a SQLView, whose SQL is
/// inlined. The built-in LocationHierarchy is the table the locations step
/// writes.
fn input_table(s: &Settings, a: &Artifact) -> Option<String> {
    match a {
        Artifact::Library(l) if l.url == HIERARCHY_URL => Some(HIERARCHY_TABLE.to_string()),
        Artifact::Library(l) if l.kind == Kind::SqlView => None,
        _ => Some(a.table(&s.strip_prefix)),
    }
}

/// Every table `lib` reads, through the SQLViews it inlines, must be there.
/// `query` is the SQLQuery being run, for the message.
fn check_inputs(
    s: &Settings,
    stage: &stage::Stage,
    catalog: &Catalog,
    lib: &Library,
    query: &Library,
) -> Result<()> {
    for dep in &lib.depends_on {
        let input = catalog
            .get(&dep.canonical, dep.version.as_deref())
            .with_context(|| format!("Library {} depends-on {}", lib.name, dep.label))
            .map_err(usage)?;
        match (input_table(s, input), input) {
            (None, Artifact::Library(view)) => check_inputs(s, stage, catalog, view, query)?,
            (None, Artifact::View(_)) => {}
            (Some(table), _) => {
                let dir = stage.read(&table);
                if !dir.is_dir() || files(s, &dir).is_empty() {
                    return Err(usage(missing(lib, query, dep, &table, input)));
                }
            }
        }
    }
    Ok(())
}

/// The error for an input without a table.
fn missing(
    lib: &Library,
    query: &Library,
    dep: &Dependency,
    table: &str,
    input: &Artifact,
) -> anyhow::Error {
    let by = if lib.url == query.url {
        String::new()
    } else {
        format!(" (read by {})", query.name)
    };
    let canonical = match &dep.version {
        Some(v) => format!("{}|{v}", dep.canonical),
        None => dep.canonical.clone(),
    };
    let why = match input {
        Artifact::Library(l) if l.url == HIERARCHY_URL => {
            "the locations step writes it; run the locations step"
        }
        Artifact::Library(_) => "its Library runs before this one and wrote none",
        Artifact::View(_) => {
            "the views step writes a table only for the views in the views directory; \
             add the view there, or run the views step"
        }
    };
    anyhow!(
        "Library {}{by}: input {} ({canonical}) has no table {table} in the warehouse; {why}",
        lib.name,
        dep.label
    )
}

/// A table's Parquet files, by its own partition layout.
fn files(s: &Settings, dir: &Path) -> Vec<std::path::PathBuf> {
    table_files(dir, &partition_key(dir, &s.partition_column))
}

/// Each input but a SQLView is read from its table's files in this run.
fn bind(s: &Settings, stage: &stage::Stage, a: &Artifact) -> Result<Option<Bind>> {
    let Some(table) = input_table(s, a) else {
        return Ok(None);
    };
    let dir = stage.read(&table);
    let files = files(s, &dir);
    if files.is_empty() {
        return Err(anyhow!("{} has no table {table}", a.url()));
    }
    Ok(Some(Bind::Relation(read_parquet(&files))))
}

/// `read_parquet` over `files`, with a partition directory's value read as
/// text, so `country=123` is the string `123` as it is in the views' tables.
fn read_parquet(files: &[std::path::PathBuf]) -> String {
    let list: Vec<String> = files.iter().map(|f| sql_path(f)).collect();
    format!(
        "read_parquet([{}], hive_partitioning = true, union_by_name = true, \
         hive_types_autocast = false)",
        list.join(", ")
    )
}

/// Write the rows of `sql` to `table_dir`, replacing what was there only once
/// every file is written, as `partition::write` does.
fn write(s: &Settings, sql: &str, table_dir: &Path) -> Result<()> {
    let parent = table_dir
        .parent()
        .context("table directory has no parent")?;
    fs::create_dir_all(parent)?;
    let name = table_dir
        .file_name()
        .context("table directory has no name")?
        .to_string_lossy();
    let staging = parent.join(format!(".{name}.new"));
    let _ = fs::remove_dir_all(&staging);
    let copied = copy(s, sql, &staging);
    if copied.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    copied?;
    replace(&staging, table_dir)
}

/// Write the rows of `sql` as a table at `staging`.
fn copy(s: &Settings, sql: &str, staging: &Path) -> Result<()> {
    if columns(s, sql)?.iter().any(|c| c == PARTITION_COLUMN) {
        duckdb(
            s,
            &format!(
                "COPY ({sql}) TO {} (FORMAT PARQUET, PARTITION_BY ({PARTITION_COLUMN}), \
                 WRITE_PARTITION_COLUMNS true, FILENAME_PATTERN 'part-{{i}}');",
                sql_path(staging)
            ),
        )?;
        // No rows: one empty file in the null partition, so the table still
        // has its columns, as `partition::write` leaves an empty view table.
        let empty = !staging.is_dir() || fs::read_dir(staging)?.next().is_none();
        if empty {
            let dir = staging.join(format!("{PARTITION_COLUMN}={NULL_PARTITION}"));
            fs::create_dir_all(&dir)?;
            duckdb(
                s,
                &format!(
                    "COPY (SELECT * FROM ({sql}) LIMIT 0) TO {} (FORMAT PARQUET);",
                    sql_path(&dir.join("part-0.parquet"))
                ),
            )?;
        }
    } else {
        fs::create_dir_all(staging)?;
        duckdb(
            s,
            &format!(
                "COPY ({sql}) TO {} (FORMAT PARQUET);",
                sql_path(&staging.join("part-0.parquet"))
            ),
        )?;
    }
    Ok(())
}

/// A file with no SQL, given to duckdb's `-init`.
const NO_INIT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// duckdb for a Library's SQL. `-init` with an empty file keeps duckdb from
/// running the user's `~/.duckdbrc` first, which could change settings or
/// the output format the step reads, or run statements of its own.
fn duckdb_command(s: &Settings) -> Command {
    let mut c = Command::new(&s.duckdb);
    c.args(["-init", NO_INIT]);
    c
}

/// Run a SQL script with `duckdb_command`, discarding its result table.
fn duckdb(s: &Settings, sql: &str) -> Result<()> {
    tools::output(duckdb_command(s).args(["-c", sql])).map(|_| ())
}

/// The column names of `sql`'s result.
fn columns(s: &Settings, sql: &str) -> Result<Vec<String>> {
    let out = tools::output(duckdb_command(s).args([
        "-noheader",
        "-list",
        "-c",
        &format!("SELECT column_name FROM (DESCRIBE ({sql}));"),
    ]))?;
    Ok(out.lines().map(str::to_string).collect())
}
