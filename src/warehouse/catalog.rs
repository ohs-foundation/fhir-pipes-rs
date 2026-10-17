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
//! a view, so the file is built where the views can reach the data: right after
//! the run's tables are swapped in when the paths are relative, otherwise during
//! --push, after the tables are uploaded. Only that needs the duckdb command.
//!
//! catalog.spark.sql, with `spark = true`, has a Spark SQL table per warehouse
//! table, for a Spark Thrift server (as the fhir-data-pipes Java pipeline
//! uses). Each table is dropped and created again over its directory under
//! `spark_location`, so a changed schema is picked up; the tables are
//! external, so dropping one leaves the files. The columns are written out,
//! read from the files' footers here, rather than left for Spark to infer:
//! Spark 4.1 cannot infer a schema with a Parquet GEOMETRY column (the
//! Locations table) but reads one declared BINARY, and a column Spark has no
//! type for (TIME) is left out instead of failing the table. Spark reads the
//! files recursively (`recursiveFileLookup`) as an unpartitioned table: the
//! partition columns are in the files as well as the directory names, and
//! Spark's partition discovery does not handle that or the Locations table's
//! deeper directories. A filter on `country` still skips other countries'
//! files, by their Parquet statistics.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use arrow::datatypes::{DataType, TimeUnit};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use super::config::{Catalog, Settings};
use super::partition::{partition_key, table_files};
use super::sql::{ident, sql_str};
use super::stage::Stage;

pub const SQL_FILE: &str = "catalog.sql";
pub const DUCKDB_FILE: &str = "catalog.duckdb";
pub const SPARK_FILE: &str = "catalog.spark.sql";

/// Write catalog.sql into the staging directory, naming the tables as they
/// will be once the run is published.
pub fn write(s: &Settings, c: &Catalog, refreshed_at: &str, stage: &Stage) -> Result<()> {
    println!("== catalog");
    let tables = tables(s, stage)?;
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
    let path = stage.file(SQL_FILE);
    fs::write(&path, &sql).with_context(|| format!("writing {}", path.display()))?;
    let listed = tables.iter().filter(|t| !t.files.is_empty()).count();
    println!("   {SQL_FILE}: {listed} table view(s)");
    if let Some(spark) = &c.spark {
        let extra = match &spark.extra {
            Some(p) => Some((
                p.clone(),
                fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            )),
            None => None,
        };
        let location = match &spark.location {
            Some(l) => l.clone(),
            None => file_url(&s.data)?,
        };
        // Column types from the files as they are now: the run's staged copy
        // of a table, else the live one.
        let read_from: std::collections::HashMap<String, PathBuf> =
            stage.tables()?.into_iter().collect();
        let spark_tables = tables
            .iter()
            .map(|t| {
                let dir = read_from.get(&t.name).context("a table the stage lists")?;
                Ok((t, spark_columns(dir, t)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let sql = render_spark(
            &spark_tables,
            &location,
            &spark.database,
            extra.as_ref().map(|(p, t)| (p.as_path(), t.as_str())),
            refreshed_at,
        );
        let path = stage.file(SPARK_FILE);
        fs::write(&path, &sql).with_context(|| format!("writing {}", path.display()))?;
        println!(
            "   {SPARK_FILE}: {listed} table(s) in database {}",
            spark.database
        );
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
    // Removed on any failure below; renamed into place at the end.
    let file = crate::fs::AtomicFile::adopt(&target, tmp.clone());
    // DuckDB runs in the warehouse directory, so relative paths in the views
    // resolve against it; the database is named relative to it too, since
    // s.data may itself be relative.
    let mut child = Command::new(&s.duckdb)
        .arg(format!("{DUCKDB_FILE}.tmp"))
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
        bail!("building {DUCKDB_FILE} with duckdb: {}", err.trim());
    }
    if wal(&tmp).exists() {
        bail!("duckdb left {} behind", wal(&tmp).display());
    }
    file.commit()
        .with_context(|| format!("writing {}", target.display()))?;
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

/// The tables under data/parquet once the run is published, as the manifest
/// counts them, with each file named where it will be then.
fn tables(s: &Settings, stage: &Stage) -> Result<Vec<Table>> {
    Ok(stage
        .tables()?
        .into_iter()
        .map(|(name, dir)| {
            let column = partition_key(&dir, &s.partition_column);
            let files = table_files(&dir, &column)
                .iter()
                .filter_map(|f| f.strip_prefix(&dir).ok())
                .map(|f| {
                    let mut path = vec!["parquet".to_string(), name.clone()];
                    path.extend(
                        f.components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                    );
                    path.join("/")
                })
                .collect();
            Table {
                name,
                column,
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

/// A column of a Spark table: its name and Spark SQL type, or, for a column
/// Spark cannot read, its Arrow type.
#[derive(Debug, Clone, PartialEq)]
pub struct SparkColumn {
    pub name: String,
    pub spark_type: std::result::Result<String, String>,
}

/// The columns of a table's files, by name in the order first seen, each
/// typed as in the first file that has it.
fn spark_columns(dir: &Path, t: &Table) -> Result<Vec<SparkColumn>> {
    let prefix = format!("parquet/{}/", t.name);
    let mut columns: Vec<SparkColumn> = Vec::new();
    for f in &t.files {
        // `files` are named where they will be; read them from `dir`.
        let path = dir.join(f.strip_prefix(&prefix).unwrap_or(f));
        let file = fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("reading {}", path.display()))?;
        for field in builder.schema().fields() {
            if columns.iter().any(|c| c.name == *field.name()) {
                continue;
            }
            columns.push(SparkColumn {
                name: field.name().clone(),
                spark_type: spark_type(field.data_type())
                    .ok_or_else(|| format!("{:?}", field.data_type())),
            });
        }
    }
    Ok(columns)
}

/// The Spark SQL type Spark reads an Arrow type's Parquet column as, if it
/// can. Not: TIME, nanosecond timestamps, durations, intervals, unions.
fn spark_type(t: &DataType) -> Option<String> {
    Some(match t {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "STRING".into(),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => "BINARY".into(),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 | DataType::UInt8 => "SMALLINT".into(),
        DataType::Int32 | DataType::UInt16 => "INT".into(),
        DataType::Int64 | DataType::UInt32 => "BIGINT".into(),
        DataType::UInt64 => "DECIMAL(20,0)".into(),
        DataType::Float16 | DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Decimal32(p, s)
        | DataType::Decimal64(p, s)
        | DataType::Decimal128(p, s)
        | DataType::Decimal256(p, s)
            if *p <= 38 =>
        {
            format!("DECIMAL({p},{s})")
        }
        DataType::Date32 | DataType::Date64 => "DATE".into(),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => return None,
        DataType::Timestamp(_, Some(_)) => "TIMESTAMP".into(),
        DataType::Timestamp(_, None) => "TIMESTAMP_NTZ".into(),
        DataType::List(f)
        | DataType::LargeList(f)
        | DataType::ListView(f)
        | DataType::LargeListView(f)
        | DataType::FixedSizeList(f, _) => format!("ARRAY<{}>", spark_type(f.data_type())?),
        DataType::Struct(fields) => {
            let fields = fields
                .iter()
                .map(|f| {
                    Some(format!(
                        "{}: {}",
                        spark_ident(f.name()),
                        spark_type(f.data_type())?
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            format!("STRUCT<{}>", fields.join(", "))
        }
        DataType::Map(entries, _) => match entries.data_type() {
            DataType::Struct(kv) if kv.len() == 2 => format!(
                "MAP<{}, {}>",
                spark_type(kv[0].data_type())?,
                spark_type(kv[1].data_type())?
            ),
            _ => return None,
        },
        DataType::Dictionary(_, values) => spark_type(values)?,
        _ => return None,
    })
}

/// The text of catalog.spark.sql: the database, then for each table a DROP
/// TABLE and a CREATE TABLE over `<location>/parquet/<table>` with the
/// table's columns. A table without files is only dropped.
pub fn render_spark(
    tables: &[(&Table, Vec<SparkColumn>)],
    location: &str,
    database: &str,
    extra: Option<(&Path, &str)>,
    refreshed_at: &str,
) -> String {
    let base = location.trim_end_matches('/');
    let db = spark_ident(database);
    let mut out = String::new();
    // No quotes or semicolons in the comments: some statement splitters
    // (beeline's, older spark-sql) do not skip comments.
    let _ = writeln!(
        out,
        "-- Spark SQL tables over the warehouse tables, written by fhir-pipes refresh at {refreshed_at}."
    );
    let _ = writeln!(
        out,
        "-- Rewritten on every refresh: put your own views in the file named by [catalog] spark_extra."
    );
    let _ = writeln!(out, "-- The tables read the files under {base}/parquet.");
    let _ = writeln!(
        out,
        "-- Run it after each refresh, with spark-sql or beeline:"
    );
    let _ = writeln!(out, "--   spark-sql -f {SPARK_FILE}");
    let _ = writeln!(
        out,
        "--   beeline -u jdbc:hive2://localhost:10000 -f {SPARK_FILE}"
    );
    let _ = writeln!(
        out,
        "-- The tables are external: DROP TABLE removes a definition, not the files."
    );
    out.push('\n');
    let _ = writeln!(out, "CREATE DATABASE IF NOT EXISTS {db};");
    let _ = writeln!(out, "USE {db};");
    for (t, columns) in tables {
        let name = format!("{db}.{}", spark_ident(&t.name));
        out.push('\n');
        let readable: Vec<(&str, &str)> = columns
            .iter()
            .filter_map(|c| Some((c.name.as_str(), c.spark_type.as_deref().ok()?)))
            .collect();
        let skip = if t.files.is_empty() {
            Some("no files")
        } else if readable.is_empty() {
            Some("no column Spark can read")
        } else {
            None
        };
        if let Some(why) = skip {
            let _ = writeln!(out, "-- {}: {why}", t.name);
            let _ = writeln!(out, "DROP TABLE IF EXISTS {name};");
            continue;
        }
        for c in columns {
            if let Err(arrow) = &c.spark_type {
                let _ = writeln!(
                    out,
                    "-- {}.{} ({arrow}) is left out: Spark has no type for it.",
                    t.name, c.name
                );
            }
        }
        // fhir-pipes names its files part-<n>.parquet; the filter then
        // leaves out other Parquet files kept in the table directory, such
        // as a catalog's items.parquet.
        let glob = if t.files.iter().all(|f| {
            f.rsplit('/')
                .next()
                .is_some_and(|n| n.starts_with("part-") && n.ends_with(".parquet"))
        }) {
            "part-*.parquet"
        } else {
            "*.parquet"
        };
        let cols: Vec<String> = readable
            .iter()
            .map(|(n, ty)| format!("  {} {ty}", spark_ident(n)))
            .collect();
        let _ = writeln!(out, "DROP TABLE IF EXISTS {name};");
        let _ = writeln!(
            out,
            "CREATE TABLE {name} (\n{}\n)\nUSING PARQUET\nOPTIONS (recursiveFileLookup 'true', pathGlobFilter '{glob}')\nLOCATION {};",
            cols.join(",\n"),
            spark_str(&format!("{base}/parquet/{}", t.name))
        );
    }
    if let Some((path, text)) = extra {
        let _ = writeln!(out, "\n-- From {}:\n", path.display());
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// A directory as a file: URL, for a Spark on this machine. Spark reads the
/// path as given, so nothing is percent-encoded.
fn file_url(dir: &Path) -> Result<String> {
    let abs = std::path::absolute(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let path = abs.to_string_lossy().replace('\\', "/");
    let path = path.trim_end_matches('/');
    Ok(if path.starts_with('/') {
        format!("file://{path}")
    } else {
        // A Windows path, C:/…
        format!("file:///{path}")
    })
}

/// A Spark SQL string literal. Spark reads backslash escapes in strings and
/// does not take '' for a quote.
fn spark_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A Spark SQL identifier in backticks, so a keyword (`group`) works.
fn spark_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
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
            "CREATE OR REPLACE VIEW \"coverage\" AS\n  SELECT * FROM read_parquet('parquet/coverage/country=*/**/*.parquet', hive_partitioning = true, union_by_name = true);"
        ), "{sql}");
        assert!(sql.contains("-- empty: no files"), "{sql}");
        assert!(!sql.contains("VIEW \"empty\""), "{sql}");
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
        assert_eq!(ident("indicator_value"), "\"indicator_value\"");
        assert_eq!(ident("group"), "\"group\"");
        assert_eq!(ident("o\"d"), "\"o\"\"d\"");
        assert_eq!(
            url_path("parquet/t/country=Côte d'Ivoire/part-0.parquet"),
            "parquet/t/country=C%C3%B4te%20d%27Ivoire/part-0.parquet"
        );
        assert_eq!(sql_str("o'b"), "'o''b'");
    }

    fn col(name: &str, ty: &str) -> SparkColumn {
        SparkColumn {
            name: name.into(),
            spark_type: Ok(ty.into()),
        }
    }

    fn spark_tables(t: &[Table]) -> Vec<(&Table, Vec<SparkColumn>)> {
        t.iter()
            .map(|t| {
                let columns = if t.files.is_empty() {
                    vec![]
                } else {
                    vec![col("id", "STRING"), col("country", "STRING")]
                };
                (t, columns)
            })
            .collect()
    }

    #[test]
    fn spark_tables_are_recreated_over_their_directories() {
        let t = tables();
        let sql = render_spark(
            &spark_tables(&t),
            "file:///srv/warehouse/",
            "fhir",
            None,
            "T",
        );
        assert!(
            sql.contains("CREATE DATABASE IF NOT EXISTS `fhir`;\nUSE `fhir`;\n"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "DROP TABLE IF EXISTS `fhir`.`coverage`;\nCREATE TABLE `fhir`.`coverage` (\n  `id` STRING,\n  `country` STRING\n)\nUSING PARQUET\nOPTIONS (recursiveFileLookup 'true', pathGlobFilter 'part-*.parquet')\nLOCATION 'file:///srv/warehouse/parquet/coverage';\n"
            ),
            "{sql}"
        );
        // A table without files is only dropped, so a stale one goes.
        assert!(
            sql.contains("-- empty: no files\nDROP TABLE IF EXISTS `fhir`.`empty`;\n"),
            "{sql}"
        );
        assert!(!sql.contains("CREATE TABLE `fhir`.`empty`"), "{sql}");
        assert!(sql.contains("spark-sql -f catalog.spark.sql"), "{sql}");
        // Some statement splitters do not skip comments.
        for line in sql.lines().filter(|l| l.starts_with("--")) {
            assert!(!line.contains(';') && !line.contains('\''), "{line}");
        }
    }

    #[test]
    fn spark_locations_name_the_table_directories() {
        let t = tables();
        for (location, expect) in [
            (
                "s3a://bucket/warehouse",
                "'s3a://bucket/warehouse/parquet/coverage'",
            ),
            (
                "hdfs://nn:8020/dwh/",
                "'hdfs://nn:8020/dwh/parquet/coverage'",
            ),
            ("/dwh", "'/dwh/parquet/coverage'"),
            (
                "file:///C:/data/o'brien",
                "'file:///C:/data/o\\'brien/parquet/coverage'",
            ),
        ] {
            let sql = render_spark(&spark_tables(&t), location, "fhir", None, "T");
            assert!(sql.contains(&format!("LOCATION {expect};")), "{sql}");
        }
    }

    #[test]
    fn spark_names_are_quoted() {
        let t = Table {
            name: "group".into(),
            column: "country".into(),
            files: vec!["parquet/group/country=NGA/part-0.parquet".into()],
        };
        let columns = vec![col("order", "STRING"), col("a`b", "BIGINT")];
        let sql = render_spark(&[(&t, columns)], "/dwh", "my db", None, "T");
        assert!(
            sql.contains("CREATE DATABASE IF NOT EXISTS `my db`;"),
            "{sql}"
        );
        assert!(
            sql.contains("CREATE TABLE `my db`.`group` (\n  `order` STRING,\n  `a``b` BIGINT\n)"),
            "{sql}"
        );
        assert_eq!(spark_ident("x`y"), "`x``y`");
        assert_eq!(spark_str("a'b\\c"), "'a\\'b\\\\c'");
    }

    #[test]
    fn spark_leaves_out_columns_it_cannot_read() {
        let t = tables();
        let columns = vec![
            col("id", "STRING"),
            SparkColumn {
                name: "at".into(),
                spark_type: Err("Time64(Microsecond)".into()),
            },
        ];
        let sql = render_spark(&[(&t[0], columns)], "/dwh", "fhir", None, "T");
        assert!(
            sql.contains("-- coverage.at (Time64(Microsecond)) is left out: Spark has no type for it.\nDROP TABLE"),
            "{sql}"
        );
        assert!(sql.contains("(\n  `id` STRING\n)"), "{sql}");
        let only_time = vec![SparkColumn {
            name: "at".into(),
            spark_type: Err("Time64(Microsecond)".into()),
        }];
        let sql = render_spark(&[(&t[0], only_time)], "/dwh", "fhir", None, "T");
        assert!(
            sql.contains(
                "-- coverage: no column Spark can read\nDROP TABLE IF EXISTS `fhir`.`coverage`;"
            ),
            "{sql}"
        );
        assert!(!sql.contains("CREATE TABLE"), "{sql}");
    }

    #[test]
    fn spark_reads_every_parquet_file_when_they_are_not_named_part() {
        let t = vec![Table {
            name: "other".into(),
            column: "country".into(),
            files: vec!["parquet/other/country=NGA/data.parquet".into()],
        }];
        let sql = render_spark(&spark_tables(&t), "/dwh", "fhir", None, "T");
        assert!(sql.contains("pathGlobFilter '*.parquet'"), "{sql}");
    }

    #[test]
    fn spark_extra_is_appended() {
        let t = tables();
        let sql = render_spark(
            &spark_tables(&t),
            "/dwh",
            "fhir",
            Some((
                Path::new("spark-extra.sql"),
                "CREATE OR REPLACE VIEW x AS SELECT 1;\n\n",
            )),
            "T",
        );
        assert!(
            sql.ends_with("-- From spark-extra.sql:\n\nCREATE OR REPLACE VIEW x AS SELECT 1;\n"),
            "{sql}"
        );
    }

    #[test]
    fn spark_types_follow_how_spark_reads_parquet() {
        use arrow::datatypes::{Field, Fields};
        use std::sync::Arc;
        let t = |d: DataType| spark_type(&d);
        assert_eq!(t(DataType::Utf8).as_deref(), Some("STRING"));
        assert_eq!(t(DataType::Int64).as_deref(), Some("BIGINT"));
        assert_eq!(t(DataType::UInt64).as_deref(), Some("DECIMAL(20,0)"));
        assert_eq!(t(DataType::Float64).as_deref(), Some("DOUBLE"));
        assert_eq!(t(DataType::Boolean).as_deref(), Some("BOOLEAN"));
        assert_eq!(t(DataType::Date32).as_deref(), Some("DATE"));
        assert_eq!(
            t(DataType::Decimal128(10, 2)).as_deref(),
            Some("DECIMAL(10,2)")
        );
        assert_eq!(
            t(DataType::Timestamp(
                TimeUnit::Microsecond,
                Some("UTC".into())
            ))
            .as_deref(),
            Some("TIMESTAMP")
        );
        assert_eq!(
            t(DataType::Timestamp(TimeUnit::Microsecond, None)).as_deref(),
            Some("TIMESTAMP_NTZ")
        );
        // GeoParquet geometry (WKB) is binary.
        assert_eq!(t(DataType::Binary).as_deref(), Some("BINARY"));
        assert_eq!(
            t(DataType::List(Arc::new(Field::new(
                "item",
                DataType::Utf8,
                true
            ))))
            .as_deref(),
            Some("ARRAY<STRING>")
        );
        let bbox = Fields::from(vec![
            Field::new("xmin", DataType::Float64, false),
            Field::new("ymin", DataType::Float64, false),
        ]);
        assert_eq!(
            t(DataType::Struct(bbox)).as_deref(),
            Some("STRUCT<`xmin`: DOUBLE, `ymin`: DOUBLE>")
        );
        assert_eq!(t(DataType::Time64(TimeUnit::Microsecond)), None);
        assert_eq!(t(DataType::Timestamp(TimeUnit::Nanosecond, None)), None);
        assert_eq!(
            t(DataType::List(Arc::new(Field::new(
                "item",
                DataType::Time32(TimeUnit::Second),
                true
            )))),
            None
        );
    }

    #[test]
    fn spark_columns_come_from_the_files() {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh");
        let t = Table {
            name: "locations".into(),
            column: "country".into(),
            files: vec!["parquet/locations/country=SLE/part-0.parquet".into()],
        };
        let columns = spark_columns(&data.join("parquet/locations"), &t).unwrap();
        assert_eq!(
            columns,
            vec![
                col("id", "STRING"),
                col("name", "STRING"),
                col("country", "STRING")
            ]
        );
    }

    #[test]
    fn the_default_spark_location_is_the_warehouse_as_a_file_url() {
        let url = file_url(Path::new("data")).unwrap();
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.ends_with("/data"), "{url}");
        assert!(!url.contains('\\'), "{url}");
        #[cfg(unix)]
        assert_eq!(file_url(Path::new("/srv/w h/")).unwrap(), "file:///srv/w h");
    }
}
