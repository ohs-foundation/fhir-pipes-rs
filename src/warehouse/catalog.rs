//! catalog.sql: a DuckDB view per warehouse table, so a dashboard or an analyst
//! queries `coverage` instead of the files under parquet/coverage/, and the
//! files can move without breaking their queries. It is written from the files
//! the refresh wrote, as text; DuckDB is not needed to write it.
//!
//! The views read the tables from `base_url`:
//!   unset            paths relative to the warehouse directory, globbed
//!   http(s)://…      each table's files listed, since HTTP cannot list a directory
//!   anything else    globbed under it: s3://, r2://, gcs://, az://, a local path
//!
//! catalog.duckdb holds the same views in a DuckDB database a client can
//! ATTACH, by URL once pushed. DuckDB reads the files' schemas when it creates
//! a view, so the file is built where the views can reach the data: in the
//! catalog step when the paths are relative, otherwise during --push, after
//! the tables are uploaded. Only that needs the duckdb command.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::config::{Catalog, Settings};
use super::partition::table_files;

pub const SQL_FILE: &str = "catalog.sql";
pub const DUCKDB_FILE: &str = "catalog.duckdb";

/// Write data/catalog.sql and, when the views read local files, build
/// catalog.duckdb from it.
pub fn write(s: &Settings, c: &Catalog, refreshed_at: &str) -> Result<()> {
    println!("== catalog");
    let tables = tables(s)?;
    let extra = match &c.extra {
        Some(p) => Some((
            p.clone(),
            fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        )),
        None => None,
    };
    let sql = render(
        &tables,
        c.base_url.as_deref(),
        extra.as_ref().map(|(p, t)| (p.as_path(), t.as_str())),
        refreshed_at,
    );
    let path = s.data.join(SQL_FILE);
    fs::write(&path, &sql).with_context(|| format!("writing {}", path.display()))?;
    let listed = tables.iter().filter(|t| !t.files.is_empty()).count();
    println!("   {SQL_FILE}: {listed} table view(s)");
    if c.duckdb_file && c.base_url.is_none() {
        build_duckdb(s, c)?;
    }
    Ok(())
}

/// Build data/catalog.duckdb from data/catalog.sql with the duckdb command,
/// running `setup` first (for credentials; a temporary secret is not saved in
/// the file). It is built beside the target and renamed into place.
pub fn build_duckdb(s: &Settings, c: &Catalog) -> Result<()> {
    let sql_path = s.data.join(SQL_FILE);
    let sql =
        fs::read_to_string(&sql_path).with_context(|| format!("reading {}", sql_path.display()))?;
    let setup = match &c.duckdb_setup {
        Some(p) => fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => String::new(),
    };
    let target = s.data.join(DUCKDB_FILE);
    let tmp = s.data.join(format!("{DUCKDB_FILE}.tmp"));
    for p in [&tmp, &wal(&tmp)] {
        if p.exists() {
            fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    // Relative paths resolve against the warehouse directory.
    let mut child = Command::new(&s.duckdb)
        .arg(&tmp)
        .current_dir(&s.data)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", s.duckdb))?;
    child
        .stdin
        .take()
        .context("duckdb stdin")?
        .write_all(format!("{setup}\n{sql}").as_bytes())
        .context("writing to duckdb")?;
    let out = child.wait_with_output().context("running duckdb")?;
    // The duckdb shell carries on past a failed statement and still exits 0,
    // so its error output is the failure signal.
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || !err.trim().is_empty() {
        let _ = fs::remove_file(&tmp);
        bail!("building {DUCKDB_FILE} with duckdb: {}", err.trim());
    }
    if wal(&tmp).exists() {
        bail!("duckdb left {} behind", wal(&tmp).display());
    }
    fs::rename(&tmp, &target).with_context(|| format!("writing {}", target.display()))?;
    println!("   {DUCKDB_FILE}");
    Ok(())
}

fn wal(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.wal", p.display()))
}

/// A warehouse table and its files, relative to the warehouse directory.
pub struct Table {
    pub name: String,
    /// The partition column's directory level, e.g. `country`.
    pub column: String,
    pub files: Vec<String>,
}

/// The tables under data/parquet, as the manifest counts them.
fn tables(s: &Settings) -> Result<Vec<Table>> {
    let parquet = s.parquet_dir();
    let mut names: Vec<String> = fs::read_dir(&parquet)
        .with_context(|| format!("reading {}", parquet.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| {
            let files = table_files(&parquet.join(&name), &s.partition_column)
                .iter()
                .filter_map(|f| f.strip_prefix(&s.data).ok())
                .map(|f| {
                    f.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect();
            Table {
                name,
                column: s.partition_column.clone(),
                files,
            }
        })
        .collect())
}

/// The text of catalog.sql.
pub fn render(
    tables: &[Table],
    base_url: Option<&str>,
    extra: Option<(&Path, &str)>,
    refreshed_at: &str,
) -> String {
    let base = base_url.map(|b| format!("{}/", b.trim_end_matches('/')));
    let http = base
        .as_deref()
        .is_some_and(|b| b.starts_with("https://") || b.starts_with("http://"));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "-- DuckDB views over the warehouse tables, written by fhir-pipes refresh at {refreshed_at}."
    );
    let _ = writeln!(
        out,
        "-- Rewritten on every refresh: put your own views in the file named by [catalog] extra."
    );
    match &base {
        None => {
            let _ = writeln!(out, "-- Paths are relative to the warehouse directory:");
            let _ = writeln!(out, "--   cd <warehouse> && duckdb -init {SQL_FILE}");
        }
        Some(b) => {
            let _ = writeln!(out, "-- The tables are read from {b}");
            let _ = writeln!(
                out,
                "--   ATTACH '{b}{DUCKDB_FILE}' AS warehouse (READ_ONLY);"
            );
            let _ = writeln!(out, "--   or run this file's statements in any DuckDB.");
        }
    }
    for t in tables {
        out.push('\n');
        if t.files.is_empty() {
            let _ = writeln!(out, "-- {}: no files", t.name);
            continue;
        }
        let source = if http {
            let b = base.as_deref().unwrap_or_default();
            let list: Vec<String> = t
                .files
                .iter()
                .map(|f| format!("    {}", sql_str(&format!("{b}{}", url_path(f)))))
                .collect();
            format!("[\n{}\n  ]", list.join(",\n"))
        } else {
            let b = base.as_deref().unwrap_or_default();
            sql_str(&format!(
                "{b}parquet/{}/{}=*/**/*.parquet",
                t.name, t.column
            ))
        };
        let _ = writeln!(
            out,
            "CREATE OR REPLACE VIEW {} AS\n  SELECT * FROM read_parquet({source}, hive_partitioning = true, union_by_name = true);",
            ident(&t.name)
        );
    }
    if let Some((path, text)) = extra {
        let _ = writeln!(out, "\n-- From {}:\n", path.display());
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// A SQL string literal.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A table name as a SQL identifier, quoted unless it is a plain lower-case name.
fn ident(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A relative file path as a URL path: bytes outside the unreserved set, `/`
/// and `=` are percent-encoded (a partition value with a space, say).
fn url_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/=".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> Vec<Table> {
        vec![
            Table {
                name: "coverage".into(),
                column: "country".into(),
                files: vec![
                    "parquet/coverage/country=NGA/part-0.parquet".into(),
                    "parquet/coverage/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet".into(),
                ],
            },
            Table {
                name: "empty".into(),
                column: "country".into(),
                files: vec![],
            },
        ]
    }

    #[test]
    fn relative_paths_are_globbed_from_the_partition_level() {
        let sql = render(&tables(), None, None, "T");
        assert!(sql.contains(
            "CREATE OR REPLACE VIEW coverage AS\n  SELECT * FROM read_parquet('parquet/coverage/country=*/**/*.parquet', hive_partitioning = true, union_by_name = true);"
        ), "{sql}");
        assert!(sql.contains("-- empty: no files"), "{sql}");
        assert!(!sql.contains("VIEW empty"), "{sql}");
        assert!(sql.contains("duckdb -init catalog.sql"), "{sql}");
    }

    #[test]
    fn https_lists_every_file() {
        let sql = render(&tables(), Some("https://data.example.org/icr"), None, "T");
        assert!(sql.contains(
            "read_parquet([\n    'https://data.example.org/icr/parquet/coverage/country=NGA/part-0.parquet',\n    'https://data.example.org/icr/parquet/coverage/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet'\n  ], hive_partitioning = true"
        ), "{sql}");
        assert!(
            sql.contains("ATTACH 'https://data.example.org/icr/catalog.duckdb'"),
            "{sql}"
        );
    }

    #[test]
    fn object_storage_is_globbed() {
        let sql = render(&tables(), Some("s3://icr/"), None, "T");
        assert!(
            sql.contains("read_parquet('s3://icr/parquet/coverage/country=*/**/*.parquet'"),
            "{sql}"
        );
    }

    #[test]
    fn extra_views_are_appended() {
        let sql = render(
            &tables(),
            None,
            Some((Path::new("extra.sql"), "CREATE VIEW x AS SELECT 1;\n\n")),
            "T",
        );
        assert!(
            sql.ends_with("-- From extra.sql:\n\nCREATE VIEW x AS SELECT 1;\n"),
            "{sql}"
        );
    }

    #[test]
    fn names_and_paths_are_escaped() {
        assert_eq!(ident("indicator_value"), "indicator_value");
        assert_eq!(ident("Odd-Name"), "\"Odd-Name\"");
        assert_eq!(
            url_path("parquet/t/country=Côte d'Ivoire/part-0.parquet"),
            "parquet/t/country=C%C3%B4te%20d%27Ivoire/part-0.parquet"
        );
        assert_eq!(sql_str("o'b"), "'o''b'");
    }
}
