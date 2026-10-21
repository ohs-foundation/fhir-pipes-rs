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
//! log cannot vouch for what the database holds (`upsert_loads`, `write`);
//! only those tables are analyzed after the commit.
//!
//! Each upsert load records a mark of its own (a UUID) for every table it
//! loads in `<schema>.fhir_pipes_loads`, in the same transaction, and the
//! change log records the mark after the commit. A table whose row there is
//! not the log's mark was last loaded by something else (another warehouse,
//! a replace load, a restored backup, the same URL pointing elsewhere) and
//! is loaded in full. A replace load removes the rows of the tables it loads
//! or empties, when that table exists; it needs no more privileges than
//! before otherwise. A relation of that name is taken for the records table
//! only when upsert made it (`classify_records`); any other is never written to. A table only needs ordinary privileges (SELECT, INSERT,
//! DELETE) for either mode; the key column's index is made only by the
//! table's owner.

use std::collections::{BTreeMap, HashSet};
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
use parquet::arrow::arrow_reader::{ArrowPredicateFn, RowFilter};
use sqlx_core::connection::Connection;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::PgConnection;

use crate::error::usage;
use crate::export::changes::Changes;

use super::config::{Database, Mode, Settings};
use super::partition::{open_parquet, table_files_checked};
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
    /// Its view calls a function of the time (`time_dependent`).
    pub time_dependent: bool,
}

/// The table, in the target schema, where upsert records which load wrote
/// each table last.
const LOADS: &str = "fhir_pipes_loads";

/// The comment upsert gives `fhir_pipes_loads` when it creates it. A
/// relation of that name is taken for the records table only when it is an
/// ordinary table with this comment and exactly its columns (`classify_records`).
const RECORDS: &str = "fhir-pipes load records, version 1";

/// What the relation named `fhir_pipes_loads` in the target schema is.
enum Records {
    Absent,
    /// The records table, as upsert created it.
    Ours,
    /// Something else of that name, with what it is.
    Other(String),
}

/// Whether the view `v` calls `now()`, `today()` or `timeOfDay()` in any of
/// its expressions (columns, `where`, `forEach`, `repeat`, nested selects and
/// `unionAll`): its rows can change while its resources do not, so upsert
/// loads its table in full every run. The expressions are read with the
/// views engine's FHIRPath parser, so comments and delimited names are no
/// way round it; an expression that does not parse counts as calling one.
pub fn time_dependent(v: &ViewDefinition) -> bool {
    const TIME: [&str; 3] = ["now", "today", "timeOfDay"];
    let calls = |path: &str| fhir_pipes_sof::calls_any(path, &TIME).unwrap_or(true);
    fn walk(selects: &[SelectColumn], calls: &dyn Fn(&str) -> bool) -> bool {
        selects.iter().any(|s| {
            s.path.iter().any(|p| calls(p))
                || s.for_each.iter().any(|p| calls(p))
                || s.for_each_or_null.iter().any(|p| calls(p))
                || s.repeat.iter().any(|p| calls(p))
                || s.column.iter().flatten().any(|c| calls(&c.path))
                || walk(&s.select, calls)
                || s.union_all.as_deref().is_some_and(|u| walk(u, calls))
        })
    }
    v.where_.iter().any(|w| calls(&w.path)) || walk(&v.select, &calls)
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
    if upsert && tables.iter().any(|t| t.name == LOADS) {
        return Err(usage(anyhow::anyhow!(
            "[database] mode = \"upsert\" keeps its records in a table named {LOADS}, \
             which is also a view's table; rename the view or leave it out of [database] tables"
        )));
    }
    let mut plans = Vec::new();
    for t in tables {
        let column = super::partition::partition_key(&t.dir, &s.partition_column);
        let mut p = plan(&t.dir, &t.name, &column)?;
        if upsert && let Some(key) = &t.key {
            p.key_nulls = key_has_nulls(&p.files, key)?;
            p.key = Some(key.clone());
        }
        plans.push(p);
    }
    let (loads, marks) = if upsert {
        let (loads, last) = upsert_loads(s, tables, &plans, built)?;
        let next = uuid::Uuid::new_v4().to_string();
        (loads, Some((last, next)))
    } else {
        (plans.iter().map(|_| Load::Full(None)).collect(), None)
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the database runtime")?;
    rt.block_on(write(&url, &d.schema, &plans, loads, marks.as_ref()))?;
    if let Some((_, next)) = &marks {
        record_load(s, next, &plans, built)?;
    }
    Ok(())
}

/// Whether some row of `files` may have no `key`: by the files' statistics,
/// and yes where a file has none.
fn key_has_nulls(files: &[PathBuf], key: &str) -> Result<bool> {
    for f in files {
        let builder = open_parquet(f)?;
        let Some(i) = builder
            .parquet_schema()
            .columns()
            .iter()
            .position(|c| c.path().string() == key)
        else {
            continue;
        };
        for rg in builder.metadata().row_groups() {
            if rg.num_rows() == 0 {
                continue;
            }
            match rg.column(i).statistics().and_then(|s| s.null_count_opt()) {
                Some(0) => {}
                _ => return Ok(true),
            }
        }
    }
    Ok(false)
}

/// How a table is loaded.
enum Load {
    /// Every row; in upsert, with why, when that is the table's own.
    Full(Option<String>),
    /// The rows of these resources, and the rows without a key (which no
    /// id names, so they are rewritten whenever there are any).
    Ids {
        key: String,
        ids: Arc<HashSet<String>>,
    },
    /// None of its resources changed; its rows without a key, if there are
    /// any, are rewritten all the same.
    Unchanged,
}

/// How upsert loads each table, and the last load's mark, if the log has
/// one. Every table in full when the change log cannot vouch for the
/// database: there is none, a refresh did not finish, `--full` asked for it,
/// or the views step did not run in this refresh (the Parquet may predate
/// what the log lists). A table in full when its view depends on the time,
/// what its rows are built from changed, or its type is marked `full`. Its
/// ids otherwise; `write` finds a table that is new, whose columns changed,
/// or that another load wrote last.
fn upsert_loads(
    s: &Settings,
    tables: &[Table],
    plans: &[Plan],
    built: Option<&Built>,
) -> Result<(Vec<Load>, Option<String>)> {
    let log = Changes::read(&s.raw_dir());
    let last = log
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(|c| c.load.clone());
    let all = match (&log, built) {
        (Err(why), _) => Some(why.clone()),
        (Ok(None), _) => Some("no record of the last load".to_string()),
        (Ok(Some(c)), _) if c.reload.is_some() => c.reload.clone(),
        (_, None) => Some("the views step did not run in this refresh".to_string()),
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
        if t.time_dependent {
            loads.push(Load::Full(Some(
                "its view calls now(), today() or timeOfDay()".to_string(),
            )));
            continue;
        }
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
    Ok((loads, last))
}

/// After an upsert's commit: the change log now starts from this load, and
/// is no longer running. With
/// the views step in this run, what it listed is in the database, and each
/// table is recorded with what it was built from. Without it the Parquet may
/// be older than the log, so the ids are kept and no table is recorded, and
/// the next load is in full.
fn record_load(s: &Settings, mark: &str, plans: &[Plan], built: Option<&Built>) -> Result<()> {
    let raw = s.raw_dir();
    let mut c = Changes::read(&raw).ok().flatten().unwrap_or_default();
    // The log now matches the database. What is left of the run (publish,
    // hooks, push) writes neither, so a failure there need not make the
    // next load a full one.
    c.running = false;
    c.load = Some(mark.to_string());
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

/// One table to load: its columns and files, and in upsert its key column
/// and whether some rows may have no key.
struct Plan {
    table: String,
    columns: Vec<(String, &'static str)>,
    files: Vec<PathBuf>,
    key: Option<String>,
    key_nulls: bool,
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
        let reader = open_parquet(f)?;
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
        key_nulls: false,
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

/// `marks`, in upsert: the last load's mark as the log has it, and this
/// load's.
async fn write(
    url: &str,
    schema: &str,
    plans: &[Plan],
    mut loads: Vec<Load>,
    marks: Option<&(Option<String>, String)>,
) -> Result<()> {
    let upsert = marks.is_some();
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
    // Made only when missing: PostgreSQL checks the database's CREATE
    // privilege before IF NOT EXISTS, so a role with only table privileges
    // could not run it even for an existing schema.
    let exists: Option<i32> =
        sqlx_core::query_scalar::query_scalar("SELECT 1 FROM pg_namespace WHERE nspname = $1")
            .bind(schema)
            .fetch_optional(&mut *tx)
            .await
            .context("looking the schema up")?;
    if exists.is_none() {
        exec(&mut tx, &format!("CREATE SCHEMA {}", ident(schema))).await?;
    }
    let name = |p: &Plan| format!("{}.{}", ident(schema), ident(&p.table));
    let loads_table = format!("{}.{}", ident(schema), ident(LOADS));
    // Upsert's records table; a relation of its name that is not it is
    // never written to. Upsert stops; replace, which does not need it,
    // leaves it alone and says so. The records table is locked (ROW
    // EXCLUSIVE, which INSERT, UPDATE or DELETE on it allows, and which a
    // RENAME, DROP or ALTER waits for until this transaction ends) and
    // checked again under the lock, so the table written below is the one
    // checked.
    let mut records = classify_records(&mut tx, &loads_table).await?;
    if matches!(records, Records::Ours) {
        let grant = if upsert {
            "SELECT, INSERT, UPDATE, DELETE ON"
        } else {
            "SELECT, DELETE ON"
        };
        exec(
            &mut tx,
            &format!("LOCK TABLE {loads_table} IN ROW EXCLUSIVE MODE"),
        )
        .await
        .with_context(|| loads_grant(schema, grant, &loads_table))?;
        records = classify_records(&mut tx, &loads_table).await?;
    }
    if let Records::Other(what) = &records {
        if upsert {
            return Err(usage(anyhow::anyhow!(
                "{schema}.{LOADS} is {what}, not the table where [database] mode = \"upsert\" \
                 records its loads; rename it, load into another schema, or use mode = \"replace\""
            )));
        }
        println!("   note: {schema}.{LOADS} is {what}, not fhir-pipes' load records; left alone");
    }
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
                // Only the owner can index a table; without the index the
                // deletes scan the table, which is slower, not wrong.
                if owns(&mut tx, &name(p)).await? {
                    exec(
                        &mut tx,
                        &format!("CREATE INDEX ON {} ({})", name(p), ident(key)),
                    )
                    .await?;
                } else {
                    println!(
                        "   {}: no index on {key}, and only the table's owner can make one; \
                         upsert's deletes scan the table until the owner runs CREATE INDEX ON {} ({})",
                        p.table,
                        name(p),
                        ident(key)
                    );
                }
            }
        }
    }
    // Made the first time after the view tables and their indexes, so that
    // its primary key's index cannot take a name one of them needs.
    if upsert && matches!(records, Records::Absent) {
        exec(
            &mut tx,
            &format!(
                "CREATE TABLE {loads_table} (table_name text PRIMARY KEY, mark text NOT NULL)"
            ),
        )
        .await
        .with_context(|| loads_grant(schema, "CREATE ON SCHEMA", &ident(schema)))?;
        exec(
            &mut tx,
            &format!("COMMENT ON TABLE {loads_table} IS '{RECORDS}'"),
        )
        .await?;
        records = Records::Ours;
    }
    // A table the last load of this log did not write may hold anything.
    if let Some((last, _)) = marks {
        let written = marks_of(&mut tx, &loads_table, schema).await?;
        for (i, p) in plans
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.files.is_empty())
        {
            if matches!(loads[i], Load::Full(_)) {
                continue;
            }
            if last.is_none() || written.get(&p.table) != last.as_ref() {
                loads[i] = Load::Full(Some(
                    "the database's copy was not last loaded from this warehouse".to_string(),
                ));
            }
        }
    }
    // Rows without a key are not named by any id: rewritten whenever the
    // files or the table have any.
    for (i, p) in plans
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.files.is_empty())
    {
        if let (Load::Unchanged, Some(key)) = (&loads[i], &p.key)
            && (p.key_nulls || has_null_keys(&mut tx, &name(p), key).await?)
        {
            loads[i] = Load::Ids {
                key: key.clone(),
                ids: Arc::default(),
            };
        }
    }
    // The tables emptied for want of files, whose marks are removed.
    let mut emptied = Vec::new();
    for (p, load) in plans.iter().zip(loads) {
        let name = name(p);
        if p.files.is_empty() {
            // A table written before empty results got an empty file has no
            // columns to create it from: empty it if it exists.
            if table_exists(&mut tx, schema, &p.table).await? {
                exec(&mut tx, &format!("DELETE FROM {name}")).await?;
                println!("   {} → {schema}.{} (0 rows)", p.table, p.table);
                loaded.push(name);
                emptied.push(p.table.clone());
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
                if ids.is_empty() {
                    println!(
                        "   {} → {schema}.{} (rows without an id: {deleted} deleted, {rows} copied)",
                        p.table, p.table
                    );
                } else {
                    println!(
                        "   {} → {schema}.{} ({} resources changed or deleted: {deleted} rows deleted, {rows} copied)",
                        p.table,
                        p.table,
                        ids.len()
                    );
                }
            }
            Load::Unchanged => println!("   {} → {schema}.{} (unchanged)", p.table, p.table),
        }
    }
    // The marks: in upsert, this load's for each table it loaded and none
    // for one it emptied; in replace, none for any table it touched, so
    // that upsert knows the rows are not its own. Without the records table
    // (no upsert has loaded this schema) a replace does nothing here.
    let filled: Vec<&str> = plans
        .iter()
        .filter(|p| !p.files.is_empty())
        .map(|p| p.table.as_str())
        .collect();
    match marks {
        Some((_, next)) => {
            set_marks(&mut tx, &loads_table, &filled, next)
                .await
                .with_context(|| {
                    loads_grant(schema, "SELECT, INSERT, UPDATE, DELETE ON", &loads_table)
                })?;
            let emptied: Vec<&str> = emptied.iter().map(String::as_str).collect();
            remove_marks(&mut tx, &loads_table, &emptied)
                .await
                .with_context(|| {
                    loads_grant(schema, "SELECT, INSERT, UPDATE, DELETE ON", &loads_table)
                })?;
        }
        None if matches!(records, Records::Ours) => {
            let touched: Vec<&str> = filled
                .iter()
                .copied()
                .chain(emptied.iter().map(String::as_str))
                .collect();
            remove_marks(&mut tx, &loads_table, &touched)
                .await
                .with_context(|| loads_grant(schema, "SELECT, DELETE ON", &loads_table))?;
        }
        None => {}
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

/// Why a statement on the records table failed, as the grant it needs.
fn loads_grant(schema: &str, privileges: &str, object: &str) -> String {
    format!(
        "{schema}.{LOADS} records which load wrote each table, for [database] mode = \"upsert\"; \
         this load must update it: GRANT {privileges} {object} TO the role in the connection string"
    )
}

/// The mark the records table `loads` has for each table of `schema`.
async fn marks_of(
    conn: &mut PgConnection,
    loads: &str,
    schema: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let sql = format!("SELECT table_name, mark FROM {loads}");
    let rows = sqlx_core::query::query(AssertSqlSafe(sql.clone()))
        .fetch_all(&mut *conn)
        .await
        .with_context(|| loads_grant(schema, "SELECT, INSERT, UPDATE, DELETE ON", loads))?;
    rows.iter()
        .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
        .collect::<std::result::Result<_, sqlx_core::error::Error>>()
        .with_context(|| format!("reading {sql}"))
}

/// Record `mark` for `tables` in the records table `loads`.
async fn set_marks(
    conn: &mut PgConnection,
    loads: &str,
    tables: &[&str],
    mark: &str,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO {loads} (table_name, mark) SELECT t, $2 FROM unnest($1::text[]) AS t \
         ON CONFLICT (table_name) DO UPDATE SET mark = excluded.mark"
    );
    sqlx_core::query::query(AssertSqlSafe(sql.clone()))
        .bind(tables.to_vec())
        .bind(mark)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {sql}"))?;
    Ok(())
}

/// Remove the marks of `tables` from the records table `loads`.
async fn remove_marks(conn: &mut PgConnection, loads: &str, tables: &[&str]) -> Result<()> {
    if tables.is_empty() {
        return Ok(());
    }
    let sql = format!("DELETE FROM {loads} WHERE table_name = ANY($1)");
    sqlx_core::query::query(AssertSqlSafe(sql.clone()))
        .bind(tables.to_vec())
        .execute(&mut *conn)
        .await
        .with_context(|| format!("running {sql}"))?;
    Ok(())
}

/// What the relation `name` is, read from the catalogs, which show it
/// whatever the role may read of it (information_schema shows only what it
/// has privileges on).
async fn classify_records(conn: &mut PgConnection, name: &str) -> Result<Records> {
    let row = sqlx_core::query::query(
        "SELECT c.relkind::text, obj_description(c.oid, 'pg_class'), \
         (SELECT string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod), ', ' \
          ORDER BY a.attnum) FROM pg_attribute a \
          WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped) \
         FROM pg_class c WHERE c.oid = to_regclass($1)",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .with_context(|| format!("looking for {name}"))?;
    let Some(row) = row else {
        return Ok(Records::Absent);
    };
    let (kind, comment, columns) = (|| {
        Ok::<_, sqlx_core::error::Error>((
            row.try_get::<String, _>(0)?,
            row.try_get::<Option<String>, _>(1)?,
            row.try_get::<Option<String>, _>(2)?,
        ))
    })()
    .with_context(|| format!("reading what {name} is"))?;
    let what = match kind.as_str() {
        "r" => "a table",
        "v" => "a view",
        "m" => "a materialized view",
        "p" => "a partitioned table",
        "f" => "a foreign table",
        _ => "a relation",
    };
    if kind == "r"
        && comment.as_deref() == Some(RECORDS)
        && columns.as_deref() == Some("table_name text, mark text")
    {
        Ok(Records::Ours)
    } else {
        Ok(Records::Other(what.to_string()))
    }
}

/// Whether the role in the connection owns table `name` (or is a member of
/// the role that does).
async fn owns(conn: &mut PgConnection, name: &str) -> Result<bool> {
    sqlx_core::query::query(
        "SELECT pg_has_role(relowner, 'USAGE') FROM pg_class WHERE oid = $1::text::regclass",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await
    .and_then(|r| r.try_get::<bool, _>(0))
    .with_context(|| format!("reading the owner of {name}"))
}

/// Whether `name` has rows without a `key`.
async fn has_null_keys(conn: &mut PgConnection, name: &str, key: &str) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM {name} WHERE {} IS NULL)",
        ident(key)
    );
    sqlx_core::query::query(AssertSqlSafe(sql.clone()))
        .fetch_one(&mut *conn)
        .await
        .and_then(|r| r.try_get::<bool, _>(0))
        .with_context(|| format!("running {sql}"))
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
        let mut builder = open_parquet(f)?.with_batch_size(COPY_ROWS);
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
    fn views_that_call_a_time_function_are_found() {
        let view = |select: serde_json::Value, filter: serde_json::Value| {
            ViewDefinition::from_json(&serde_json::json!({
                "resourceType": "ViewDefinition", "resource": "Patient", "status": "active",
                "select": select, "where": filter
            }))
            .unwrap()
        };
        let column = |path: &str| serde_json::json!([{"column": [{"name": "c", "path": path}]}]);
        for path in [
            "now()",
            "today ( ).toString()",
            "a.where(b < timeOfDay())",
            "now /* a comment */ ().toString()",
            "`now`().toString()",
            "now // a comment\n().toString()",
            "birthDate.now()",
            // Not FHIRPath: it may call anything.
            "now((",
        ] {
            assert!(
                time_dependent(&view(column(path), serde_json::json!([]))),
                "{path}"
            );
        }
        assert!(time_dependent(&view(
            column("id"),
            serde_json::json!([{"path": "birthDate < today()"}])
        )));
        assert!(time_dependent(&view(
            serde_json::json!([{"unionAll": [{"forEach": "name.where(period.end > now())",
                "column": [{"name": "c", "path": "family"}]}]}]),
            serde_json::json!([])
        )));
        for path in ["known()", "snow", "'now()'", "today", "now.x", "birthDate"] {
            assert!(
                !time_dependent(&view(column(path), serde_json::json!([]))),
                "{path}"
            );
        }
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
