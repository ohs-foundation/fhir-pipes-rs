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
//! log cannot vouch for what the database holds (`decide_loads`);
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, AsArray, BooleanArray, RecordBatch};
use arrow::datatypes::{
    DataType, Float32Type, Float64Type, Int64Type, TimeUnit, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType,
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
use crate::library::compose::{Bind, compose};
use crate::library::resolve::{Artifact, Catalog, HIERARCHY_TABLE, HIERARCHY_URL};
use crate::library::{Kind, Library};
use crate::views::run::View;

use super::config::{Database, Mode, Settings};
use super::libraries_step;
use super::partition::{open_parquet, table_files_checked};
use super::sql::{ident, sql_str};

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
    /// Made from other tables (`location_hierarchy`, a SQLQuery Library's
    /// result) rather than from a view: no resource key, loaded in full in
    /// both modes, and no mark in the records table.
    pub derived: bool,
}

/// A derived table: `location_hierarchy` or a SQLQuery Library's result,
/// read from `dir`.
pub fn derived_table(name: &str, dir: PathBuf) -> Table {
    Table {
        name: name.to_string(),
        dir,
        resource: String::new(),
        key: None,
        time_dependent: true,
        derived: true,
    }
}

/// The derived tables a load takes, after the view tables:
/// `location_hierarchy` when the warehouse has it, then each SQLQuery
/// Library's result in `catalog` (when the config has `libraries`), in the
/// order they run. `dir` gives where a table is read. A result with no table
/// yet (the libraries step has not run) is left out by `load`, which says
/// so, and the database's copy, if any, left as it is.
pub fn derived_tables(
    s: &Settings,
    catalog: Option<&Catalog>,
    dir: impl Fn(&str) -> PathBuf,
) -> Result<Vec<Table>> {
    let mut out = Vec::new();
    let hierarchy = dir(HIERARCHY_TABLE);
    if hierarchy.is_dir() {
        out.push(derived_table(HIERARCHY_TABLE, hierarchy));
    }
    let Some(catalog) = catalog else {
        return Ok(out);
    };
    for lib in catalog.order()? {
        if lib.kind != Kind::SqlQuery {
            continue;
        }
        let table = lib.table(&s.strip_prefix);
        out.push(derived_table(&table, dir(&table)));
    }
    Ok(out)
}

/// The comment that marks a view as a SQLView fhir-pipes made: followed by
/// the Library's canonical. A load drops every view in its schema with it
/// before it makes them again.
const SQLVIEW_COMMENT: &str = "fhir-pipes SQLView ";

/// A SQLView Library as a PostgreSQL view.
pub struct SqlView {
    /// The Library's name, for messages.
    pub library: String,
    /// The view's name: the Library's table.
    pub name: String,
    /// `CREATE VIEW <schema>.<name> AS <the Library's SQL>`.
    pub create: String,
    /// The view's comment: `fhir-pipes SQLView <canonical>`.
    pub comment: String,
}

/// Each SQLView Library in `catalog` but the built-in LocationHierarchy, in
/// `order()`, as a view in `schema`. Each input is read from its relation in
/// `schema`: a view's table, `location_hierarchy`, a SQLQuery's result, or
/// another SQLView's view (so no SQLView is inlined). A view input must be
/// one the views directory writes, as the libraries step requires
/// (`libraries_step::written`): the database may still hold the table of a
/// view removed from it. This runs before the load drops or loads anything.
pub fn sql_views(s: &Settings, catalog: &Catalog, schema: &str) -> Result<Vec<SqlView>> {
    let relation = |a: &Artifact| match a {
        Artifact::Library(l) if l.url == HIERARCHY_URL => HIERARCHY_TABLE.to_string(),
        _ => a.table(&s.strip_prefix),
    };
    let bind = |a: &Artifact| -> Result<Option<Bind>> {
        Ok(Some(Bind::Relation(qualified(schema, &relation(a)))))
    };
    let mut view_tables = None;
    let mut out = Vec::new();
    for lib in catalog.order()? {
        if lib.kind != Kind::SqlView || lib.url == HIERARCHY_URL {
            continue;
        }
        let view_tables = match &view_tables {
            Some(t) => t,
            None => view_tables.insert(libraries_step::view_tables(s)?),
        };
        for dep in &lib.depends_on {
            let input = catalog
                .get(&dep.canonical, dep.version.as_deref())
                .with_context(|| format!("Library {} depends-on {}", lib.name, dep.label))
                .map_err(usage)?;
            let table = relation(input);
            if !libraries_step::written(input, &table, view_tables) {
                return Err(usage(libraries_step::missing(
                    lib, lib, dep, &table, input, true,
                )));
            }
        }
        let name = lib.table(&s.strip_prefix);
        let select = compose(lib, "postgresql", catalog, &bind)
            .with_context(|| format!("Library {}", lib.name))
            .map_err(usage)?;
        out.push(SqlView {
            library: lib.name.clone(),
            create: format!("CREATE VIEW {} AS\n{select}", qualified(schema, &name)),
            comment: format!("{SQLVIEW_COMMENT}{}", canonical(lib)),
            name,
        });
    }
    Ok(out)
}

/// `url|version`, or the url alone.
fn canonical(lib: &Library) -> String {
    match &lib.version {
        Some(v) => format!("{}|{v}", lib.url),
        None => lib.url.clone(),
    }
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

/// The views whose tables [database] loads: every view's, sorted by table,
/// or those of the tables `tables` names, in its order, each of which must
/// be a view's.
pub fn view_tables(s: &Settings, d: &Database) -> Result<Vec<View>> {
    let dir = s
        .views
        .as_ref()
        .context("[database] loads the view tables, so it needs the views directory")
        .map_err(usage)?;
    let mut all: BTreeMap<String, View> = crate::views::run::discover(dir, &s.strip_prefix)
        .map_err(usage)?
        .into_iter()
        .map(|v| (v.table.clone(), v))
        .collect();
    match &d.tables {
        None => Ok(all.into_values().collect()),
        Some(names) => {
            if let Some(bad) = names.iter().find(|n| !all.contains_key(*n)) {
                return Err(usage(anyhow::anyhow!(
                    "[database] tables: `{bad}` is not a view table ({})",
                    all.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
            Ok(names.iter().filter_map(|n| all.remove(n)).collect())
        }
    }
}

/// Load `tables` into the database `d` points at. `built`: what the views
/// step wrote in this run, for upsert.
/// `committed` is set to `Unknown` as the commit is sent and to `Yes` once it
/// succeeds; a failure after it (an `ANALYZE`) leaves it `Yes`.
pub fn load(
    s: &Settings,
    d: &Database,
    tables: Vec<Table>,
    views: &[SqlView],
    built: Option<&Built>,
    committed: &mut super::status::Committed,
) -> Result<()> {
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
    let (tables, unwritten): (Vec<Table>, Vec<Table>) = tables
        .into_iter()
        .partition(|t| !t.derived || t.dir.is_dir());
    let tables = &tables[..];
    for t in unwritten {
        println!(
            "   {}: no table in the warehouse yet; run the libraries step",
            t.name
        );
    }
    let mut plans = Vec::new();
    for t in tables {
        let column = super::partition::partition_key(&t.dir, &s.partition_column);
        let mut p = plan(&t.dir, &t.name, &column)?;
        p.derived = t.derived;
        if upsert && let Some(key) = &t.key {
            p.key_nulls = key_has_nulls(&p.files, key)?;
            p.key = Some(key.clone());
        }
        plans.push(p);
    }
    let upsert = if upsert {
        let u = Upsert::read(s, built);
        check_keys(tables, &plans)?;
        Some(u)
    } else {
        None
    };
    crate::runtime::block_on(write(
        &url,
        &d.schema,
        tables,
        &plans,
        views,
        upsert.as_ref(),
        committed,
    ))
    .context("starting the database runtime")??;
    // A load of derived tables alone says nothing of the view tables: the
    // change log is left to the next load that has them.
    if let Some(u) = &upsert
        && tables.iter().any(|t| !t.derived)
    {
        record_load(s, &u.next, &plans, built)?;
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

/// What upsert knows before it connects: the change log, when it can vouch
/// for the database, and the marks of the last load and of this one.
struct Upsert<'a> {
    /// The log, and what the views step wrote in this run. None when every
    /// table is loaded in full: there is no log, a refresh did not finish,
    /// `--full` asked for it, or the views step did not run in this refresh
    /// (the Parquet may predate what the log lists).
    vouched: Option<(Changes, &'a Built)>,
    /// The last load's mark, if the log has one.
    last: Option<String>,
    /// This load's mark.
    next: String,
}

impl<'a> Upsert<'a> {
    /// Reads the change log, saying so when it cannot vouch for the
    /// database.
    fn read(s: &Settings, built: Option<&'a Built>) -> Self {
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
        let vouched = match (log, all, built) {
            (Ok(Some(c)), None, Some(built)) => Some((c, built)),
            _ => None,
        };
        Self {
            vouched,
            last,
            next: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Every table needs its view's `getResourceKey()` column in upsert, as
/// text in the Parquet files.
fn check_keys(tables: &[Table], plans: &[Plan]) -> Result<()> {
    for (t, p) in tables.iter().zip(plans).filter(|(t, _)| !t.derived) {
        let key = t
            .key
            .clone()
            .ok_or_else(|| usage(anyhow::anyhow!(no_key(&t.name, &t.name))))?;
        match p.columns.iter().find(|(n, _)| *n == key) {
            Some((_, t)) if t == "text" => {}
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
    }
    Ok(())
}

/// How each table is loaded. In replace, every table in full. In upsert,
/// every table in full when the log cannot vouch for the database
/// (`Upsert::vouched`), else a table in full, with the first of these
/// reasons that holds:
///
/// - its view depends on the time;
/// - it was not in the last load, or what its rows are built from changed;
/// - its type is marked `full` in the log;
/// - it is new in the database, or its columns changed (`recreated`);
/// - another load wrote it last (`written`, the marks in the records
///   table).
///
/// Otherwise the ids the log lists for its type, or none.
fn decide_loads(
    tables: &[Table],
    plans: &[Plan],
    upsert: Option<&Upsert>,
    recreated: &[Option<&str>],
    written: Option<&HashMap<String, String>>,
) -> Vec<Load> {
    let full = |why: &str| Load::Full(Some(why.to_string()));
    tables
        .iter()
        .zip(plans)
        .zip(recreated)
        .map(|((t, p), recreated)| {
            let Some(u) = upsert else {
                return Load::Full(None);
            };
            if t.derived {
                return Load::Full(None);
            }
            let Some((c, built)) = &u.vouched else {
                return Load::Full(None);
            };
            if t.time_dependent {
                return full("its view calls now(), today() or timeOfDay()");
            }
            let last = c.tables.get(&t.name);
            if last.is_none() {
                return full("not in the last load");
            }
            if last != built.get(&t.name) {
                return full("its view, its partition lookup or fhir-pipes changed");
            }
            let types = c.types.get(&t.resource);
            if types.is_some_and(|tc| tc.full) {
                return Load::Full(Some(format!(
                    "every {} is reloaded (read without an old file to compare, or most changed)",
                    t.resource
                )));
            }
            if let Some(why) = recreated {
                return full(why);
            }
            if !p.files.is_empty()
                && (u.last.is_none() || written.and_then(|w| w.get(&p.table)) != u.last.as_ref())
            {
                return full("the database's copy was not last loaded from this warehouse");
            }
            let ids: HashSet<String> = types
                .map(|tc| tc.ids().into_iter().map(str::to_string).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                Load::Unchanged
            } else {
                Load::Ids {
                    key: t.key.clone().expect("checked by check_keys"),
                    ids: Arc::new(ids),
                }
            }
        })
        .collect()
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
    columns: Vec<(String, String)>,
    files: Vec<PathBuf>,
    key: Option<String>,
    key_nulls: bool,
    /// A derived table (`Table::derived`).
    derived: bool,
}

/// A table's columns, from its Parquet files, which must all agree.
fn plan(dir: &Path, table: &str, partition_column: &str) -> Result<Plan> {
    // A missing or unreadable directory is an error, so a wrong `--data` or a
    // missing mount cannot empty the database.
    let files = table_files_checked(dir, partition_column).with_context(|| {
        format!("{table}: cannot read its Parquet; the database was not changed")
    })?;
    let mut columns: Option<Vec<(String, String)>> = None;
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
        derived: false,
    })
}

/// The PostgreSQL type for a column the views step or DuckDB (the libraries
/// step) writes, as information_schema names it, with a numeric's precision
/// and scale (as `existing_columns` reads them).
fn pg_type(t: &DataType) -> Option<String> {
    Some(
        match t {
            DataType::Decimal128(p, s) => return Some(format!("numeric({p},{s})")),
            DataType::Utf8 | DataType::LargeUtf8 => "text",
            DataType::Int64 => "bigint",
            DataType::Int32 => "integer",
            DataType::Int16 | DataType::Int8 => "smallint",
            DataType::Float64 => "double precision",
            DataType::Float32 => "real",
            DataType::Boolean => "boolean",
            DataType::Date32 => "date",
            DataType::Timestamp(_, Some(_)) => "timestamp with time zone",
            DataType::Timestamp(_, None) => "timestamp without time zone",
            DataType::Time64(TimeUnit::Microsecond) => "time without time zone",
            _ => return None,
        }
        .to_string(),
    )
}

/// `schema.table`, quoted.
fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", ident(schema), ident(table))
}

/// Load `plans` into `schema` in one transaction, then analyze the tables
/// loaded. The SQLViews fhir-pipes made in `schema` are dropped first, so
/// that no table they read is held by them, and `views` made after the
/// tables are loaded, in the same transaction.
async fn write(
    url: &str,
    schema: &str,
    tables: &[Table],
    plans: &[Plan],
    views: &[SqlView],
    upsert: Option<&Upsert<'_>>,
    committed: &mut super::status::Committed,
) -> Result<()> {
    // TLS (sslmode=require and up) uses rustls with the same provider as the
    // FHIR client.
    crate::http::ensure_crypto_provider();
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
    let mut records = prepare_records(&mut tx, schema, upsert.is_some()).await?;
    drop_sql_views(&mut tx, schema).await?;
    let (created, recreated) = reconcile_tables(&mut tx, schema, plans).await?;
    ensure_indexes(&mut tx, schema, plans, &created, upsert.is_some()).await?;
    let loads_table = qualified(schema, LOADS);
    // Made the first time after the view tables and their indexes, so that
    // its primary key's index cannot take a name one of them needs.
    if upsert.is_some() && matches!(records, Records::Absent) {
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
    let written = match upsert {
        Some(_) => Some(marks_of(&mut tx, &loads_table, schema).await?),
        None => None,
    };
    let mut loads = decide_loads(tables, plans, upsert, &recreated, written.as_ref());
    // Rows without a key are not named by any id: rewritten whenever the
    // files or the table have any.
    for (i, p) in plans
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.files.is_empty())
    {
        if let (Load::Unchanged, Some(key)) = (&loads[i], &p.key)
            && (p.key_nulls || has_null_keys(&mut tx, &qualified(schema, &p.table), key).await?)
        {
            loads[i] = Load::Ids {
                key: key.clone(),
                ids: Arc::default(),
            };
        }
    }
    let (loaded, emptied) = apply_loads(&mut tx, schema, plans, loads).await?;
    update_marks(&mut tx, schema, plans, &emptied, upsert, &records).await?;
    create_sql_views(&mut tx, schema, views).await?;
    *committed = super::status::Committed::Unknown;
    tx.commit().await.context("committing")?;
    *committed = super::status::Committed::Yes;
    // Outside the load's transaction, so the lock it held is released and
    // readers see the new rows before this finishes.
    for name in loaded {
        exec(&mut conn, &format!("ANALYZE {name}")).await?;
    }
    Ok(())
}

/// What the relation named like upsert's records table is. A relation of
/// its name that is not it is never written to: upsert stops; replace,
/// which does not need it, leaves it alone and says so. The records table
/// is locked (ROW EXCLUSIVE, which INSERT, UPDATE or DELETE on it allows,
/// and which a RENAME, DROP or ALTER waits for until this transaction ends)
/// and checked again under the lock, so the table written later is the one
/// checked.
async fn prepare_records(conn: &mut PgConnection, schema: &str, upsert: bool) -> Result<Records> {
    let loads_table = qualified(schema, LOADS);
    let mut records = classify_records(conn, &loads_table).await?;
    if matches!(records, Records::Ours) {
        let grant = if upsert {
            "SELECT, INSERT, UPDATE, DELETE ON"
        } else {
            "SELECT, DELETE ON"
        };
        exec(
            conn,
            &format!("LOCK TABLE {loads_table} IN ROW EXCLUSIVE MODE"),
        )
        .await
        .with_context(|| loads_grant(schema, grant, &loads_table))?;
        records = classify_records(conn, &loads_table).await?;
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
    Ok(records)
}

/// Creates each table with files that is missing, and recreates each whose
/// columns differ from its files'. Returns the tables created, by index,
/// and for each table why it was, if it was.
async fn reconcile_tables(
    conn: &mut PgConnection,
    schema: &str,
    plans: &[Plan],
) -> Result<(Vec<usize>, Vec<Option<&'static str>>)> {
    // First every table, so that indexes, which PostgreSQL names to avoid
    // the relations that exist, cannot take a name a table of this load needs.
    let mut created = Vec::new();
    let mut recreated = vec![None; plans.len()];
    for (i, p) in plans
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.files.is_empty())
    {
        let existing = existing_columns(conn, schema, &p.table).await?;
        if existing == p.columns {
            continue;
        }
        recreated[i] = Some(if existing.is_empty() {
            "a new table"
        } else {
            "its columns changed"
        });
        let name = qualified(schema, &p.table);
        if !existing.is_empty() {
            exec(conn, &format!("DROP TABLE {name}"))
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
        exec(conn, &format!("CREATE TABLE {name} ({})", cols.join(", "))).await?;
        created.push(i);
    }
    Ok((created, recreated))
}

/// The indexes of the tables just `created`: unnamed, made with the table,
/// so later runs find them in place. Upsert also needs one on the key
/// column, which a table made by replace may lack.
async fn ensure_indexes(
    conn: &mut PgConnection,
    schema: &str,
    plans: &[Plan],
    created: &[usize],
    upsert: bool,
) -> Result<()> {
    for &i in created {
        let p = &plans[i];
        if p.columns.iter().any(|(n, _)| n == "id") {
            let name = qualified(schema, &p.table);
            exec(conn, &format!("CREATE INDEX ON {name} (id)")).await?;
        }
    }
    if !upsert {
        return Ok(());
    }
    for p in plans.iter().filter(|p| !p.files.is_empty()) {
        let name = qualified(schema, &p.table);
        if let Some(key) = p.key.as_deref()
            && !has_index(conn, &name, key).await?
        {
            // Only the owner can index a table; without the index the
            // deletes scan the table, which is slower, not wrong.
            if owns(conn, &name).await? {
                exec(conn, &format!("CREATE INDEX ON {name} ({})", ident(key))).await?;
            } else {
                println!(
                    "   {}: no index on {key}, and only the table's owner can make one; \
                     upsert's deletes scan the table until the owner runs CREATE INDEX ON {} ({})",
                    p.table,
                    name,
                    ident(key)
                );
            }
        }
    }
    Ok(())
}

/// Loads each table as `loads` says, and empties each table with no files
/// that exists. Returns the tables filled or emptied, to analyze after the
/// commit, and the tables emptied, whose marks are removed.
async fn apply_loads(
    conn: &mut PgConnection,
    schema: &str,
    plans: &[Plan],
    loads: Vec<Load>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut loaded = Vec::new();
    let mut emptied = Vec::new();
    for (p, load) in plans.iter().zip(loads) {
        let name = qualified(schema, &p.table);
        if p.files.is_empty() {
            // A table written before empty results got an empty file has no
            // columns to create it from: empty it if it exists.
            if table_exists(conn, schema, &p.table).await? {
                exec(conn, &format!("DELETE FROM {name}")).await?;
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
                exec(conn, &format!("DELETE FROM {name}")).await?;
                let rows = copy(conn, &name, p, None).await?;
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
                let deleted = delete_ids(conn, &name, &key, &ids).await?;
                let rows = copy(conn, &name, p, Some((&key, &ids))).await?;
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
    Ok((loaded, emptied))
}

/// The marks: in upsert, this load's for each table it loaded and none for
/// one it `emptied`; in replace, none for any table it touched, so that
/// upsert knows the rows are not its own. Without the records table (no
/// upsert has loaded this schema) a replace does nothing here.
async fn update_marks(
    conn: &mut PgConnection,
    schema: &str,
    plans: &[Plan],
    emptied: &[String],
    upsert: Option<&Upsert<'_>>,
    records: &Records,
) -> Result<()> {
    let loads_table = qualified(schema, LOADS);
    // A derived table is always loaded in full: no mark.
    let filled: Vec<&str> = plans
        .iter()
        .filter(|p| !p.files.is_empty() && !p.derived)
        .map(|p| p.table.as_str())
        .collect();
    let derived: HashSet<&str> = plans
        .iter()
        .filter(|p| p.derived)
        .map(|p| p.table.as_str())
        .collect();
    let emptied: Vec<String> = emptied
        .iter()
        .filter(|t| !derived.contains(t.as_str()))
        .cloned()
        .collect();
    match upsert {
        Some(u) => {
            set_marks(conn, &loads_table, &filled, &u.next)
                .await
                .with_context(|| {
                    loads_grant(schema, "SELECT, INSERT, UPDATE, DELETE ON", &loads_table)
                })?;
            let emptied: Vec<&str> = emptied.iter().map(String::as_str).collect();
            remove_marks(conn, &loads_table, &emptied)
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
            remove_marks(conn, &loads_table, &touched)
                .await
                .with_context(|| loads_grant(schema, "SELECT, DELETE ON", &loads_table))?;
        }
        None => {}
    }
    Ok(())
}

/// Drop every view in `schema` that fhir-pipes made for a SQLView (by its
/// comment, `SQLVIEW_COMMENT`), each before the views it reads. A view
/// something else made on one of them stops the load: it is not dropped
/// with it.
async fn drop_sql_views(conn: &mut PgConnection, schema: &str) -> Result<()> {
    let rows = sqlx_core::query::query(
        "SELECT c.oid::bigint, c.relname::text, obj_description(c.oid, 'pg_class') \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind = 'v'",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await
    .with_context(|| format!("reading the views of {schema}"))?;
    let mut ours: BTreeMap<i64, String> = BTreeMap::new();
    for r in &rows {
        let comment: Option<String> = r.try_get(2)?;
        if comment.is_some_and(|c| c.starts_with(SQLVIEW_COMMENT)) {
            ours.insert(r.try_get(0)?, r.try_get(1)?);
        }
    }
    if ours.is_empty() {
        return Ok(());
    }
    // (view, a relation it reads), among these views.
    let oids: Vec<i64> = ours.keys().copied().collect();
    let reads: Vec<(i64, i64)> = sqlx_core::query::query(
        "SELECT DISTINCT r.ev_class::bigint, d.refobjid::bigint \
         FROM pg_depend d JOIN pg_rewrite r ON r.oid = d.objid \
         WHERE d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass \
         AND r.ev_class <> d.refobjid \
         AND r.ev_class::bigint = ANY($1) AND d.refobjid::bigint = ANY($1)",
    )
    .bind(&oids)
    .fetch_all(&mut *conn)
    .await
    .context("reading what the SQLViews read")?
    .iter()
    .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
    .collect::<std::result::Result<_, sqlx_core::error::Error>>()?;
    while !ours.is_empty() {
        // A view no remaining view reads.
        let free: Vec<i64> = ours
            .keys()
            .copied()
            .filter(|v| {
                !reads
                    .iter()
                    .any(|(by, read)| read == v && ours.contains_key(by))
            })
            .collect();
        if free.is_empty() {
            bail!("the SQLViews in {schema} read each other in a cycle; drop them by hand");
        }
        for v in free {
            let name = ours.remove(&v).expect("a remaining view");
            exec(conn, &format!("DROP VIEW {}", qualified(schema, &name)))
                .await
                .with_context(|| {
                    format!(
                        "dropping the view {schema}.{name}, a SQLView fhir-pipes made, to make \
                         it again; drop what depends on it first"
                    )
                })?;
        }
    }
    Ok(())
}

/// Make each of `views` in `schema`, with its comment. A view PostgreSQL
/// cannot make fails the load, which rolls back.
async fn create_sql_views(conn: &mut PgConnection, schema: &str, views: &[SqlView]) -> Result<()> {
    for v in views {
        sqlx_core::query::query(AssertSqlSafe(v.create.clone()))
            .execute(&mut *conn)
            .await
            .with_context(|| {
                format!(
                    "Library {}: creating the view {schema}.{}; the database was not changed",
                    v.library, v.name
                )
            })?;
        exec(
            conn,
            &format!(
                "COMMENT ON VIEW {} IS {}",
                qualified(schema, &v.name),
                sql_str(&v.comment)
            ),
        )
        .await?;
        println!("   {} → {schema}.{} (view)", v.library, v.name);
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
) -> Result<HashMap<String, String>> {
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
        "SELECT column_name::text, CASE WHEN data_type = 'numeric' THEN 'numeric(' || \
         coalesce(numeric_precision::text, '') || ',' || coalesce(numeric_scale::text, '') || ')' \
         ELSE data_type::text END FROM information_schema.columns \
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
            | DataType::Float32
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
                    float(&mut out, v, || v.to_string());
                }
                DataType::Float32 => {
                    let v = column.as_primitive::<Float32Type>().value(row);
                    float(&mut out, f64::from(v), || v.to_string());
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

/// A float as PostgreSQL reads it: `text`, Rust's shortest text that reads
/// back to the value; NaN and the infinities by PostgreSQL's names. `v` is
/// the value, widened.
fn float(out: &mut String, v: f64, text: impl FnOnce() -> String) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        out.push_str(&text());
    }
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
        BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int8Array,
        Int16Array, Int64Array, StringArray, TimestampMicrosecondArray, TimestampMillisecondArray,
        TimestampSecondArray,
    };
    use arrow::datatypes::{Field, Schema};

    use super::*;

    /// Each table's load and, for a full one, the first reason that holds.
    #[test]
    fn a_load_in_full_says_the_first_reason_that_holds() {
        use crate::export::changes::TypeChanges;
        let table = |name: &str, resource: &str, time_dependent: bool| Table {
            name: name.into(),
            dir: PathBuf::new(),
            resource: resource.into(),
            key: Some("id".into()),
            time_dependent,
            derived: false,
        };
        let plan = |name: &str| Plan {
            table: name.into(),
            columns: vec![("id".into(), "text".into())],
            files: vec![PathBuf::from("part-0.parquet")],
            key: Some("id".into()),
            key_nulls: false,
            derived: false,
        };
        let names = [
            "now", "new", "patients", "built", "every", "dropped", "other", "ids", "same",
        ];
        let tables = [
            table("now", "Patient", true),
            table("new", "Patient", false),
            table("patients", "Patient", false),
            table("built", "Patient", false),
            table("every", "Encounter", false),
            table("dropped", "Patient", false),
            table("other", "Patient", false),
            table("ids", "Patient", false),
            table("same", "Location", false),
        ];
        let plans: Vec<Plan> = names.iter().map(|n| plan(n)).collect();
        let mut log = Changes::default();
        let mut built = Built::new();
        for n in names.iter().filter(|n| **n != "new") {
            log.tables.insert(n.to_string(), "f1".into());
            built.insert(n.to_string(), "f1".into());
        }
        built.insert("built".into(), "f2".into());
        built.insert("new".into(), "f1".into());
        log.types.insert(
            "Encounter".into(),
            TypeChanges {
                full: true,
                ..TypeChanges::default()
            },
        );
        log.types.insert(
            "Patient".into(),
            TypeChanges {
                changed: ["p1".to_string()].into(),
                ..TypeChanges::default()
            },
        );
        let upsert = Upsert {
            vouched: Some((log, &built)),
            last: Some("m1".into()),
            next: "m2".into(),
        };
        let mut recreated = vec![None; names.len()];
        // A new table's reason comes after those from the log.
        recreated[0] = Some("a new table");
        recreated[5] = Some("a new table");
        let written: HashMap<String, String> = names
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    if *n == "other" { "m0" } else { "m1" }.to_string(),
                )
            })
            .collect();
        let loads = decide_loads(&tables, &plans, Some(&upsert), &recreated, Some(&written));
        let got: Vec<String> = loads
            .iter()
            .map(|l| match l {
                Load::Full(why) => format!("full: {}", why.as_deref().unwrap_or("")),
                Load::Ids { ids, .. } => format!("{} ids", ids.len()),
                Load::Unchanged => "unchanged".into(),
            })
            .collect();
        assert_eq!(
            got,
            [
                "full: its view calls now(), today() or timeOfDay()",
                "full: not in the last load",
                "1 ids",
                "full: its view, its partition lookup or fhir-pipes changed",
                "full: every Encounter is reloaded (read without an old file to compare, or most changed)",
                "full: a new table",
                "full: the database's copy was not last loaded from this warehouse",
                "1 ids",
                "unchanged",
            ]
        );
        // Without a log that vouches for the database, and in replace,
        // every table in full with no reason of its own.
        let unvouched = Upsert {
            vouched: None,
            ..upsert
        };
        for upsert in [Some(&unvouched), None] {
            let loads = decide_loads(&tables, &plans, upsert, &recreated, Some(&written));
            assert!(loads.iter().all(|l| matches!(l, Load::Full(None))));
        }
    }

    /// A derived table has no resource key: upsert loads it in full, with no
    /// reason of its own, and does not ask for its key.
    #[test]
    fn a_derived_table_is_loaded_in_full_without_a_key() {
        let mut log = Changes::default();
        log.tables.insert("location_hierarchy".into(), "f1".into());
        let built = Built::new();
        let upsert = Upsert {
            vouched: Some((log, &built)),
            last: Some("m1".into()),
            next: "m2".into(),
        };
        let tables = [derived_table("location_hierarchy", PathBuf::new())];
        let plans = [Plan {
            table: "location_hierarchy".into(),
            columns: vec![("id".into(), "text".into())],
            files: vec![PathBuf::from("part-0.parquet")],
            key: None,
            key_nulls: false,
            derived: true,
        }];
        let loads = decide_loads(&tables, &plans, Some(&upsert), &[None], None);
        assert!(matches!(loads[..], [Load::Full(None)]));
        check_keys(&tables, &plans).unwrap();
    }

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

    /// DuckDB's types: DECIMAL, SMALLINT, TINYINT and REAL.
    #[test]
    fn csv_writes_decimals_small_integers_and_reals() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("d", DataType::Decimal128(10, 2), true),
            Field::new("s", DataType::Int16, true),
            Field::new("t", DataType::Int8, true),
            Field::new("r", DataType::Float32, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(
                    Decimal128Array::from(vec![Some(150), Some(-3), None])
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
                Arc::new(Int16Array::from(vec![Some(1), None, Some(-32768)])),
                Arc::new(Int8Array::from(vec![Some(2), Some(-128), None])),
                Arc::new(Float32Array::from(vec![
                    Some(0.1),
                    Some(f32::NEG_INFINITY),
                    Some(f32::NAN),
                ])),
            ],
        )
        .unwrap();
        assert_eq!(
            csv(&batch).unwrap(),
            "1.50,1,2,0.1\n-0.03,,-128,-Infinity\n,-32768,,NaN\n"
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
        assert_eq!(pg_type(&DataType::Utf8).as_deref(), Some("text"));
        for (t, pg) in [
            (DataType::Decimal128(38, 0), "numeric(38,0)"),
            (DataType::Decimal128(10, 2), "numeric(10,2)"),
            (DataType::Int16, "smallint"),
            (DataType::Int8, "smallint"),
            (DataType::Float32, "real"),
        ] {
            assert_eq!(pg_type(&t).as_deref(), Some(pg), "{t}");
        }
        assert_eq!(
            pg_type(&DataType::Timestamp(
                TimeUnit::Microsecond,
                Some("UTC".into())
            ))
            .as_deref(),
            Some("timestamp with time zone")
        );
        assert_eq!(pg_type(&DataType::Binary), None);
        assert_eq!(ident("o\"d"), "\"o\"\"d\"");
    }
}
