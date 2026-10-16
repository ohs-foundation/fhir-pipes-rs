//! `export --hapi-db`: read resources straight from a HAPI FHIR JPA server's
//! PostgreSQL database instead of its API, as fhir-data-pipes'
//! `--fhirFetchMode=HAPI_JDBC` does. This ties fhir-pipes to HAPI's internal
//! schema, which HAPI changes between releases; the layout is checked before
//! anything is read, and one fhir-pipes does not know is refused.
//!
//! Each type's resources are `hfj_resource` rows joined to their current
//! `hfj_res_ver` row. HAPI stores the resource without `id` and `meta`: the
//! id comes from `hfj_resource.fhir_id` (HAPI 7 and later) or
//! `hfj_forced_id` (earlier), `meta.versionId` and `meta.lastUpdated` from
//! the row, `meta.source` from `hfj_res_ver.source_uri` and `request_id`,
//! and profiles, security labels and tags from `hfj_res_tag` and
//! `hfj_tag_def`. The text is `res_text_vc` (JSON, which HAPI 8 writes) or
//! the `res_text` large object (JSON, or gzipped JSON as `JSONC`, which
//! earlier releases wrote and which stays in a database HAPI upgraded).
//!
//! Every type is read in one read-only, repeatable-read transaction, through
//! a cursor, FETCH_ROWS rows at a time. The watermark is the greatest
//! `res_updated` read, as the database has it, but no later than the
//! database's clock when the transaction began less WATERMARK_MARGIN_SECS:
//! a row HAPI had not committed then may carry an earlier time, and the next
//! run reads it. An incremental run reads the rows updated after the
//! watermark, merges the live ones into the file by id and removes the
//! deleted ones (a delete sets `res_deleted_at` and `res_updated`). A type
//! whose merged count then differs from the database's (a resource was
//! expunged) is read in full.
//!
//! `res_updated` has no time zone: HAPI writes it in its JVM's zone, which
//! is UTC in the HAPI images, and `meta.lastUpdated` is written as UTC.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};

use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx_core::connection::Connection;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnection, PgRow};

use super::{ExportState, TypeState, incoming_path, ndjson_path, tmp_path, type_state};
use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::page::PageNote;
use crate::report::Report;
use crate::snapshot::instant::WATERMARK_MARGIN_SECS;
use crate::snapshot::merge::{MergeFiles, MergeOptions, merge_file};
use crate::snapshot::same_server;

/// Rows fetched from the cursor at a time.
const FETCH_ROWS: usize = 1000;

/// The tables read, each with the columns that must be there.
const REQUIRED: &[(&str, &[&str])] = &[
    (
        "hfj_resource",
        &[
            "res_id",
            "res_type",
            "res_ver",
            "res_updated",
            "res_deleted_at",
        ],
    ),
    ("hfj_res_ver", &["res_id", "res_ver", "res_encoding"]),
];
const TABLES: &[&str] = &[
    "hfj_resource",
    "hfj_res_ver",
    "hfj_forced_id",
    "hfj_res_tag",
    "hfj_tag_def",
];

/// The state file's server for a database: host, port, database name and
/// the schema HAPI's tables were found in, without the user or password.
/// The schema is the one the search path resolved `hfj_resource` to, so
/// another schema in the same database (through the connection string's
/// `options` or the role's default search path) is another server, and its
/// files are read in full.
fn identity(url: &str, schema: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => format!(
            "hapi-db:{}{}{}/{schema}",
            u.host_str().unwrap_or(""),
            u.port().map(|p| format!(":{p}")).unwrap_or_default(),
            u.path()
        ),
        Err(_) => format!("hapi-db:/{schema}"),
    }
}

/// Where a resource's FHIR id is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdColumn {
    /// `hfj_resource.fhir_id`, set for every resource (HAPI 7 and later).
    FhirId,
    /// `hfj_forced_id.forced_id` for a client-assigned id, else `res_id`.
    ForcedId,
}

/// The `hfj_res_ver.res_text` column, when there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextBlob {
    Absent,
    Bytea,
    /// An `oid`: a large object.
    LargeObject,
}

/// How a large object is read: `lo_get` needs SELECT on each large object,
/// so a read-only role is given SELECT on `pg_largeobject` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LargeObjects {
    Catalog,
    LoGet,
}

/// The parts of HAPI's schema found in the database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    id: IdColumn,
    text_vc: bool,
    text: TextBlob,
    /// `hfj_res_tag` and `hfj_tag_def`, and whether the definitions have
    /// `tag_version` and `tag_user_selected`.
    tags: Option<(bool, bool)>,
    source_uri: bool,
    request_id: bool,
}

/// Table name to its columns and their types.
type Found = BTreeMap<String, BTreeMap<String, String>>;

impl Layout {
    /// The layout `found` describes, or why it is not one fhir-pipes reads.
    fn detect(found: &Found) -> std::result::Result<Layout, String> {
        let has = |t: &str, c: &str| found.get(t).is_some_and(|cols| cols.contains_key(c));
        let missing: Vec<String> = REQUIRED
            .iter()
            .flat_map(|(t, cols)| cols.iter().map(move |c| (*t, *c)))
            .filter(|(t, c)| !has(t, c))
            .map(|(t, c)| format!("{t}.{c}"))
            .collect();
        let id = if has("hfj_resource", "fhir_id") {
            Some(IdColumn::FhirId)
        } else if has("hfj_forced_id", "forced_id") && has("hfj_forced_id", "resource_pid") {
            Some(IdColumn::ForcedId)
        } else {
            None
        };
        let text_vc = has("hfj_res_ver", "res_text_vc");
        let text = match found.get("hfj_res_ver").and_then(|c| c.get("res_text")) {
            None => TextBlob::Absent,
            Some(t) if t == "oid" => TextBlob::LargeObject,
            Some(t) if t == "bytea" => TextBlob::Bytea,
            Some(t) => return Err(format!("hfj_res_ver.res_text is {t}, not oid or bytea")),
        };
        let (Some(id), true, false) =
            (id, missing.is_empty(), !text_vc && text == TextBlob::Absent)
        else {
            let mut what = missing;
            if id.is_none() {
                what.push("hfj_resource.fhir_id or hfj_forced_id".into());
            }
            if !text_vc && text == TextBlob::Absent {
                what.push("hfj_res_ver.res_text_vc or res_text".into());
            }
            let tables: Vec<&str> = found.keys().map(String::as_str).collect();
            return Err(format!(
                "this database does not have the HAPI FHIR JPA schema fhir-pipes reads: \
                 missing {}; found tables: {}",
                what.join(", "),
                if tables.is_empty() {
                    "none of hfj_resource, hfj_res_ver, hfj_forced_id, hfj_res_tag, hfj_tag_def"
                        .to_string()
                } else {
                    tables.join(", ")
                }
            ));
        };
        let tags = (has("hfj_res_tag", "res_id")
            && has("hfj_res_tag", "tag_id")
            && [
                "tag_id",
                "tag_type",
                "tag_system",
                "tag_code",
                "tag_display",
            ]
            .iter()
            .all(|c| has("hfj_tag_def", c)))
        .then(|| {
            (
                has("hfj_tag_def", "tag_version"),
                has("hfj_tag_def", "tag_user_selected"),
            )
        });
        Ok(Layout {
            id,
            text_vc,
            text,
            tags,
            source_uri: has("hfj_res_ver", "source_uri"),
            request_id: has("hfj_res_ver", "request_id"),
        })
    }

    fn describe(&self) -> String {
        let mut parts = vec![match self.id {
            IdColumn::FhirId => "ids from hfj_resource.fhir_id",
            IdColumn::ForcedId => "ids from hfj_forced_id",
        }];
        if self.tags.is_none() {
            parts.push("no tag tables");
        }
        parts.join(", ")
    }

    /// The query for one type's rows: `$1` is the type; with `since`, `$2`
    /// is the watermark and every row updated after it is read, deleted
    /// ones too; without, the rows that are not deleted.
    fn select(&self, lo: LargeObjects, since: bool) -> String {
        let live = "r.res_deleted_at IS NULL";
        let id = match self.id {
            IdColumn::FhirId => "COALESCE(r.fhir_id, r.res_id::text)",
            IdColumn::ForcedId => "COALESCE(f.forced_id, r.res_id::text)",
        };
        let text_vc = if self.text_vc {
            format!("CASE WHEN {live} THEN v.res_text_vc END")
        } else {
            "NULL::text".to_string()
        };
        let text = match (self.text, lo) {
            (TextBlob::Absent, _) => "NULL::bytea".to_string(),
            (TextBlob::Bytea, _) => format!("CASE WHEN {live} THEN v.res_text END"),
            (TextBlob::LargeObject, LargeObjects::LoGet) => {
                format!("CASE WHEN {live} AND v.res_text IS NOT NULL THEN lo_get(v.res_text) END")
            }
            (TextBlob::LargeObject, LargeObjects::Catalog) => format!(
                "CASE WHEN {live} AND v.res_text IS NOT NULL THEN \
                 (SELECT string_agg(lo.data, ''::bytea ORDER BY lo.pageno) \
                 FROM pg_catalog.pg_largeobject lo WHERE lo.loid = v.res_text) END"
            ),
        };
        let column = |present: bool, name: &str| {
            if present {
                format!("v.{name}")
            } else {
                "NULL::text".to_string()
            }
        };
        // In tag_id order, as HAPI returns them (its tag query walks the
        // (res_id, tag_id) index), not in the order they were added.
        let tags = match self.tags {
            None => "NULL::text".to_string(),
            Some((version, user_selected)) => format!(
                "CASE WHEN {live} THEN (SELECT json_agg(json_build_object(\
                 'type', d.tag_type, 'system', d.tag_system, 'version', {}, \
                 'code', d.tag_code, 'display', d.tag_display, 'userSelected', {}) \
                 ORDER BY d.tag_type, t.tag_id)::text \
                 FROM hfj_res_tag t JOIN hfj_tag_def d ON d.tag_id = t.tag_id \
                 WHERE t.res_id = r.res_id) END",
                if version { "d.tag_version" } else { "NULL" },
                if user_selected {
                    "d.tag_user_selected"
                } else {
                    "NULL"
                },
            ),
        };
        let forced = match self.id {
            IdColumn::ForcedId => " LEFT JOIN hfj_forced_id f ON f.resource_pid = r.res_id",
            IdColumn::FhirId => "",
        };
        let filter = if since {
            "r.res_updated > $2::timestamp".to_string()
        } else {
            live.to_string()
        };
        format!(
            "SELECT {id} AS id, r.res_ver, \
             to_char(r.res_updated, 'YYYY-MM-DD\"T\"HH24:MI:SS.US') AS updated, \
             r.res_deleted_at IS NOT NULL AS deleted, v.res_encoding, \
             {text_vc} AS text_vc, {text} AS text, {} AS source_uri, {} AS request_id, \
             {tags} AS tags \
             FROM hfj_resource r \
             LEFT JOIN hfj_res_ver v ON v.res_id = r.res_id AND v.res_ver = r.res_ver{forced} \
             WHERE r.res_type = $1 AND {filter} ORDER BY r.res_id",
            column(self.source_uri, "source_uri"),
            column(self.request_id, "request_id"),
        )
    }
}

/// One row of the query `Layout::select` builds.
#[derive(Debug, Default)]
struct HapiRow {
    id: String,
    version: i64,
    /// `res_updated` as `YYYY-MM-DDTHH:MM:SS.ffffff`, no time zone.
    updated: String,
    deleted: bool,
    encoding: Option<String>,
    text_vc: Option<String>,
    text: Option<Vec<u8>>,
    source_uri: Option<String>,
    request_id: Option<String>,
    /// A JSON array of `Tag`.
    tags: Option<String>,
}

impl HapiRow {
    fn from_pg(r: &PgRow) -> std::result::Result<Self, sqlx_core::error::Error> {
        Ok(HapiRow {
            id: r.try_get("id")?,
            version: r.try_get("res_ver")?,
            updated: r.try_get("updated")?,
            deleted: r.try_get("deleted")?,
            encoding: r.try_get("res_encoding")?,
            text_vc: r.try_get("text_vc")?,
            text: r.try_get("text")?,
            source_uri: r.try_get("source_uri")?,
            request_id: r.try_get("request_id")?,
            tags: r.try_get("tags")?,
        })
    }
}

/// One `hfj_tag_def` row: tag_type 0 is a tag, 1 a profile (the code is
/// its URL), 2 a security label.
#[derive(Debug, Deserialize)]
struct Tag {
    #[serde(rename = "type")]
    kind: i32,
    system: Option<String>,
    version: Option<String>,
    code: Option<String>,
    display: Option<String>,
    #[serde(rename = "userSelected")]
    user_selected: Option<bool>,
}

impl Tag {
    fn coding(&self) -> Value {
        let mut c = Map::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                c.insert(k.into(), Value::String(v.clone()));
            }
        };
        put("system", &self.system);
        put("version", &self.version);
        put("code", &self.code);
        put("display", &self.display);
        if let Some(u) = self.user_selected {
            c.insert("userSelected".into(), Value::Bool(u));
        }
        Value::Object(c)
    }
}

/// `res_updated` as a FHIR instant, to the millisecond as HAPI gives it:
/// `2026-10-16T02:50:04.440000` becomes `2026-10-16T02:50:04.440+00:00`.
fn last_updated(updated: &str) -> String {
    format!("{}+00:00", updated.get(..23).unwrap_or(updated))
}

/// The stored text of a row, by its encoding: `JSON` in `res_text_vc` or
/// (uncompressed) in `res_text`, `JSONC` gzipped in `res_text`.
fn stored_text(row: &HapiRow) -> std::result::Result<String, String> {
    let encoding = row.encoding.as_deref().unwrap_or("");
    let bytes = || {
        row.text
            .as_deref()
            .ok_or_else(|| format!("encoding {encoding} without res_text"))
    };
    match encoding {
        "JSON" => match &row.text_vc {
            Some(t) => Ok(t.clone()),
            None => String::from_utf8(bytes()?.to_vec())
                .map_err(|_| "res_text is not UTF-8".to_string()),
        },
        "JSONC" => {
            let mut text = String::new();
            flate2::read::GzDecoder::new(bytes()?)
                .read_to_string(&mut text)
                .map_err(|e| format!("res_text is not gzipped JSON: {e}"))?;
            Ok(text)
        }
        "" => Err("no current version in hfj_res_ver".to_string()),
        other => Err(format!(
            "encoding {other} (fhir-pipes reads JSON and JSONC)"
        )),
    }
}

/// The resource as the API gives it: `resourceType`, `id` and `meta` first,
/// then the stored elements.
fn resource(resource_type: &str, row: &HapiRow) -> std::result::Result<Value, String> {
    let text = stored_text(row)?;
    let Ok(Value::Object(stored)) = serde_json::from_str::<Value>(&text) else {
        return Err("the stored text is not a JSON object".to_string());
    };
    if stored.get("resourceType").and_then(Value::as_str) != Some(resource_type) {
        return Err(format!("the stored text is not a {resource_type}"));
    }
    let tags: Vec<Tag> = match &row.tags {
        Some(t) => serde_json::from_str(t).map_err(|e| format!("tags: {e}"))?,
        None => Vec::new(),
    };
    let mut out = Map::new();
    out.insert("resourceType".into(), Value::String(resource_type.into()));
    out.insert("id".into(), Value::String(row.id.clone()));
    out.insert("meta".into(), meta(stored.get("meta"), row, &tags));
    for (k, v) in stored {
        if !matches!(k.as_str(), "resourceType" | "id" | "meta") {
            out.insert(k, v);
        }
    }
    Ok(Value::Object(out))
}

/// `meta` in the API's element order. HAPI keeps tags in its tag tables and
/// leaves `meta` out of the stored text; whatever the text does have (with
/// inline tag storage) is kept where the tables have nothing.
fn meta(stored: Option<&Value>, row: &HapiRow, tags: &[Tag]) -> Value {
    let stored = stored
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut m = Map::new();
    for k in ["id", "extension"] {
        if let Some(v) = stored.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    m.insert("versionId".into(), Value::String(row.version.to_string()));
    m.insert(
        "lastUpdated".into(),
        Value::String(last_updated(&row.updated)),
    );
    // As HAPI builds it: the source URI, then `#` and the request id.
    let source = match (&row.source_uri, &row.request_id) {
        (Some(s), Some(r)) if !r.is_empty() => Some(format!("{s}#{r}")),
        (Some(s), _) => Some(s.clone()),
        (None, Some(r)) if !r.is_empty() => Some(format!("#{r}")),
        _ => None,
    };
    match source {
        Some(s) => {
            m.insert("source".into(), Value::String(s));
        }
        None => {
            if let Some(s) = stored.get("source") {
                m.insert("source".into(), s.clone());
            }
        }
    }
    let of = |kind: i32| tags.iter().filter(move |t| t.kind == kind);
    let profiles: Vec<Value> = of(1)
        .filter_map(|t| t.code.clone().map(Value::String))
        .collect();
    let security: Vec<Value> = of(2).map(Tag::coding).collect();
    let tag: Vec<Value> = of(0).map(Tag::coding).collect();
    for (k, from_tables) in [("profile", profiles), ("security", security), ("tag", tag)] {
        if !from_tables.is_empty() {
            m.insert(k.into(), Value::Array(from_tables));
        } else if let Some(v) = stored.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    for (k, v) in stored {
        m.entry(k).or_insert(v);
    }
    Value::Object(m)
}

/// A saved watermark this module wrote.
fn valid_watermark(w: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(w, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
}

fn db_error(what: &str) -> impl FnOnce(sqlx_core::error::Error) -> PipesError + '_ {
    move |e| PipesError::Environment(format!("{what}: {e}"))
}

/// Bring OUT/<TYPE>.ndjson up to date from the database `url` for each of
/// `types`, recording each in the export state's `hapi_db` as it finishes.
pub fn export(url: &str, args: &ExportArgs, types: &[&str], report: &mut Report) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PipesError::Environment(format!("starting the database runtime: {e}")))?;
    rt.block_on(export_all(url, args, types, report))
}

async fn export_all(
    url: &str,
    args: &ExportArgs,
    types: &[&str],
    report: &mut Report,
) -> Result<()> {
    // TLS (sslmode=require and up) uses rustls with the same provider as the
    // FHIR client.
    crate::extract::client::ensure_crypto_provider();
    let mut conn = PgConnection::connect(url)
        .await
        .map_err(db_error("connecting to the HAPI database"))?;
    let layout =
        Layout::detect(&found_tables(&mut conn).await?).map_err(PipesError::Environment)?;
    let lo = large_objects(&mut conn, &layout).await?;
    let schema = schema_of(&mut conn, "hfj_resource").await?;
    println!("HAPI database: schema {schema}, {}", layout.describe());
    let server = identity(url, &schema);
    let mut state = ExportState::read(&args.out);
    if !same_server(&state.server, &server) {
        // Files from a server's API, another database or another schema
        // are not merged into.
        state = ExportState {
            server,
            ..ExportState::default()
        };
    }
    // One snapshot for every type, and nothing written.
    let mut tx = conn
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(db_error("starting a read-only transaction"))?;
    // now() is the transaction's start; res_updated is read as UTC.
    let cap: String = sqlx_core::query::query(AssertSqlSafe(format!(
        "SELECT to_char((now() AT TIME ZONE 'UTC') - interval '{WATERMARK_MARGIN_SECS} seconds', \
         'YYYY-MM-DD\"T\"HH24:MI:SS.US')"
    )))
    .fetch_one(&mut *tx)
    .await
    .and_then(|r| r.try_get(0))
    .map_err(db_error("reading the database's clock"))?;
    let reading = Reading {
        layout: &layout,
        lo,
        args,
        cap,
    };
    for &t in types {
        let out = ndjson_path(&args.out, t);
        let since = state
            .hapi_db
            .get(t)
            .filter(|_| args.incremental && out.exists())
            .and_then(|p| p.watermark.clone())
            .filter(|w| valid_watermark(w));
        let mut done = None;
        if let Some(since) = &since {
            done = read_changes(&mut tx, &reading, t, since, report).await?;
        }
        let done = match done {
            Some(d) => d,
            None => read_all(&mut tx, &reading, t, report).await?,
        };
        state.hapi_db.insert(t.to_string(), done);
        // After each type, so an interrupted run keeps what it finished.
        state.write(&args.out)?;
    }
    tx.rollback()
        .await
        .map_err(db_error("ending the read-only transaction"))?;
    Ok(())
}

/// The columns of the tables in TABLES that exist, through the search path.
async fn found_tables(conn: &mut PgConnection) -> Result<Found> {
    let rows = sqlx_core::query::query(
        "SELECT t.name, a.attname::text, format_type(a.atttypid, a.atttypmod) \
         FROM unnest($1::text[]) AS t(name) \
         JOIN pg_catalog.pg_attribute a ON a.attrelid = to_regclass(t.name) \
         WHERE a.attnum > 0 AND NOT a.attisdropped",
    )
    .bind(TABLES.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error("reading the HAPI schema"))?;
    let mut found = Found::new();
    for r in rows {
        let get = |i| {
            r.try_get::<String, _>(i)
                .map_err(db_error("reading the HAPI schema"))
        };
        found.entry(get(0)?).or_default().insert(get(1)?, get(2)?);
    }
    Ok(found)
}

/// The schema `table` resolves to through the search path, as the queries
/// that name it unqualified see it.
async fn schema_of(conn: &mut PgConnection, table: &str) -> Result<String> {
    sqlx_core::query::query(
        "SELECT n.nspname::text FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.oid = to_regclass($1)",
    )
    .bind(table)
    .fetch_one(&mut *conn)
    .await
    .and_then(|r| r.try_get(0))
    .map_err(db_error(&format!("finding the schema of {table}")))
}

async fn large_objects(conn: &mut PgConnection, layout: &Layout) -> Result<LargeObjects> {
    if layout.text != TextBlob::LargeObject {
        return Ok(LargeObjects::LoGet);
    }
    let catalog: bool = sqlx_core::query::query(
        "SELECT has_table_privilege('pg_catalog.pg_largeobject', 'SELECT')",
    )
    .fetch_one(&mut *conn)
    .await
    .and_then(|r| r.try_get(0))
    .map_err(db_error("checking access to large objects"))?;
    Ok(if catalog {
        LargeObjects::Catalog
    } else {
        LargeObjects::LoGet
    })
}

/// What every type's read shares within a run.
struct Reading<'a> {
    layout: &'a Layout,
    lo: LargeObjects,
    args: &'a ExportArgs,
    /// The database's clock when the transaction began, less the margin:
    /// the latest a watermark may be.
    cap: String,
}

/// Run `sql` (see `Layout::select`) through a cursor and hand each row to
/// `each`.
async fn read_rows(
    conn: &mut PgConnection,
    sql: &str,
    resource_type: &str,
    since: Option<&str>,
    mut each: impl FnMut(HapiRow) -> Result<()>,
) -> Result<()> {
    let what = format!("reading {resource_type} from the HAPI database");
    let mut declare = sqlx_core::query::query(AssertSqlSafe(format!(
        "DECLARE fhir_pipes_rows NO SCROLL CURSOR FOR {sql}"
    )))
    .bind(resource_type);
    if let Some(s) = since {
        declare = declare.bind(s);
    }
    declare.execute(&mut *conn).await.map_err(db_error(&what))?;
    let fetch = format!("FETCH {FETCH_ROWS} FROM fhir_pipes_rows");
    loop {
        let rows = sqlx_core::query::query(AssertSqlSafe(fetch.clone()))
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| {
                let mut message = format!("{what}: {e}");
                if message.contains("permission denied for large object") {
                    message.push_str(
                        "; resources HAPI stored as large objects need SELECT on \
                         pg_catalog.pg_largeobject for this role (see the README)",
                    );
                }
                PipesError::Environment(message)
            })?;
        if rows.is_empty() {
            break;
        }
        for r in &rows {
            each(HapiRow::from_pg(r).map_err(db_error(&what))?)?;
        }
    }
    sqlx_core::query::query("CLOSE fhir_pipes_rows")
        .execute(&mut *conn)
        .await
        .map_err(db_error(&what))?;
    Ok(())
}

/// The resource line for a live row, or `None` after reporting why it
/// cannot be read.
fn line(resource_type: &str, row: &HapiRow, report: &mut Report) -> Result<Option<String>> {
    match resource(resource_type, row) {
        Ok(r) => Ok(Some(serde_json::to_string(&r)?)),
        Err(why) => {
            report.add(
                "hapi_db_row_skipped",
                &format!("{resource_type}/{}", row.id),
                &format!("version {}: {why}; left out", row.version),
            );
            Ok(None)
        }
    }
}

/// Read every live resource of `resource_type` into OUT/<TYPE>.ndjson,
/// written beside it and renamed into place.
async fn read_all(
    conn: &mut PgConnection,
    reading: &Reading<'_>,
    resource_type: &str,
    report: &mut Report,
) -> Result<TypeState> {
    let Reading {
        layout,
        lo,
        args,
        cap,
    } = reading;
    let out = ndjson_path(&args.out, resource_type);
    let tmp = tmp_path(&args.out, resource_type);
    let mut total = 0usize;
    let mut watermark: Option<String> = None;
    let result = async {
        let file = crate::fs::AtomicFile::create_at(&out, tmp.clone())?;
        let mut w = std::io::BufWriter::new(file);
        read_rows(
            conn,
            &layout.select(*lo, false),
            resource_type,
            None,
            |row| {
                if watermark
                    .as_deref()
                    .is_none_or(|m| row.updated.as_str() > m)
                {
                    watermark = Some(row.updated.clone());
                }
                if let Some(l) = line(resource_type, &row, report)? {
                    writeln!(w, "{l}").map_err(|e| PipesError::io(&tmp, e))?;
                    total += 1;
                }
                Ok(())
            },
        )
        .await?;
        w.into_inner()
            .map_err(|e| PipesError::io(&tmp, e.into_error()))?
            .commit()
    }
    .await;
    result?;
    println!(
        "{resource_type}: {total} resources from the database -> {}",
        out.display()
    );
    // With no rows, everything committed before the cap was read.
    Ok(type_state(Some(capped(watermark.as_deref(), cap)), total))
}

/// The watermark: the greatest `res_updated` seen, but no later than `cap`.
/// Both are in the same fixed-width form, so they compare as text.
fn capped(seen: Option<&str>, cap: &str) -> String {
    match seen {
        Some(s) if s < cap => s.to_string(),
        _ => cap.to_string(),
    }
}

/// Read what changed since `since`, merge it into OUT/<TYPE>.ndjson by id
/// and remove what was deleted. `None` when the merged file's count differs
/// from the database's, so the caller reads the type in full instead.
async fn read_changes(
    conn: &mut PgConnection,
    reading: &Reading<'_>,
    resource_type: &str,
    since: &str,
    report: &mut Report,
) -> Result<Option<TypeState>> {
    let Reading {
        layout,
        lo,
        args,
        cap,
    } = reading;
    let out = ndjson_path(&args.out, resource_type);
    let tmp = tmp_path(&args.out, resource_type);
    let incoming = incoming_path(&args.out, resource_type);
    let mut notes: Vec<PageNote> = Vec::new();
    let mut deleted: HashSet<String> = HashSet::new();
    let mut watermark = since.to_string();
    let result = async {
        let file = std::fs::File::create(&incoming).map_err(|e| PipesError::io(&incoming, e))?;
        let mut w = std::io::BufWriter::new(file);
        read_rows(
            conn,
            &layout.select(*lo, true),
            resource_type,
            Some(since),
            |row| {
                if row.updated > watermark {
                    watermark = row.updated.clone();
                }
                if row.deleted {
                    deleted.insert(row.id);
                    return Ok(());
                }
                if let Some(l) = line(resource_type, &row, report)? {
                    writeln!(w, "{l}").map_err(|e| PipesError::io(&incoming, e))?;
                    notes.push(PageNote {
                        last_updated: Some(last_updated(&row.updated)),
                        id: row.id,
                        boundary_url: None,
                    });
                }
                Ok(())
            },
        )
        .await?;
        w.flush().map_err(|e| PipesError::io(&incoming, e))
    }
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&incoming);
        return Err(e);
    }
    // A resource deleted and created again is live: its row says so, and
    // the merge keeps an id it reads.
    let gone: HashSet<&str> = deleted.iter().map(String::as_str).collect();
    let stats = merge_file(
        &MergeFiles {
            existing: out.clone(),
            tmp: tmp.clone(),
            incoming,
            unparsed_kind: "export_line_unparsed",
        },
        &notes,
        &MergeOptions {
            drop: Some(&gone),
            ..MergeOptions::default()
        },
        report,
    )?;
    let (total, removed) = (stats.total, stats.removed);
    let live: i64 = sqlx_core::query::query(
        "SELECT count(*) FROM hfj_resource WHERE res_type = $1 AND res_deleted_at IS NULL",
    )
    .bind(resource_type)
    .fetch_one(&mut *conn)
    .await
    .and_then(|r| r.try_get(0))
    .map_err(db_error(&format!(
        "counting {resource_type} in the HAPI database"
    )))?;
    if live as usize != total {
        println!(
            "{resource_type}: the database has {live} resources and the merged file {total}; \
             reading in full (a resource was expunged, or the file had drifted)"
        );
        report.add(
            "incremental_count_mismatch",
            resource_type,
            &format!(
                "the database has {live} {resource_type} resources but the merged file has {total}; read in full"
            ),
        );
        return Ok(None);
    }
    println!(
        // A capped watermark means rows updated after the cap are read
        // again by the next run.
        "{resource_type}: {} new, {} updated or read again, {removed} deleted since {since}, {total} in all -> {}",
        stats.added,
        stats.updated,
        out.display()
    );
    Ok(Some(type_state(Some(capped(Some(&watermark), cap)), total)))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;

    fn found(tables: &[(&str, &[(&str, &str)])]) -> Found {
        tables
            .iter()
            .map(|(t, cols)| {
                (
                    t.to_string(),
                    cols.iter()
                        .map(|(c, ty)| (c.to_string(), ty.to_string()))
                        .collect(),
                )
            })
            .collect()
    }

    const RESOURCE: &[(&str, &str)] = &[
        ("res_id", "bigint"),
        ("res_type", "character varying(40)"),
        ("res_ver", "bigint"),
        ("res_updated", "timestamp(6) without time zone"),
        ("res_deleted_at", "timestamp(6) without time zone"),
    ];
    const VERSION: &[(&str, &str)] = &[
        ("res_id", "bigint"),
        ("res_ver", "bigint"),
        ("res_encoding", "character varying(5)"),
        ("res_text", "oid"),
        ("res_text_vc", "text"),
        ("source_uri", "character varying(768)"),
        ("request_id", "character varying(16)"),
    ];
    const TAG: &[(&str, &str)] = &[("res_id", "bigint"), ("tag_id", "bigint")];
    const TAG_DEF: &[(&str, &str)] = &[
        ("tag_id", "bigint"),
        ("tag_type", "integer"),
        ("tag_system", "character varying(200)"),
        ("tag_code", "character varying(200)"),
        ("tag_display", "character varying(200)"),
        ("tag_version", "character varying(30)"),
        ("tag_user_selected", "boolean"),
    ];

    fn hapi8() -> Found {
        let mut resource = RESOURCE.to_vec();
        resource.push(("fhir_id", "character varying(64)"));
        found(&[
            ("hfj_resource", &resource),
            ("hfj_res_ver", VERSION),
            ("hfj_res_tag", TAG),
            ("hfj_tag_def", TAG_DEF),
            (
                "hfj_forced_id",
                &[("forced_id", "text"), ("resource_pid", "bigint")],
            ),
        ])
    }

    #[test]
    fn hapi_8_reads_ids_from_fhir_id() {
        let l = Layout::detect(&hapi8()).unwrap();
        assert_eq!(l.id, IdColumn::FhirId);
        assert_eq!(l.text, TextBlob::LargeObject);
        assert!(l.text_vc && l.source_uri && l.request_id);
        assert_eq!(l.tags, Some((true, true)));
        let sql = l.select(LargeObjects::LoGet, false);
        assert!(sql.contains("COALESCE(r.fhir_id, r.res_id::text) AS id"));
        assert!(sql.contains("lo_get(v.res_text)"));
        assert!(!sql.contains("hfj_forced_id"));
        assert!(
            sql.ends_with("WHERE r.res_type = $1 AND r.res_deleted_at IS NULL ORDER BY r.res_id")
        );
        let sql = l.select(LargeObjects::Catalog, true);
        assert!(sql.contains("FROM pg_catalog.pg_largeobject lo WHERE lo.loid = v.res_text"));
        assert!(
            sql.contains(
                "WHERE r.res_type = $1 AND r.res_updated > $2::timestamp ORDER BY r.res_id"
            )
        );
    }

    #[test]
    fn older_hapi_reads_client_ids_from_hfj_forced_id() {
        let l = Layout::detect(&found(&[
            ("hfj_resource", RESOURCE),
            (
                "hfj_res_ver",
                &[
                    ("res_id", "bigint"),
                    ("res_ver", "bigint"),
                    ("res_encoding", "character varying(5)"),
                    ("res_text", "bytea"),
                ],
            ),
            (
                "hfj_forced_id",
                &[("forced_id", "text"), ("resource_pid", "bigint")],
            ),
            ("hfj_res_tag", TAG),
            ("hfj_tag_def", &TAG_DEF[..5]),
        ]))
        .unwrap();
        assert_eq!(l.id, IdColumn::ForcedId);
        assert_eq!(l.text, TextBlob::Bytea);
        assert!(!l.text_vc && !l.source_uri);
        assert_eq!(l.tags, Some((false, false)));
        let sql = l.select(LargeObjects::LoGet, false);
        assert!(sql.contains("COALESCE(f.forced_id, r.res_id::text) AS id"));
        assert!(sql.contains("LEFT JOIN hfj_forced_id f ON f.resource_pid = r.res_id"));
        assert!(sql.contains("NULL::text AS text_vc"));
        assert!(sql.contains("'version', NULL"));
    }

    #[test]
    fn an_unknown_schema_is_refused_naming_what_was_found() {
        let err = Layout::detect(&found(&[("hfj_resource", RESOURCE)])).unwrap_err();
        assert!(err.contains("hfj_res_ver.res_id"), "{err}");
        assert!(
            err.contains("hfj_resource.fhir_id or hfj_forced_id"),
            "{err}"
        );
        assert!(err.contains("found tables: hfj_resource"), "{err}");
        let err = Layout::detect(&Found::new()).unwrap_err();
        assert!(err.contains("found tables: none of hfj_resource"), "{err}");
    }

    fn row(encoding: &str) -> HapiRow {
        HapiRow {
            id: "abc".into(),
            version: 3,
            updated: "2026-10-16T02:50:04.440000".into(),
            encoding: Some(encoding.into()),
            ..HapiRow::default()
        }
    }

    #[test]
    fn json_and_gzipped_json_are_read() {
        let text = r#"{"resourceType":"Patient","gender":"female"}"#;
        let mut r = row("JSON");
        r.text_vc = Some(text.into());
        assert_eq!(stored_text(&r).unwrap(), text);
        r.text_vc = None;
        r.text = Some(text.as_bytes().to_vec());
        assert_eq!(stored_text(&r).unwrap(), text);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(text.as_bytes()).unwrap();
        let mut r = row("JSONC");
        r.text = Some(gz.finish().unwrap());
        assert_eq!(stored_text(&r).unwrap(), text);
        r.text = Some(text.as_bytes().to_vec());
        assert!(stored_text(&r).unwrap_err().contains("not gzipped"));

        assert!(
            stored_text(&row("ESR"))
                .unwrap_err()
                .contains("encoding ESR")
        );
        let mut r = row("JSON");
        r.encoding = None;
        assert!(stored_text(&r).unwrap_err().contains("no current version"));
    }

    #[test]
    fn id_and_meta_are_rebuilt_from_the_row_and_tag_tables() {
        let mut r = row("JSON");
        r.text_vc = Some(
            r#"{"resourceType":"Patient","name":[{"family":"Abc"}],"gender":"female"}"#.into(),
        );
        r.tags = Some(
            json!([
                {"type": 0, "system": "http://example.org/tags", "version": null, "code": "t1", "display": "Tag one", "userSelected": null},
                {"type": 1, "system": "https://github.com/hapifhir/hapi-fhir/ns/jpa/profile", "version": null, "code": "http://example.org/P", "display": null, "userSelected": null},
                {"type": 2, "system": "http://terminology.hl7.org/CodeSystem/v3-Confidentiality", "version": null, "code": "R", "display": null, "userSelected": null},
            ])
            .to_string(),
        );
        let got = resource("Patient", &r).unwrap();
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            json!({
                "resourceType": "Patient",
                "id": "abc",
                "meta": {
                    "versionId": "3",
                    "lastUpdated": "2026-10-16T02:50:04.440+00:00",
                    "profile": ["http://example.org/P"],
                    "security": [{"system": "http://terminology.hl7.org/CodeSystem/v3-Confidentiality", "code": "R"}],
                    "tag": [{"system": "http://example.org/tags", "code": "t1", "display": "Tag one"}]
                },
                "name": [{"family": "Abc"}],
                "gender": "female"
            })
            .to_string()
        );
        assert!(
            resource("Observation", &r)
                .unwrap_err()
                .contains("not a Observation")
        );
    }

    #[test]
    fn stored_meta_is_kept_where_the_tables_have_nothing() {
        let mut r = row("JSON");
        r.text_vc = Some(
            r#"{"resourceType":"Patient","id":"old","meta":{"versionId":"1","source":"s","tag":[{"code":"x"}]},"active":true}"#
                .into(),
        );
        r.request_id = Some("req1".into());
        let got = resource("Patient", &r).unwrap();
        assert_eq!(
            got["meta"],
            json!({"versionId": "3", "lastUpdated": "2026-10-16T02:50:04.440+00:00", "source": "#req1", "tag": [{"code": "x"}]})
        );
        assert_eq!(got["id"], "abc");
        r.source_uri = Some("http://src".into());
        assert_eq!(
            resource("Patient", &r).unwrap()["meta"]["source"],
            "http://src#req1"
        );
    }

    #[test]
    fn identity_leaves_out_the_credentials() {
        assert_eq!(
            identity(
                "postgres://ro:secret@db.example.org:5432/hapi?sslmode=require",
                "public"
            ),
            "hapi-db:db.example.org:5432/hapi/public"
        );
        assert_eq!(
            identity("postgres://db/hapi", "fhir"),
            "hapi-db:db/hapi/fhir"
        );
        assert!(valid_watermark("2026-10-16T02:50:04.440000"));
        let cap = "2026-10-16T02:45:00.000000";
        assert_eq!(
            capped(Some("2026-10-16T02:40:00.123000"), cap),
            "2026-10-16T02:40:00.123000"
        );
        assert_eq!(capped(Some("2026-10-16T02:50:04.440000"), cap), cap);
        assert_eq!(capped(None, cap), cap);
        assert!(!valid_watermark("2026-10-16T02:50:04Z"));
    }
}
