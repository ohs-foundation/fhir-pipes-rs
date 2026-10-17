//! The database step: each view table, as the views step wrote it to
//! parquet/, loaded into PostgreSQL, so dashboards that read a database (as the
//! fhir-data-pipes single-machine setup does) get the same tables.
//!
//! Every table is replaced in one transaction: created if missing (recreated
//! if its columns changed), emptied with DELETE, and filled with COPY. Readers
//! see the previous run's rows until the commit, then all of the new run's, and
//! views or grants built on the tables survive, since they are not dropped
//! unless their columns change. Column types come from the Parquet files.
//!
//! The DELETE leaves the old rows as dead tuples for autovacuum to reclaim.
//! After the commit each loaded table is analyzed, so the planner's
//! statistics describe the new rows at once rather than after autovacuum's
//! next analyze.
//!
//! With `mode = "upsert"`, only the rows of the resources that changed since
//! the last load are rewritten, as the Java pipeline's JdbcResourceWriter
//! does: for each table, `DELETE FROM t WHERE <key> = ANY($1)` for the ids
//! the change log (export/changes.rs) lists for its view's resource type,
//! changed and deleted alike, then a COPY of the rows with those ids, read
//! from the table's Parquet files. `<key>` is the view's column with the path
//! `getResourceKey()` (`key_column`). The tables still go in one
//! transaction. A table is loaded in full instead, as in replace, when the
//! log cannot vouch for what the database holds (`upsert_loads`); only those
//! tables are analyzed after the commit.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, AsArray, BooleanArray, RecordBatch};
use arrow::datatypes::{
    DataType, Float64Type, Int64Type, TimeUnit, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType,
};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use fhir_pipes_sof::{SelectColumn, ViewDefinition};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowPredicateFn, ParquetRecordBatchReaderBuilder, RowFilter};
use sqlx_core::connection::Connection;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::PgConnection;

use crate::error::usage;
use crate::export::changes::Changes;

use super::config::{Database, Mode, Settings};
use super::partition::table_files_checked;
use super::sql::ident;

/// Rows sent to COPY per message.
const COPY_ROWS: usize = 4096;

/// A view table to load.
pub struct Table {
    pub name: String,
    /// Where to read it: the run's staged copy, else parquet/.
    pub dir: PathBuf,
    /// The resource type its view reads.
    pub resource: String,
    /// Its view's `getResourceKey()` column (`key_column`).
    pub key: Option<String>,
}

/// Each table the views step wrote in this run, with what its rows were
/// built from (a fingerprint of its view, its partition lookup and the
/// fhir-pipes version). None when the views step did not run.
pub type Built = BTreeMap<String, String>;

/// The column of `v` upsert finds a resource's rows by: one whose path is
/// `getResourceKey()`, in a select that keeps the resource as its focus (no
/// `forEach`, `forEachOrNull`, `repeat` or `path`, where `getResourceKey()`
/// would key the element, as for a contained resource) and not in a
/// `unionAll`. The one named `id` if there are several, else the first.
pub fn key_column(v: &ViewDefinition) -> Option<String> {
    fn walk(selects: &[SelectColumn], found: &mut Vec<String>) {
        for s in selects {
            if s.for_each.is_some()
                || s.for_each_or_null.is_some()
                || !s.repeat.is_empty()
                || s.path.is_some()
            {
                continue;
            }
            for c in s.column.iter().flatten() {
                if c.path.trim() == "getResourceKey()" && c.collection != Some(true) {
                    found.push(c.name.clone());
                }
            }
            walk(&s.select, found);
        }
    }
    let mut found = Vec::new();
    walk(&v.select, &mut found);
    found.iter().find(|n| *n == "id").or(found.first()).cloned()
}

/// Why upsert refuses the view `view`, which writes `table`.
pub fn no_key(view: &str, table: &str) -> String {
    format!(
        "[database] mode = \"upsert\" finds a resource's rows by a column with the path \
         getResourceKey(), outside any forEach, and view {view} (table {table}) has none; \
         add {{\"name\": \"id\", \"path\": \"getResourceKey()\"}} to its select, leave {table} \
         out of [database] tables, or use mode = \"replace\""
    )
}

/// Load `tables` into the database `d` points at. `built`: what the views
/// step wrote in this run, for upsert.
pub fn load(s: &Settings, d: &Database, tables: &[Table], built: Option<&Built>) -> Result<()> {
    let upsert = d.mode == Mode::Upsert;
    if upsert {
        println!("== database ({}, upsert)", d.schema);
    } else {
        println!("== database ({})", d.schema);
    }
    let url = std::env::var(&d.url_env)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("[database] needs the connection string in ${}", d.url_env))?;
    let mut plans = Vec::new();
    for t in tables {
        let column = super::partition::partition_key(&t.dir, &s.partition_column);
        let mut p = plan(&t.dir, &t.name, &column)?;
        if upsert {
            p.key = t.key.clone();
        }
        plans.push(p);
    }
    let loads = if upsert {
        upsert_loads(s, &identity(&url, &d.schema), tables, &plans, built)?
    } else {
        plans.iter().map(|_| Load::Full(None)).collect()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the database runtime")?;
    rt.block_on(write(&url, &d.schema, &plans, loads, upsert))?;
    if upsert {
        record_load(s, &identity(&url, &d.schema), &plans, built)?;
    }
    Ok(())
}

/// How a table is loaded.
enum Load {
    /// Every row; in upsert, with why, when that is the table's own.
    Full(Option<String>),
    /// The rows of these resources, and the rows without a key.
    Ids {
        key: String,
        ids: Arc<HashSet<String>>,
    },
    /// None of its resources changed.
    Unchanged,
}

/// How upsert loads each table. Every table in full when the change log
/// cannot vouch for the database: there is none, a refresh did not finish,
/// `--full` asked for it, the views step did not run in this refresh (the
/// Parquet may predate what the log lists), or the last load went to another
/// database or schema. A table in full when what its rows are built from
/// changed, or its type is marked `full`. Its ids otherwise; a new table or
/// one whose columns changed is found to need a full load in `write`.
fn upsert_loads(
    s: &Settings,
    identity: &str,
    tables: &[Table],
    plans: &[Plan],
    built: Option<&Built>,
) -> Result<Vec<Load>> {
    let log = Changes::read(&s.raw_dir());
    let all = match (&log, built) {
        (Err(why), _) => Some(why.clone()),
        (Ok(None), _) => Some("no record of the last load".to_string()),
        (Ok(Some(c)), _) if c.reload.is_some() => c.reload.clone(),
        (_, None) => Some("the views step did not run in this refresh".to_string()),
        (Ok(Some(c)), _) if c.database.as_deref() != Some(identity) => {
            Some("the last load went to another database or schema".to_string())
        }
        _ => None,
    };
    if let Some(why) = &all {
        println!("   every table in full: {why}");
    }
    let mut loads = Vec::new();
    for (t, p) in tables.iter().zip(plans) {
        let key = t
            .key
            .clone()
            .ok_or_else(|| usage(anyhow::anyhow!(no_key(&t.name, &t.name))))?;
        match p.columns.iter().find(|(n, _)| *n == key) {
            Some((_, "text")) => {}
            Some((_, other)) => bail!(
                "{}.{key}: the getResourceKey() column is {other} in the Parquet files, not text",
                t.name
            ),
            None if p.files.is_empty() => {}
            None => bail!(
                "{}: its Parquet files have no column {key}; run the views step",
                t.name
            ),
        }
        let (Ok(Some(c)), None, Some(built)) = (&log, &all, built) else {
            loads.push(Load::Full(None));
            continue;
        };
        let last = c.tables.get(&t.name);
        if last.is_none() || last != built.get(&t.name) {
            loads.push(Load::Full(Some(
                if last.is_none() {
                    "not in the last load"
                } else {
                    "its view, its partition lookup or fhir-pipes changed"
                }
                .to_string(),
            )));
            continue;
        }
        loads.push(match c.types.get(&t.resource) {
            None => Load::Unchanged,
            Some(tc) if tc.full => Load::Full(Some(format!(
                "every {} is reloaded (read without an old file to compare, or most changed)",
                t.resource
            ))),
            Some(tc) => {
                let ids: HashSet<String> = tc.ids().into_iter().map(str::to_string).collect();
                if ids.is_empty() {
                    Load::Unchanged
                } else {
                    Load::Ids {
                        key,
                        ids: Arc::new(ids),
                    }
                }
            }
        });
    }
    Ok(loads)
}

/// After an upsert's commit: the change log now starts from this load. With
/// the views step in this run, what it listed is in the database, and each
/// table is recorded with what it was built from. Without it the Parquet may
/// be older than the log, so the ids are kept and no table is recorded, and
/// the next load is in full.
fn record_load(s: &Settings, identity: &str, plans: &[Plan], built: Option<&Built>) -> Result<()> {
    let raw = s.raw_dir();
    let mut c = match Changes::read(&raw) {
        Ok(Some(c)) => c,
        _ => Changes {
            running: true,
            ..Changes::default()
        },
    };
    c.database = Some(identity.to_string());
    match built {
        Some(built) => {
            c.reload = None;
            c.types.clear();
            c.tables = plans
                .iter()
                .filter(|p| !p.files.is_empty())
                .filter_map(|p| built.get(&p.table).map(|f| (p.table.clone(), f.clone())))
                .collect();
        }
        None => c.tables.clear(),
    }
    c.write(&raw)?;
    Ok(())
}

/// The database a URL names and the schema, without credentials, for the
/// change log.
fn identity(url: &str, schema: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => format!(
            "{}:{}{} schema {schema}",
            u.host_str().unwrap_or_default(),
            u.port().unwrap_or(5432),
            u.path()
        ),
        Err(_) => format!("schema {schema}"),
    }
}

/// One table to load: its columns and files, and in upsert its key column.
struct Plan {
    table: String,
    columns: Vec<(String, &'static str)>,
    files: Vec<PathBuf>,
    key: Option<String>,
}

/// A table's columns, from its Parquet files, which must all agree.
fn plan(dir: &Path, table: &str, partition_column: &str) -> Result<Plan> {
    // A missing or unreadable directory is an error, so a wrong `--data` or a
    // missing mount cannot empty the database.
    let files = table_files_checked(dir, partition_column).with_context(|| {
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
        key: None,
    })
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

async fn write(
    url: &str,
    schema: &str,
    plans: &[Plan],
    mut loads: Vec<Load>,
    upsert: bool,
) -> Result<()> {
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
    // The tables this load fills or empties, analyzed after the commit.
    let mut loaded = Vec::new();
    for (i, p) in plans
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.files.is_empty())
    {
        let existing = existing_columns(&mut tx, schema, &p.table).await?;
        let wanted: Vec<(String, String)> = p
            .columns
            .iter()
            .map(|(n, t)| (n.clone(), (*t).to_string()))
            .collect();
        if existing == wanted {
            continue;
        }
        // A table already loaded in full keeps the reason said for all.
        if upsert && !matches!(loads[i], Load::Full(_)) {
            let why = if existing.is_empty() {
                "a new table"
            } else {
                "its columns changed"
            };
            loads[i] = Load::Full(Some(why.to_string()));
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
        created.push(i);
    }
    // Then their indexes: unnamed, made with the table, so later runs find
    // them in place. Upsert also needs one on the key column, which a table
    // made by replace may lack.
    for &i in &created {
        let p = &plans[i];
        if p.columns.iter().any(|(n, _)| n == "id") {
            exec(&mut tx, &format!("CREATE INDEX ON {} (id)", name(p))).await?;
        }
    }
    if upsert {
        for p in plans.iter().filter(|p| !p.files.is_empty()) {
            if let Some(key) = p.key.as_deref()
                && !has_index(&mut tx, &name(p), key).await?
            {
                exec(
                    &mut tx,
                    &format!("CREATE INDEX ON {} ({})", name(p), ident(key)),
                )
                .await?;
            }
        }
    }
    for (p, load) in plans.iter().zip(loads) {
        let name = name(p);
        if p.files.is_empty() {
            // A table written before empty results got an empty file has no
            // columns to create it from: empty it if it exists.
            if table_exists(&mut tx, schema, &p.table).await? {
                exec(&mut tx, &format!("DELETE FROM {name}")).await?;
                println!("   {} → {schema}.{} (0 rows)", p.table, p.table);
                loaded.push(name);
            } else {
                println!(
                    "   {}: no files to take its columns from, not created; run the views step",
                    p.table
                );
            }
            continue;
        }
        match load {
            Load::Full(why) => {
                exec(&mut tx, &format!("DELETE FROM {name}")).await?;
                let rows = copy(&mut tx, &name, p, None).await?;
                match why {
                    None => println!("   {} → {schema}.{} ({rows} rows)", p.table, p.table),
                    Some(why) => println!(
                        "   {} → {schema}.{} ({rows} rows, in full: {why})",
                        p.table, p.table
                    ),
                }
                loaded.push(name);
            }
            Load::Ids { key, ids } => {
                let deleted = delete_ids(&mut tx, &name, &key, &ids).await?;
                let rows = copy(&mut tx, &name, p, Some((&key, &ids))).await?;
                println!(
                    "   {} → {schema}.{} ({} resources changed or deleted: {deleted} rows deleted, {rows} copied)",
                    p.table,
                    p.table,
                    ids.len()
                );
            }
            Load::Unchanged => println!("   {} → {schema}.{} (unchanged)", p.table, p.table),
        }
    }
    tx.commit().await.context("committing")?;
    // Outside the load's transaction, so the lock it held is released and
    // readers see the new rows before this finishes.
    for name in loaded {
        exec(&mut conn, &format!("ANALYZE {name}")).await?;
    }
    Ok(())
}

async fn exec(conn: &mut PgConnection, sql: &str) -> Result<()> {
    sqlx_core::query::query(AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {sql}"))?;
    Ok(())
}

/// Delete the rows of `ids` from `name`, and the rows without a key, which
/// are copied again with the changed ones. Returns the rows deleted.
async fn delete_ids(
    conn: &mut PgConnection,
    name: &str,
    key: &str,
    ids: &HashSet<String>,
) -> Result<u64> {
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    // Two statements rather than one with OR, so each can use the index.
    let by_id = format!("DELETE FROM {name} WHERE {} = ANY($1)", ident(key));
    let mut n = sqlx_core::query::query(AssertSqlSafe(by_id.clone()))
        .bind(ids)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {by_id}"))?
        .rows_affected();
    let no_key = format!("DELETE FROM {name} WHERE {} IS NULL", ident(key));
    n += sqlx_core::query::query(AssertSqlSafe(no_key.clone()))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {no_key}"))?
        .rows_affected();
    Ok(n)
}

/// Whether `name` has an index whose first column is `column`.
async fn has_index(conn: &mut PgConnection, name: &str, column: &str) -> Result<bool> {
    sqlx_core::query::query(
        "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_attribute a \
         ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
         WHERE i.indrelid = $1::text::regclass AND a.attname = $2)",
    )
    .bind(name)
    .bind(column)
    .fetch_one(&mut *conn)
    .await
    .and_then(|r| r.try_get::<bool, _>(0))
    .with_context(|| format!("reading the indexes of {name}"))
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

/// COPY the rows of `p`'s files into `name`: every row, or with `only`
/// (a key column and ids) the rows whose key is one of the ids or null.
/// Returns the row count.
async fn copy(
    conn: &mut PgConnection,
    name: &str,
    p: &Plan,
    only: Option<(&str, &Arc<HashSet<String>>)>,
) -> Result<u64> {
    let cols: Vec<String> = p.columns.iter().map(|(n, _)| ident(n)).collect();
    let mut sink = conn
        .copy_in_raw(&format!(
            "COPY {name} ({}) FROM STDIN (FORMAT csv)",
            cols.join(", ")
        ))
        .await
        .with_context(|| format!("starting COPY into {name}"))?;
    for f in &p.files {
        let mut builder = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(f)?)?
            .with_batch_size(COPY_ROWS);
        if let Some((key, ids)) = only {
            let mask = ProjectionMask::columns(builder.parquet_schema(), [key]);
            builder = builder.with_row_filter(RowFilter::new(vec![Box::new(
                ArrowPredicateFn::new(mask, key_filter(Arc::clone(ids))),
            )]));
        }
        let reader = builder.build()?;
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

/// The row filter of an upsert's COPY, over the key column alone: a row is
/// kept when its key is one of `ids`, or null. The other columns are decoded
/// only for the rows kept.
fn key_filter(
    ids: Arc<HashSet<String>>,
) -> impl FnMut(RecordBatch) -> std::result::Result<BooleanArray, arrow::error::ArrowError>
+ Send
+ 'static {
    move |batch| {
        let keys = arrow::compute::cast(batch.column(0), &DataType::Utf8)?;
        let keys = keys.as_string::<i32>();
        Ok((0..keys.len())
            .map(|i| Some(keys.is_null(i) || ids.contains(keys.value(i))))
            .collect())
    }
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
                        TimeUnit::Second => c
                            .as_primitive::<TimestampSecondType>()
                            .value(row)
                            .checked_mul(1_000_000),
                        TimeUnit::Millisecond => c
                            .as_primitive::<TimestampMillisecondType>()
                            .value(row)
                            .checked_mul(1_000),
                        TimeUnit::Microsecond => {
                            Some(c.as_primitive::<TimestampMicrosecondType>().value(row))
                        }
                        TimeUnit::Nanosecond => {
                            Some(c.as_primitive::<TimestampNanosecondType>().value(row) / 1_000)
                        }
                    };
                    let t = micros
                        .and_then(chrono::DateTime::from_timestamp_micros)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
        TimestampMicrosecondArray, TimestampMillisecondArray, TimestampSecondArray,
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
    fn a_timestamp_too_large_for_microseconds_is_an_error() {
        let batch = |array: Arc<dyn Array>| {
            let schema = Arc::new(Schema::new(vec![Field::new(
                "t",
                array.data_type().clone(),
                true,
            )]));
            RecordBatch::try_new(schema, vec![array]).unwrap()
        };
        for array in [
            Arc::new(TimestampSecondArray::from(vec![i64::MAX / 1_000])) as Arc<dyn Array>,
            Arc::new(TimestampMillisecondArray::from(vec![i64::MAX / 10])),
        ] {
            let err = csv(&batch(array)).unwrap_err();
            assert!(err.to_string().contains("timestamp out of range"), "{err}");
        }
        let ok = csv(&batch(Arc::new(TimestampSecondArray::from(vec![
            1_588_291_200,
        ]))))
        .unwrap();
        assert_eq!(ok, "2020-05-01 00:00:00.000000\n");
    }

    #[test]
    fn the_key_column_is_a_resource_level_get_resource_key() {
        let key = |select: serde_json::Value| {
            let v = ViewDefinition::from_json(&serde_json::json!({
                "resourceType": "ViewDefinition", "resource": "Patient", "status": "active",
                "select": select
            }))
            .unwrap();
            key_column(&v)
        };
        let column = |name: &str, path: &str| serde_json::json!({"name": name, "path": path});
        assert_eq!(
            key(serde_json::json!([{"column": [column("pid", " getResourceKey() ")]}])),
            Some("pid".into())
        );
        // In a nested select; `id` wins over an earlier one.
        assert_eq!(
            key(serde_json::json!([
                {"column": [column("key", "getResourceKey()")]},
                {"select": [{"column": [column("id", "getResourceKey()")]}]}
            ])),
            Some("id".into())
        );
        // Under a forEach it keys the element (a contained resource); in a
        // unionAll, a path, or `id` alone, it is not taken.
        assert_eq!(
            key(serde_json::json!([
                {"forEach": "contained", "column": [column("id", "getResourceKey()")]},
                {"unionAll": [{"column": [column("u", "getResourceKey()")]}]},
                {"column": [column("id", "id")]}
            ])),
            None
        );
    }

    #[test]
    fn the_key_filter_keeps_listed_and_null_keys() {
        let ids: HashSet<String> = ["a".to_string(), "c".to_string()].into();
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![
                Some("a"),
                Some("b"),
                None,
                Some("c"),
            ]))],
        )
        .unwrap();
        let kept = key_filter(Arc::new(ids))(batch).unwrap();
        assert_eq!(kept, BooleanArray::from(vec![true, false, true, true]));
    }

    #[test]
    fn the_database_is_named_without_credentials() {
        assert_eq!(
            identity(
                "postgres://u:secret@db.example.org/analytics?sslmode=require",
                "s"
            ),
            "db.example.org:5432/analytics schema s"
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
