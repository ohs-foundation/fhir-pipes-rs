//! The database step: each view table, as the views step wrote it to
//! parquet/, loaded into PostgreSQL, so dashboards that read a database (as the
//! fhir-data-pipes single-machine setup does) get the same tables.
//!
//! Every table is replaced in one transaction: created if missing (recreated
//! if its columns changed), emptied with DELETE, and filled with COPY. Readers
//! see the previous run's rows until the commit, then all of the new run's, and
//! views or grants built on the tables survive, since they are not dropped
//! unless their columns change. Column types come from the Parquet files.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{
    DataType, Float64Type, Int64Type, TimeUnit, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType,
};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use sqlx_core::connection::Connection;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::PgConnection;

use super::config::{Database, Settings};

/// Rows sent to COPY per message.
const COPY_ROWS: usize = 4096;

/// Load `tables` (each a name and the directory to read it from) into the
/// database `d` points at.
pub fn load(s: &Settings, d: &Database, tables: &[(String, PathBuf)]) -> Result<()> {
    println!("== database ({})", d.schema);
    let url = std::env::var(&d.url_env)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("[database] needs the connection string in ${}", d.url_env))?;
    let mut plans = Vec::new();
    for (table, dir) in tables {
        let column = super::partition::partition_key(dir, &s.partition_column);
        plans.push(plan(dir, table, &column)?);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the database runtime")?;
    rt.block_on(write(&url, &d.schema, &plans))
}

/// One table to load: its columns and files.
struct Plan {
    table: String,
    columns: Vec<(String, &'static str)>,
    files: Vec<PathBuf>,
}

/// A table's columns, from its Parquet files, which must all agree.
fn plan(dir: &Path, table: &str, partition_column: &str) -> Result<Plan> {
    let files = parquet_files(dir, partition_column).with_context(|| {
        format!("{table}: cannot read its Parquet; the database was not changed")
    })?;
    let mut columns: Option<Vec<(String, &'static str)>> = None;
    for f in &files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(
            fs::File::open(f).with_context(|| format!("opening {}", f.display()))?,
        )
        .with_context(|| format!("reading {}", f.display()))?;
        let these = reader
            .schema()
            .fields()
            .iter()
            .map(|field| {
                pg_type(field.data_type())
                    .map(|t| (field.name().clone(), t))
                    .with_context(|| {
                        format!(
                            "{table}.{}: no PostgreSQL type for {}",
                            field.name(),
                            field.data_type()
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        match &columns {
            None => columns = Some(these),
            Some(c) if *c == these => {}
            Some(_) => bail!(
                "{table}: {} has different columns from the table's other files",
                f.display()
            ),
        }
    }
    Ok(Plan {
        table: table.to_string(),
        columns: columns.unwrap_or_default(),
        files,
    })
}

/// A table's Parquet files: those under its `<column>=` directories, as the
/// views step writes them. Unlike `partition::table_files`, a missing or
/// unreadable directory is an error, not an empty table, so a wrong `--data`
/// or a missing mount cannot empty the database.
fn parquet_files(dir: &Path, column: &str) -> Result<Vec<PathBuf>> {
    fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(dir)
            .and_then(|it| {
                it.map(|e| e.map(|e| e.path()))
                    .collect::<std::io::Result<Vec<_>>>()
            })
            .with_context(|| format!("reading {}", dir.display()))?;
        paths.sort();
        Ok(paths)
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for p in entries(dir)? {
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "parquet") {
                out.push(p);
            }
        }
        Ok(())
    }
    let prefix = format!("{column}=");
    let mut files = Vec::new();
    for part in entries(dir)? {
        if part.is_dir()
            && part
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        {
            walk(&part, &mut files)?;
        }
    }
    Ok(files)
}

/// The PostgreSQL type for a column the views step writes, as
/// information_schema names it.
fn pg_type(t: &DataType) -> Option<&'static str> {
    Some(match t {
        DataType::Utf8 | DataType::LargeUtf8 => "text",
        DataType::Int64 => "bigint",
        DataType::Int32 => "integer",
        DataType::Float64 => "double precision",
        DataType::Boolean => "boolean",
        DataType::Date32 => "date",
        DataType::Timestamp(_, Some(_)) => "timestamp with time zone",
        DataType::Timestamp(_, None) => "timestamp without time zone",
        DataType::Time64(TimeUnit::Microsecond) => "time without time zone",
        _ => return None,
    })
}

async fn write(url: &str, schema: &str, plans: &[Plan]) -> Result<()> {
    // TLS (sslmode=require and up) uses rustls with the same provider as the
    // FHIR client.
    crate::extract::client::ensure_crypto_provider();
    let mut conn = PgConnection::connect(url)
        .await
        .context("connecting to the database")?;
    let mut tx = conn.begin().await.context("starting a transaction")?;
    // One load into a schema at a time: a second waits here until the first
    // commits, so two overlapping refreshes cannot both delete and both copy.
    sqlx_core::query::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("fhir-pipes database {schema}"))
        .execute(&mut *tx)
        .await
        .context("waiting for another load into this schema")?;
    exec(
        &mut tx,
        &format!("CREATE SCHEMA IF NOT EXISTS {}", ident(schema)),
    )
    .await?;
    let name = |p: &Plan| format!("{}.{}", ident(schema), ident(&p.table));
    // First every table, so that indexes, which PostgreSQL names to avoid
    // the relations that exist, cannot take a name a table of this load needs.
    let mut created = Vec::new();
    for p in plans.iter().filter(|p| !p.files.is_empty()) {
        let existing = existing_columns(&mut tx, schema, &p.table).await?;
        let wanted: Vec<(String, String)> = p
            .columns
            .iter()
            .map(|(n, t)| (n.clone(), (*t).to_string()))
            .collect();
        if existing == wanted {
            continue;
        }
        if !existing.is_empty() {
            exec(&mut tx, &format!("DROP TABLE {}", name(p)))
                .await
                .with_context(|| {
                    format!(
                        "{} changed columns and must be recreated; drop what depends on it (views) first",
                        p.table
                    )
                })?;
        }
        let cols: Vec<String> = p
            .columns
            .iter()
            .map(|(n, t)| format!("{} {t}", ident(n)))
            .collect();
        exec(
            &mut tx,
            &format!("CREATE TABLE {} ({})", name(p), cols.join(", ")),
        )
        .await?;
        created.push(p);
    }
    // Then their indexes: unnamed, made with the table, so later runs find
    // them in place.
    for p in created {
        if p.columns.iter().any(|(n, _)| n == "id") {
            exec(&mut tx, &format!("CREATE INDEX ON {} (id)", name(p))).await?;
        }
    }
    for p in plans {
        let name = name(p);
        if p.files.is_empty() {
            // A table written before empty results got an empty file has no
            // columns to create it from: empty it if it exists.
            if table_exists(&mut tx, schema, &p.table).await? {
                exec(&mut tx, &format!("DELETE FROM {name}")).await?;
                println!("   {} → {schema}.{} (0 rows)", p.table, p.table);
            } else {
                println!(
                    "   {}: no files to take its columns from, not created; run the views step",
                    p.table
                );
            }
            continue;
        }
        exec(&mut tx, &format!("DELETE FROM {name}")).await?;
        let rows = copy(&mut tx, &name, p).await?;
        println!("   {} → {schema}.{} ({rows} rows)", p.table, p.table);
    }
    tx.commit().await.context("committing")?;
    Ok(())
}

async fn exec(conn: &mut PgConnection, sql: &str) -> Result<()> {
    sqlx_core::query::query(AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {sql}"))?;
    Ok(())
}

async fn table_exists(conn: &mut PgConnection, schema: &str, table: &str) -> Result<bool> {
    Ok(!existing_columns(conn, schema, table).await?.is_empty())
}

/// A table's columns and types, in order; empty when there is no such table.
async fn existing_columns(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>> {
    let rows = sqlx_core::query::query(
        "SELECT column_name::text, data_type::text FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .with_context(|| format!("reading the columns of {schema}.{table}"))?;
    rows.iter()
        .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
        .collect::<std::result::Result<_, sqlx_core::error::Error>>()
        .context("reading information_schema.columns")
}

/// COPY every row of `p`'s files into `name`. Returns the row count.
async fn copy(conn: &mut PgConnection, name: &str, p: &Plan) -> Result<u64> {
    let cols: Vec<String> = p.columns.iter().map(|(n, _)| ident(n)).collect();
    let mut sink = conn
        .copy_in_raw(&format!(
            "COPY {name} ({}) FROM STDIN (FORMAT csv)",
            cols.join(", ")
        ))
        .await
        .with_context(|| format!("starting COPY into {name}"))?;
    for f in &p.files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(f)?)?
            .with_batch_size(COPY_ROWS)
            .build()?;
        for batch in reader {
            let batch = batch.with_context(|| format!("reading {}", f.display()))?;
            let text = csv(&batch).with_context(|| format!("{}: {}", p.table, f.display()))?;
            if let Err(e) = sink.send(text.into_bytes()).await {
                return Err(anyhow::Error::from(e).context(format!("copying into {name}")));
            }
        }
    }
    sink.finish()
        .await
        .with_context(|| format!("finishing COPY into {name}"))
}

/// A batch as CSV for COPY: NULL is an empty unquoted field, every string is
/// quoted (so an empty string stays one), the rest as PostgreSQL reads them.
fn csv(batch: &RecordBatch) -> Result<String> {
    let options = FormatOptions::default();
    let formatters: Vec<Option<ArrayFormatter>> = batch
        .columns()
        .iter()
        .map(|c| match c.data_type() {
            DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Float64
            | DataType::Int64
            | DataType::Timestamp(_, _) => Ok(None),
            _ => ArrayFormatter::try_new(c.as_ref(), &options).map(Some),
        })
        .collect::<std::result::Result<_, _>>()?;
    let mut out = String::new();
    for row in 0..batch.num_rows() {
        for (i, column) in batch.columns().iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            if column.is_null(row) {
                continue;
            }
            match column.data_type() {
                DataType::Utf8 => quote(&mut out, column.as_string::<i32>().value(row)),
                DataType::LargeUtf8 => quote(&mut out, column.as_string::<i64>().value(row)),
                DataType::Int64 => {
                    out.push_str(&column.as_primitive::<Int64Type>().value(row).to_string())
                }
                DataType::Float64 => {
                    let v = column.as_primitive::<Float64Type>().value(row);
                    out.push_str(&if v.is_nan() {
                        "NaN".to_string()
                    } else if v.is_infinite() {
                        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
                    } else {
                        // Rust prints the shortest text that reads back to v.
                        v.to_string()
                    });
                }
                DataType::Timestamp(unit, tz) => {
                    let c = column.as_ref();
                    let micros = match unit {
                        TimeUnit::Second => {
                            c.as_primitive::<TimestampSecondType>().value(row) * 1_000_000
                        }
                        TimeUnit::Millisecond => {
                            c.as_primitive::<TimestampMillisecondType>().value(row) * 1_000
                        }
                        TimeUnit::Microsecond => {
                            c.as_primitive::<TimestampMicrosecondType>().value(row)
                        }
                        TimeUnit::Nanosecond => {
                            c.as_primitive::<TimestampNanosecondType>().value(row) / 1_000
                        }
                    };
                    let t = chrono::DateTime::from_timestamp_micros(micros)
                        .context("timestamp out of range")?;
                    // An instant (with a time zone) as UTC; a local time as is.
                    out.push_str(
                        &t.format(if tz.is_some() {
                            "%Y-%m-%d %H:%M:%S%.6f+00"
                        } else {
                            "%Y-%m-%d %H:%M:%S%.6f"
                        })
                        .to_string(),
                    );
                }
                _ => {
                    let f = formatters[i]
                        .as_ref()
                        .expect("a formatter per other column");
                    out.push_str(&f.value(row).try_to_string()?);
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn quote(out: &mut String, s: &str) {
    out.push('"');
    out.push_str(&s.replace('"', "\"\""));
    out.push('"');
}

/// A quoted SQL identifier.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        BooleanArray, Date32Array, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::{Field, Schema};

    use super::*;

    #[test]
    fn csv_quotes_strings_and_leaves_nulls_empty() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("s", DataType::Utf8, true),
            Field::new("n", DataType::Int64, true),
            Field::new("x", DataType::Float64, true),
            Field::new("b", DataType::Boolean, true),
            Field::new("d", DataType::Date32, true),
            Field::new(
                "t",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                true,
            ),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![Some("a \"b\", c"), Some(""), None])),
                Arc::new(Int64Array::from(vec![Some(1), None, Some(-3)])),
                Arc::new(Float64Array::from(vec![
                    Some(41.735497),
                    Some(f64::NAN),
                    None,
                ])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(Date32Array::from(vec![Some(18383), None, None])),
                Arc::new(
                    TimestampMicrosecondArray::from(vec![Some(1_588_291_200_500_000), None, None])
                        .with_timezone("UTC"),
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            csv(&batch).unwrap(),
            "\"a \"\"b\"\", c\",1,41.735497,true,2020-05-01,2020-05-01 00:00:00.500000+00\n\"\",,NaN,false,,\n,-3,,,,\n"
        );
    }

    #[test]
    fn types_map_to_postgres() {
        assert_eq!(pg_type(&DataType::Utf8), Some("text"));
        assert_eq!(
            pg_type(&DataType::Timestamp(
                TimeUnit::Microsecond,
                Some("UTC".into())
            )),
            Some("timestamp with time zone")
        );
        assert_eq!(pg_type(&DataType::Binary), None);
        assert_eq!(ident("o\"d"), "\"o\"\"d\"");
    }
}
