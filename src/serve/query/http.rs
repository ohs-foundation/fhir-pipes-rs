//! The `/fhir` routes: `$sql-run` over the approved SQLQueries, and the
//! CapabilityStatement, OperationDefinition and Library list that describe
//! it. Every error is an OperationOutcome; duckdb's own error text goes to
//! serve's log only.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use serde_json::{Value, json};

use super::catalog::{Lookup, Queries};
use super::config::Approve;
use super::run::{Format, Output, QueryError, Runner};
use crate::library::Library;
use crate::serve::metrics::Metrics;
use crate::serve::output::StdoutQueue;
use crate::warehouse::config::Settings;

/// The base of the canonical URLs of our CapabilityStatement and
/// OperationDefinition.
pub const CANONICAL_BASE: &str = "https://fhir-pipes.local/fhir";
const OPERATION_ID: &str = "fhir-pipes-sql-run";
/// The specification's operation, which ours is a subset of.
const SQL_RUN_BASE: &str = "http://hl7.org/fhir/uv/sql-on-fhir/OperationDefinition/SQLRun";
const FHIR_JSON: &str = "application/fhir+json";
const MAX_BODY: usize = 64 * 1024;
/// Seconds a caller is told to wait after a 503.
const RETRY_AFTER: &str = "5";
/// The parameters `$sql-run` takes, with the value type each takes in a
/// Parameters body.
const PARAMETERS: [(&str, &str); 3] = [
    ("subjectCanonical", "valueCanonical"),
    ("_format", "valueCode"),
    ("_limit", "valueInteger"),
];

/// What the `/fhir` routes share.
pub struct QueryApi {
    /// Swapped whole by `reload`; a query holds the one it started with.
    pub queries: RwLock<Arc<Queries>>,
    pub runner: Runner,
    /// What `reload` builds the catalog from.
    pub settings: Settings,
    pub approve: Approve,
    pub metrics: Arc<Metrics>,
    pub stdout: Arc<StdoutQueue>,
    /// Hides serve's secrets in what is logged.
    pub redactor: Arc<crate::serve::secrets::Redactor>,
}

impl QueryApi {
    /// Build the catalog again; on an error the last one is kept.
    pub fn reload(&self) -> anyhow::Result<()> {
        let queries = Queries::load(&self.settings, &self.approve)?;
        *self.queries.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(queries);
        Ok(())
    }

    fn queries(&self) -> Arc<Queries> {
        self.queries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub fn routes(api: Arc<QueryApi>) -> Router {
    // The CapabilityStatement's date: when this serve started.
    let started = chrono::Utc::now().format("%Y-%m-%d").to_string();
    // Paths and methods under /fhir that are not ours answer with an
    // OperationOutcome. These are routes, not a router fallback, so that
    // these routes merge into serve's router, which has its own fallback.
    Router::new()
        .route(
            "/fhir/$sql-run",
            get(run_get)
                .post(run_post)
                .layer(DefaultBodyLimit::max(MAX_BODY))
                .fallback(method_not_allowed),
        )
        .route(
            "/fhir/metadata",
            get(move || metadata(started.clone())).fallback(method_not_allowed),
        )
        .route(
            &format!("/fhir/OperationDefinition/{OPERATION_ID}"),
            get(operation_definition).fallback(method_not_allowed),
        )
        .route("/fhir/Library", get(libraries).fallback(method_not_allowed))
        .route("/fhir", any(not_found))
        .route("/fhir/", any(not_found))
        .route("/fhir/{*rest}", any(not_found))
        .with_state(api)
}

async fn not_found() -> Response {
    refusal(
        StatusCode::NOT_FOUND,
        "not-found",
        "no such path; this server has $sql-run, metadata, OperationDefinition and Library",
    )
    .response()
}

async fn method_not_allowed() -> Response {
    refusal(
        StatusCode::METHOD_NOT_ALLOWED,
        "not-supported",
        "this path does not take that method",
    )
    .response()
}

/// A request refused or a query that failed: the status, the
/// OperationOutcome issue code and text for the caller, and anything more
/// for serve's log.
struct Refusal {
    status: StatusCode,
    code: &'static str,
    message: String,
    log: Option<String>,
}

fn refusal(status: StatusCode, code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        message: message.into(),
        log: None,
    }
}

fn bad_request(message: impl Into<String>) -> Refusal {
    refusal(StatusCode::BAD_REQUEST, "invalid", message)
}

impl Refusal {
    fn response(&self) -> Response {
        let mut res = fhir(
            self.status,
            json!({
                "resourceType": "OperationOutcome",
                "issue": [{"severity": "error", "code": self.code, "diagnostics": self.message}],
            }),
        );
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from_static(RETRY_AFTER),
            );
        }
        res
    }
}

fn fhir(status: StatusCode, v: Value) -> Response {
    (status, [(header::CONTENT_TYPE, FHIR_JSON)], v.to_string()).into_response()
}

/// A `$sql-run` request's parameters, checked.
struct SqlRun {
    canonical: String,
    format: Format,
    limit: Option<usize>,
}

fn format_code(f: Format) -> &'static str {
    match f {
        Format::Ndjson => "ndjson",
        Format::Json => "json",
        Format::Csv => "csv",
    }
}

/// The format `Accept` asks for: of the media types we write, the one with
/// the highest `q` above 0, the first named on a tie. With none, NDJSON,
/// the specification's default.
fn accepted(headers: &HeaderMap) -> Format {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mut best: Option<(f32, Format)> = None;
    for item in accept.split(',') {
        let mut parts = item.split(';');
        let format = match parts
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "application/json" => Format::Json,
            "text/csv" => Format::Csv,
            "application/x-ndjson" => Format::Ndjson,
            _ => continue,
        };
        // A q that does not parse makes the type unacceptable.
        let q = parts
            .filter_map(|p| {
                let (name, value) = p.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("q")
                    .then(|| value.trim().parse::<f32>().unwrap_or(0.0))
            })
            .next_back()
            .unwrap_or(1.0);
        if q > 0.0 && best.is_none_or(|(b, _)| q > b) {
            best = Some((q, format));
        }
    }
    best.map_or(Format::Ndjson, |(_, f)| f)
}

/// Check the (name, value) pairs of a request. `_format` wins over
/// `Accept`.
fn sql_run(pairs: Vec<(String, String)>, headers: &HeaderMap) -> Result<SqlRun, Refusal> {
    let known = |n: &str| PARAMETERS.iter().any(|(p, _)| *p == n);
    let mut unknown: Vec<&str> = Vec::new();
    for (n, _) in &pairs {
        if !known(n) && !unknown.contains(&n.as_str()) {
            unknown.push(n);
        }
    }
    if !unknown.is_empty() {
        return Err(bad_request(format!(
            "unsupported parameter(s): {}; this server takes subjectCanonical, _format and _limit",
            unknown.join(", ")
        )));
    }
    let one = |name: &str| -> Result<Option<&str>, Refusal> {
        let mut given = pairs.iter().filter(|(n, _)| n == name);
        let first = given.next().map(|(_, v)| v.as_str());
        if given.next().is_some() {
            return Err(bad_request(format!("{name} is given more than once")));
        }
        Ok(first)
    };
    let canonical = match one("subjectCanonical")? {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return Err(bad_request("subjectCanonical is required")),
    };
    let format = match one("_format")? {
        None => accepted(headers),
        Some("ndjson") => Format::Ndjson,
        Some("json") => Format::Json,
        Some("csv") => Format::Csv,
        Some(other) => {
            return Err(bad_request(format!(
                "_format {other:?}: use ndjson, json or csv"
            )));
        }
    };
    let limit = match one("_limit")? {
        None => None,
        Some(l) => match l.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => return Err(bad_request(format!("_limit {l:?}: use a positive integer"))),
        },
    };
    Ok(SqlRun {
        canonical,
        format,
        limit,
    })
}

async fn run_get(
    State(api): State<Arc<QueryApi>>,
    method: Method,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    // axum sends HEAD to the GET handler, which would run the query.
    if method == Method::HEAD {
        return method_not_allowed().await;
    }
    let pairs = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect();
    answer(api, sql_run(pairs, &headers)).await
}

async fn run_post(
    State(api): State<Arc<QueryApi>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let request = posted(query, &headers, body).and_then(|pairs| sql_run(pairs, &headers));
    answer(api, request).await
}

/// The (name, value) pairs of a POST's Parameters body.
fn posted(
    query: Option<String>,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Vec<(String, String)>, Refusal> {
    if query.is_some_and(|q| !q.is_empty()) {
        return Err(bad_request(
            "a POST takes its parameters in a Parameters body, not the query string",
        ));
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if media != FHIR_JSON && media != "application/json" {
        return Err(refusal(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not-supported",
            "the body must be a Parameters resource, as application/fhir+json",
        ));
    }
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            refusal(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too-long",
                format!("the body is over {} KB", MAX_BODY / 1024),
            )
        } else {
            bad_request(format!("reading the body: {e}"))
        }
    })?;
    let v = serde_json::from_slice::<Strict>(&body)
        .map_err(|e| bad_request(format!("the body is not JSON: {e}")))?
        .0;
    if v["resourceType"] != "Parameters" {
        return Err(bad_request("the body must be a Parameters resource"));
    }
    let mut pairs = Vec::new();
    for p in v["parameter"].as_array().into_iter().flatten() {
        let Some(name) = p["name"].as_str() else {
            return Err(bad_request("a parameter has no name"));
        };
        let value = match PARAMETERS.iter().find(|(n, _)| *n == name) {
            // Refused by name in `sql_run`.
            None => String::new(),
            Some((_, key)) => match &p[*key] {
                Value::String(s) if *key != "valueInteger" => s.clone(),
                Value::Number(n) if *key == "valueInteger" => n.to_string(),
                _ => return Err(bad_request(format!("{name} takes {key}"))),
            },
        };
        pairs.push((name.to_string(), value));
    }
    Ok(pairs)
}

/// A JSON value with no object member named twice: `Value` keeps the last
/// of two, which would hide the first from the checks.
struct Strict(Value);

impl<'de> serde::Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Strict, D::Error> {
        d.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(v)) = seq.next_element()? {
            items.push(v);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut members = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let Strict(v) = map.next_value()?;
            if members.insert(key.clone(), v).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "member {key:?} is given more than once"
                )));
            }
        }
        Ok(Value::Object(members))
    }
}

/// Run a checked request, or refuse it; log it and count it either way.
async fn answer(api: Arc<QueryApi>, request: Result<SqlRun, Refusal>) -> Response {
    let started = Instant::now();
    let (canonical, format) = match &request {
        Ok(r) => (one_line(&r.canonical), format_code(r.format)),
        Err(_) => ("-".to_string(), "-"),
    };
    let result = match request {
        Ok(r) => run(api.clone(), r).await,
        Err(e) => Err(e),
    };
    let seconds = started.elapsed().as_secs_f64();
    let (response, status, rows, tail) = match result {
        Ok((out, format)) => {
            let mut res = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, format.media_type())],
                out.body,
            )
                .into_response();
            let mut tail = String::new();
            if let Some(n) = out.truncated_at {
                if let Ok(v) = header::HeaderValue::from_str(&format!("max_rows={n}")) {
                    res.headers_mut().insert("x-fhir-pipes-truncated", v);
                }
                tail = format!(" truncated at {n}");
            }
            (res, StatusCode::OK, out.rows, tail)
        }
        Err(r) => {
            let tail = format!(": {}", one_line(r.log.as_deref().unwrap_or(&r.message)));
            (r.response(), r.status, 0, tail)
        }
    };
    api.metrics.query_ended(status.as_u16(), seconds);
    // The canonical and a failed query's detail come from the caller or
    // duckdb, so a secret can be in them.
    api.stdout.push(api.redactor.redact(&format!(
        "query {canonical} {format} {} {rows} rows {} ms{tail}",
        status.as_u16(),
        (seconds * 1000.0).round() as u64
    )));
    response
}

/// `s` on one line, for the log: a control character becomes a space.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Look the subject up in the current catalog and run it, off the async
/// threads: the runner waits on duckdb.
async fn run(api: Arc<QueryApi>, r: SqlRun) -> Result<(Output, Format), Refusal> {
    let queries = api.queries();
    let ran = tokio::task::spawn_blocking(move || match queries.lookup(&r.canonical) {
        Lookup::Run(lib) => api
            .runner
            .run(&queries.catalog, lib, r.format, r.limit)
            .map(|out| (out, r.format))
            .map_err(query_refusal),
        Lookup::NotApproved => Err(refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!("{} is not an approved SQLQuery", r.canonical),
        )),
        Lookup::NotFound => Err(refusal(
            StatusCode::NOT_FOUND,
            "not-found",
            format!("{} is not in the catalog", r.canonical),
        )),
    })
    .await;
    ran.unwrap_or_else(|e| {
        Err(Refusal {
            log: Some(format!("the query's task failed: {e}")),
            ..internal()
        })
    })
}

fn internal() -> Refusal {
    refusal(
        StatusCode::INTERNAL_SERVER_ERROR,
        "exception",
        "internal error; the serve log has the details",
    )
}

fn query_refusal(e: QueryError) -> Refusal {
    match e {
        QueryError::Sql { message, detail } => Refusal {
            log: Some(detail),
            ..refusal(StatusCode::UNPROCESSABLE_ENTITY, "processing", message)
        },
        QueryError::Unavailable { message, detail } => Refusal {
            log: Some(detail),
            ..refusal(StatusCode::SERVICE_UNAVAILABLE, "transient", message)
        },
        QueryError::Busy => refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "transient",
            "too many queries running; retry later",
        ),
        QueryError::Internal(m) => Refusal {
            log: Some(m),
            ..internal()
        },
    }
}

fn operation_url() -> String {
    format!("{CANONICAL_BASE}/OperationDefinition/{OPERATION_ID}")
}

/// The CapabilityStatement; `date` is the day serve started (UTC).
async fn metadata(date: String) -> Response {
    fhir(
        StatusCode::OK,
        json!({
            "resourceType": "CapabilityStatement",
            "status": "active",
            "date": date,
            "kind": "instance",
            "software": {"name": "fhir-pipes", "version": env!("CARGO_PKG_VERSION")},
            "implementation": {"description": "fhir-pipes serve"},
            "fhirVersion": "4.0.1",
            "format": ["json"],
            "rest": [{
                "mode": "server",
                "resource": [{"type": "Library", "interaction": [{"code": "search-type"}]}],
                "operation": [{"name": "sql-run", "definition": operation_url()}],
            }],
        }),
    )
}

async fn operation_definition() -> Response {
    let param = |name: &str, used: &str, min: u32, ty: &str, doc: &str| json!({"name": name, "use": used, "min": min, "max": "1", "type": ty, "documentation": doc});
    fhir(
        StatusCode::OK,
        json!({
            "resourceType": "OperationDefinition",
            "id": OPERATION_ID,
            "url": operation_url(),
            "name": "FhirPipesSqlRun",
            "title": "Run an approved SQLQuery",
            "status": "active",
            "kind": "operation",
            "description": "A subset of the SQL on FHIR $sql-run: runs an approved SQLQuery \
                            Library, named by its canonical, over the warehouse.",
            "base": SQL_RUN_BASE,
            "code": "sql-run",
            "system": true,
            "type": false,
            "instance": false,
            "parameter": [
                param("subjectCanonical", "in", 1, "canonical",
                      "The SQLQuery Library to run: url or url|version."),
                param("_format", "in", 0, "code",
                      "ndjson (the default), json or csv; takes precedence over Accept."),
                param("_limit", "in", 0, "integer",
                      "The most rows to return. The server may return fewer (max_rows)."),
                param("return", "out", 1, "Binary", "The rows, in the format asked for."),
            ],
        }),
    )
}

/// The approved SQLQueries. No search parameter is supported, so any is
/// refused rather than ignored.
async fn libraries(State(api): State<Arc<QueryApi>>, RawQuery(query): RawQuery) -> Response {
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        let names: Vec<String> = url::form_urlencoded::parse(q.as_bytes())
            .map(|(n, _)| n.into_owned())
            .collect();
        return bad_request(format!(
            "unsupported search parameter(s): {}; /fhir/Library lists every approved \
             SQLQuery and takes none",
            names.join(", ")
        ))
        .response();
    }
    let queries = api.queries();
    let approved = queries.approved();
    let entries: Vec<Value> = approved
        .iter()
        .zip(bundle_ids(&approved))
        .map(|(l, id)| {
            // A copy: the catalog's Library keeps its own id.
            let mut resource = l.json.clone();
            resource["id"] = json!(id);
            json!({
                "fullUrl": format!("{CANONICAL_BASE}/Library/{id}"),
                "resource": resource,
                "search": {"mode": "match"},
            })
        })
        .collect();
    fhir(
        StatusCode::OK,
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "total": entries.len(),
            "entry": entries,
        }),
    )
}

/// An id for each Library in the bundle, unique in it: the Library's own
/// id when no other in the bundle has the same one, else one made from the
/// name and version, with `-<n>` added while it is taken. The bundled
/// resource carries this id and its `fullUrl` ends with it, so they agree.
fn bundle_ids(libs: &[&Library]) -> Vec<String> {
    let own: Vec<Option<&str>> = libs.iter().map(|l| l.json["id"].as_str()).collect();
    let mut ids: Vec<Option<String>> = own
        .iter()
        .map(|id| {
            id.filter(|id| own.iter().filter(|o| **o == Some(*id)).count() == 1)
                .map(str::to_string)
        })
        .collect();
    // The ids kept are taken before any is made.
    let mut taken: Vec<String> = ids.iter().flatten().cloned().collect();
    for (l, id) in libs.iter().zip(ids.iter_mut()) {
        if id.is_some() {
            continue;
        }
        let key: String = match &l.version {
            Some(v) => format!("{}-{v}", l.name),
            None => l.name.clone(),
        }
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
        let mut made = key.clone();
        let mut n = 2;
        while taken.contains(&made) {
            made = format!("{key}-{n}");
            n += 1;
        }
        taken.push(made.clone());
        *id = Some(made);
    }
    ids.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::query::testing::{Warehouse, have_duckdb, lines, rows};
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use serde_json::{Value, json};
    use std::io::Write;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const Q: &str = "https://example.org/Library/Q";

    /// serve's stdout, kept.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl Write for Log {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Log {
        /// The first line holding `needle`, waited for: the queue writes
        /// on a thread of its own.
        fn line(&self, needle: &str) -> String {
            for _ in 0..500 {
                let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
                if let Some(l) = text.lines().find(|l| l.contains(needle)) {
                    return l.to_string();
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("no line with {needle:?} in {:?}", self.0.lock().unwrap());
        }
    }

    fn api(w: &Warehouse, approve: Approve, max_rows: usize, log: &Log) -> Arc<QueryApi> {
        let settings = w.settings();
        let queries = Queries::load(&settings, &approve).unwrap();
        Arc::new(QueryApi {
            queries: RwLock::new(Arc::new(queries)),
            runner: w.runner(max_rows, None),
            settings,
            approve,
            metrics: Arc::default(),
            stdout: StdoutQueue::start(Box::new(log.clone())),
            redactor: Arc::new(
                crate::serve::secrets::Redactor::new([(
                    "FHIR_PIPES_QUERY_TOKEN".to_string(),
                    SECRET.to_string(),
                )])
                .0,
            ),
        })
    }

    const SECRET: &str = "s3cret-query-token";

    #[tokio::test]
    async fn the_log_line_hides_secrets() {
        if !have_duckdb("the_log_line_hides_secrets") {
            return;
        }
        let w = Warehouse::new();
        let log = Log::default();
        let api = api(&w, Approve::All, 100, &log);
        let r = send(&api, get(&[("subjectCanonical", SECRET)])).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let line = log.line("query ");
        assert!(line.contains("***") && !line.contains(SECRET), "{line}");
    }

    #[tokio::test]
    async fn head_runs_no_query_but_works_on_discovery_routes() {
        if !have_duckdb("head_runs_no_query_but_works_on_discovery_routes") {
            return;
        }
        let w = Warehouse::new();
        let log = Log::default();
        let api = api(&w, Approve::All, 100, &log);
        let head = |path: &str| Request::head(path).body(Body::empty()).unwrap();
        let r = send(
            &api,
            head(&format!("/fhir/$sql-run?subjectCanonical={}", enc(Q))),
        )
        .await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
        let metrics = api
            .metrics
            .render(&[], std::path::Path::new("/nonexistent"), false, 0);
        assert!(!metrics.contains("status=\"200\""), "{metrics}");
        for path in [
            "/fhir/metadata",
            "/fhir/OperationDefinition/fhir-pipes-sql-run",
            "/fhir/Library",
        ] {
            assert_eq!(
                send(&api, head(path)).await.status,
                StatusCode::OK,
                "{path}"
            );
        }
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }

        fn content_type(&self) -> &str {
            self.headers["content-type"].to_str().unwrap()
        }

        /// The OperationOutcome's diagnostics.
        fn says(&self) -> String {
            let v = self.json();
            assert_eq!(v["resourceType"], "OperationOutcome", "{v}");
            v["issue"][0]["diagnostics"].as_str().unwrap().to_string()
        }
    }

    async fn send(api: &Arc<QueryApi>, req: Request<Body>) -> Reply {
        let res = routes(api.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    fn enc(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    /// `GET /fhir/$sql-run` with `query` (unencoded `name=value` pairs).
    fn get(query: &[(&str, &str)]) -> Request<Body> {
        let q: Vec<String> = query
            .iter()
            .map(|(n, v)| format!("{}={}", enc(n), enc(v)))
            .collect();
        Request::get(format!("/fhir/$sql-run?{}", q.join("&")))
            .body(Body::empty())
            .unwrap()
    }

    fn post(parameters: Value) -> Request<Body> {
        Request::post("/fhir/$sql-run")
            .header("content-type", "application/fhir+json")
            .body(Body::from(parameters.to_string()))
            .unwrap()
    }

    fn with_accept(mut req: Request<Body>, accept: &str) -> Request<Body> {
        req.headers_mut().insert("accept", accept.parse().unwrap());
        req
    }

    #[tokio::test]
    async fn get_and_post_run_the_same_query() {
        if !have_duckdb("get_and_post_run_the_same_query") {
            return;
        }
        let w = Warehouse::new();
        let log = Log::default();
        let api = api(&w, Approve::All, 100, &log);
        let got = send(&api, get(&[("subjectCanonical", Q)])).await;
        let posted = send(
            &api,
            post(json!({
                "resourceType": "Parameters",
                "parameter": [{"name": "subjectCanonical", "valueCanonical": Q}],
            })),
        )
        .await;
        for r in [&got, &posted] {
            assert_eq!(
                r.status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&r.body)
            );
            assert_eq!(r.content_type(), "application/x-ndjson");
        }
        assert_eq!(got.body, posted.body);
        assert_eq!(lines(&got.body), rows());
        let line = log.line("query ");
        assert!(
            line.starts_with(&format!("query {Q} ndjson 200 3 rows ")) && line.ends_with(" ms"),
            "{line}"
        );
        let metrics = api
            .metrics
            .render(&[], std::path::Path::new("/nonexistent"), false, 0);
        assert!(
            metrics.contains("fhir_pipes_queries_total{status=\"200\"} 2"),
            "{metrics}"
        );
    }

    #[tokio::test]
    async fn format_precedence() {
        if !have_duckdb("format_precedence") {
            return;
        }
        let w = Warehouse::new();
        let api = api(&w, Approve::All, 100, &Log::default());
        let subject = ("subjectCanonical", Q);
        for (query, accept, want) in [
            (
                vec![subject, ("_format", "csv")],
                "application/json",
                "text/csv",
            ),
            (vec![subject], "application/json", "application/json"),
            (vec![subject], "text/html, text/csv;q=0.9", "text/csv"),
            (
                vec![subject],
                "application/json;Q=0, text/csv;q=0.5",
                "text/csv",
            ),
            (
                vec![subject],
                "application/json ; q = 0.2, text/csv ;Q= 0.5",
                "text/csv",
            ),
            (vec![subject], "*/*", "application/x-ndjson"),
            (
                vec![subject],
                "application/json;q=0, text/csv;q=1",
                "text/csv",
            ),
            (
                vec![subject],
                "text/csv;q=0.5, application/json;q=0.8",
                "application/json",
            ),
            (vec![subject], "text/csv, application/json", "text/csv"),
            (
                vec![subject],
                "application/json;q=0",
                "application/x-ndjson",
            ),
            (
                vec![subject, ("_format", "json")],
                "text/csv",
                "application/json",
            ),
        ] {
            let r = send(&api, with_accept(get(&query), accept)).await;
            assert_eq!(r.status, StatusCode::OK, "{query:?} {accept}");
            assert_eq!(r.content_type(), want, "{query:?} {accept}");
        }
    }

    #[tokio::test]
    async fn unsupported_parameters_are_400() {
        if !have_duckdb("unsupported_parameters_are_400") {
            return;
        }
        let w = Warehouse::new();
        let api = api(&w, Approve::All, 100, &Log::default());
        for name in [
            "subjectResource",
            "subjectReference",
            "context",
            "resource",
            "parameters",
            "patient",
            "_since",
            "header",
            "foo",
        ] {
            let r = send(&api, get(&[("subjectCanonical", Q), (name, "x")])).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{name}");
            assert!(r.says().contains(name), "{name}: {}", r.says());
            let r = send(
                &api,
                post(json!({
                    "resourceType": "Parameters",
                    "parameter": [
                        {"name": "subjectCanonical", "valueCanonical": Q},
                        {"name": name, "valueString": "x"},
                    ],
                })),
            )
            .await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "POST {name}");
            assert!(r.says().contains(name), "POST {name}: {}", r.says());
        }
        for (query, says) in [
            (
                vec![("subjectCanonical", Q), ("subjectCanonical", Q)],
                "subjectCanonical",
            ),
            (vec![("subjectCanonical", Q), ("_format", "xml")], "xml"),
            (vec![("subjectCanonical", Q), ("_limit", "0")], "_limit"),
            (vec![("subjectCanonical", Q), ("_limit", "two")], "_limit"),
            (vec![("subjectCanonical", Q), ("_limit", "-1")], "_limit"),
        ] {
            let r = send(&api, get(&query)).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{query:?}");
            assert!(r.says().contains(says), "{query:?}: {}", r.says());
        }
        // A parameter in the wrong type is refused too.
        let r = send(
            &api,
            post(json!({
                "resourceType": "Parameters",
                "parameter": [
                    {"name": "subjectCanonical", "valueCanonical": Q},
                    {"name": "_limit", "valueString": "2"},
                ],
            })),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.says().contains("valueInteger"), "{}", r.says());
        let r = send(
            &api,
            post(json!({
                "resourceType": "Parameters",
                "parameter": [
                    {"name": "subjectCanonical", "valueCanonical": Q},
                    {"name": "_limit", "valueInteger": 2},
                ],
            })),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(lines(&r.body), rows()[..2]);
    }

    #[tokio::test]
    async fn statuses() {
        if !have_duckdb("statuses") {
            return;
        }
        let w = Warehouse::new();
        let log = Log::default();
        let all = api(&w, Approve::All, 100, &log);
        let active = api(&w, Approve::Active, 100, &log);

        let r = send(&all, get(&[])).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.says().contains("subjectCanonical"), "{}", r.says());

        let r = send(
            &all,
            get(&[("subjectCanonical", "https://example.org/Nope")]),
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // A draft under `active` and a SQLView say the same.
        let draft = send(&active, get(&[("subjectCanonical", Q)])).await;
        assert_eq!(draft.status, StatusCode::FORBIDDEN);
        let view = send(
            &all,
            get(&[("subjectCanonical", "https://example.org/Library/TaskBig")]),
        )
        .await;
        assert_eq!(view.status, StatusCode::FORBIDDEN);
        assert_eq!(
            draft.says().replace(Q, "<q>"),
            view.says()
                .replace("https://example.org/Library/TaskBig", "<q>")
        );
        assert!(
            draft.says().contains("not an approved SQLQuery"),
            "{}",
            draft.says()
        );

        // duckdb's error goes to the log only.
        let bad = "https://example.org/Library/Bad";
        let r = send(&all, get(&[("subjectCanonical", bad)])).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.says(), "the query failed; the serve log has the details");
        assert!(!String::from_utf8_lossy(&r.body).contains("nope"));
        let line = log.line(&format!("query {bad} "));
        assert!(
            line.contains(" 422 0 rows ") && line.contains("nope"),
            "{line}"
        );

        let held = all.runner.slot().unwrap();
        let r = send(&all, get(&[("subjectCanonical", Q)])).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers["retry-after"], "5");
        assert_eq!(r.says(), "too many queries running; retry later");
        drop(held);

        // Which table is missing goes to the log only.
        let parquet = w.data().join("parquet/task/country=UGA/part-0.parquet");
        let kept = std::fs::read(&parquet).unwrap();
        std::fs::remove_file(&parquet).unwrap();
        let r = send(&all, get(&[("subjectCanonical", Q)])).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers["retry-after"], "5");
        assert_eq!(r.says(), "an input table is not available yet; retry later");
        let line = log.line("no table task yet");
        assert!(
            line.contains(&format!("query {Q} ndjson 503 0 rows ")),
            "{line}"
        );
        std::fs::write(&parquet, kept).unwrap();

        let r = send(
            &all,
            Request::post("/fhir/$sql-run")
                .header("content-type", "text/plain")
                .body(Body::from("subjectCanonical=x"))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        r.says();
        let big = json!({
            "resourceType": "Parameters",
            "parameter": [{"name": "subjectCanonical", "valueCanonical": "x".repeat(70_000)}],
        });
        let r = send(&all, post(big)).await;
        assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
        r.says();
        let r = send(&all, post(json!({"resourceType": "Patient"}))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.says().contains("Parameters"), "{}", r.says());

        // A repeated member would hide what the first one said.
        for body in [
            format!(
                r#"{{"resourceType": "Parameters",
                    "parameter": [{{"name": "patient", "valueString": "x"}}],
                    "parameter": [{{"name": "subjectCanonical", "valueCanonical": "{Q}"}}]}}"#
            ),
            format!(
                r#"{{"resourceType": "Parameters", "parameter": [
                    {{"name": "patient", "name": "subjectCanonical", "valueCanonical": "{Q}"}}]}}"#
            ),
        ] {
            let r = send(
                &all,
                Request::post("/fhir/$sql-run")
                    .header("content-type", "application/fhir+json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST);
            assert!(r.says().contains("more than once"), "{}", r.says());
        }

        // Other paths and methods under /fhir answer with an OperationOutcome.
        for (method, path, status) in [
            ("GET", "/fhir/Patient", StatusCode::NOT_FOUND),
            ("GET", "/fhir", StatusCode::NOT_FOUND),
            ("GET", "/fhir/", StatusCode::NOT_FOUND),
            ("DELETE", "/fhir/metadata", StatusCode::METHOD_NOT_ALLOWED),
            ("PUT", "/fhir/$sql-run", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let r = send(&all, req).await;
            assert_eq!(r.status, status, "{method} {path}");
            r.says();
        }
    }

    #[tokio::test]
    async fn a_versioned_canonical_resolves_and_an_unknown_version_is_404() {
        if !have_duckdb("a_versioned_canonical_resolves_and_an_unknown_version_is_404") {
            return;
        }
        let w = Warehouse::new();
        let mut v = crate::serve::query::testing::library(
            "V",
            false,
            &[("task", "https://example.org/ViewDefinition/Task")],
            "SELECT id FROM task ORDER BY id",
        );
        v["version"] = "1.0".into();
        w.write("versioned", &v);
        let api = api(&w, Approve::All, 100, &Log::default());
        let url = "https://example.org/Library/V";
        let r = send(&api, get(&[("subjectCanonical", &format!("{url}|1.0"))])).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(lines(&r.body).len(), 4);
        let r = send(&api, get(&[("subjectCanonical", &format!("{url}|2.0"))])).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        r.says();
    }

    #[tokio::test]
    async fn truncation_header() {
        if !have_duckdb("truncation_header") {
            return;
        }
        let w = Warehouse::new();
        let log = Log::default();
        let api_with = api;
        let api = api_with(&w, Approve::All, 1, &log);
        let r = send(&api, get(&[("subjectCanonical", Q)])).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers["x-fhir-pipes-truncated"], "max_rows=1");
        assert_eq!(lines(&r.body), rows()[..1]);
        assert!(log.line("query ").ends_with(" ms truncated at 1"));
        // Cut by the caller's _limit, under or at max_rows: no header.
        for (max_rows, limit) in [(2, "1"), (1, "1")] {
            let api = api_with(&w, Approve::All, max_rows, &log);
            let r = send(&api, get(&[("subjectCanonical", Q), ("_limit", limit)])).await;
            assert_eq!(r.status, StatusCode::OK, "{max_rows} {limit}");
            assert_eq!(lines(&r.body).len(), 1);
            assert!(
                r.headers.get("x-fhir-pipes-truncated").is_none(),
                "{max_rows} {limit}"
            );
        }
    }

    #[tokio::test]
    async fn metadata_and_library_bundle() {
        if !have_duckdb("metadata_and_library_bundle") {
            return;
        }
        let w = Warehouse::new();
        w.set_status("q", "active");
        let api = api(&w, Approve::Active, 100, &Log::default());
        let get = |path: &str| Request::get(path).body(Body::empty()).unwrap();

        let r = send(&api, get("/fhir/metadata")).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.content_type(), "application/fhir+json");
        let cs = r.json();
        assert_eq!(cs["resourceType"], "CapabilityStatement");
        assert_eq!(cs["fhirVersion"], "4.0.1");
        assert_eq!(cs["rest"][0]["mode"], "server");
        assert_eq!(
            cs["date"],
            chrono::Utc::now().format("%Y-%m-%d").to_string()
        );
        assert_eq!(cs["software"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(cs["implementation"]["url"].is_null());
        assert_eq!(cs["implementation"]["description"], "fhir-pipes serve");
        let op = &cs["rest"][0]["operation"][0];
        assert_eq!(op["name"], "sql-run");
        let definition = op["definition"].as_str().unwrap();
        assert_eq!(
            definition,
            "https://fhir-pipes.local/fhir/OperationDefinition/fhir-pipes-sql-run"
        );

        let r = send(&api, get("/fhir/OperationDefinition/fhir-pipes-sql-run")).await;
        assert_eq!(r.status, StatusCode::OK);
        let od = r.json();
        assert_eq!(od["resourceType"], "OperationDefinition");
        assert_eq!(od["url"], definition);
        assert_eq!(
            od["base"],
            "http://hl7.org/fhir/uv/sql-on-fhir/OperationDefinition/SQLRun"
        );
        assert_eq!(
            (&od["code"], &od["system"], &od["type"], &od["instance"]),
            (
                &json!("sql-run"),
                &json!(true),
                &json!(false),
                &json!(false)
            )
        );
        let names: Vec<&str> = od["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["subjectCanonical", "_format", "_limit", "return"]);

        let r = send(&api, get("/fhir/Library")).await;
        assert_eq!(r.status, StatusCode::OK);
        let bundle = r.json();
        assert_eq!(
            (&bundle["resourceType"], &bundle["type"], &bundle["total"]),
            (&json!("Bundle"), &json!("searchset"), &json!(1))
        );
        assert_eq!(bundle["entry"][0]["resource"]["url"], Q);
        assert_eq!(bundle["entry"][0]["resource"]["status"], "active");
        assert_eq!(
            bundle["entry"][0]["fullUrl"],
            "https://fhir-pipes.local/fhir/Library/Q"
        );
        assert_eq!(bundle["entry"][0]["search"]["mode"], "match");
        assert!(bundle["entry"][1].is_null());
        // It takes no search parameters.
        let r = send(&api, get("/fhir/Library?name=Q")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.says().contains("name"), "{}", r.says());
    }

    #[test]
    fn bundle_ids_are_unique_and_keep_a_unique_own_id() {
        let lib = |name: &str, id: Option<&str>, version: Option<&str>| {
            let mut v = crate::serve::query::testing::library(name, false, &[], "SELECT 1");
            if let Some(id) = id {
                v["id"] = id.into();
            }
            if let Some(version) = version {
                v["version"] = version.into();
            }
            crate::library::Library::from_json(&v).unwrap()
        };
        let (a, b) = (lib("A", Some("a"), None), lib("B", Some("b"), None));
        assert_eq!(bundle_ids(&[&a, &b]), ["a", "b"]);
        // A shared id: each gets its name, with the version; a Library
        // whose id no other shares keeps it.
        let (a, b, c) = (
            lib("A", Some("x"), Some("1")),
            lib("B", Some("x"), None),
            lib("C", Some("c"), None),
        );
        assert_eq!(bundle_ids(&[&a, &b, &c]), ["A-1", "B", "c"]);
        // A derived id never takes one a Library keeps, nor another's.
        let (a, b) = (lib("A", None, None), lib("B", Some("A"), None));
        assert_eq!(bundle_ids(&[&a, &b]), ["A-2", "A"]);
        let (a, b, c) = (
            lib("A", None, Some("1")),
            lib("A", None, Some("1")),
            lib("A-1", None, None),
        );
        assert_eq!(bundle_ids(&[&a, &b, &c]), ["A-1", "A-1-2", "A-1-3"]);
        // Characters an id cannot hold become `-`.
        let a = lib("A", None, Some("1.0+b/2"));
        assert_eq!(bundle_ids(&[&a]), ["A-1.0-b-2"]);
    }

    #[tokio::test]
    async fn bundled_ids_agree_with_their_full_urls() {
        if !have_duckdb("bundled_ids_agree_with_their_full_urls") {
            return;
        }
        let w = Warehouse::new();
        for (file, name, id) in [("x1", "X1", "x"), ("x2", "X2", "x"), ("own", "Own", "own")] {
            let mut v = crate::serve::query::testing::library(
                name,
                false,
                &[("task", "https://example.org/ViewDefinition/Task")],
                "SELECT id FROM task",
            );
            v["id"] = id.into();
            w.write(file, &v);
        }
        let api = api(&w, Approve::All, 100, &Log::default());
        let r = send(
            &api,
            Request::get("/fhir/Library").body(Body::empty()).unwrap(),
        )
        .await;
        let bundle = r.json();
        let mut ids = std::collections::BTreeMap::new();
        for e in bundle["entry"].as_array().unwrap() {
            let id = e["resource"]["id"].as_str().unwrap().to_string();
            assert_eq!(
                e["fullUrl"],
                format!("https://fhir-pipes.local/fhir/Library/{id}")
            );
            ids.insert(e["resource"]["name"].as_str().unwrap().to_string(), id);
        }
        assert_eq!(ids["X1"], "X1");
        assert_eq!(ids["X2"], "X2");
        assert_eq!(ids["Own"], "own");
        // The catalog's own copy is unchanged.
        let queries = api.queries();
        let x1 = queries
            .approved()
            .into_iter()
            .find(|l| l.name == "X1")
            .unwrap();
        assert_eq!(x1.json["id"], "x");
    }

    #[test]
    fn reload_reads_the_catalog_again() {
        if !have_duckdb("reload_reads_the_catalog_again") {
            return;
        }
        let w = Warehouse::new();
        let api = api(&w, Approve::Active, 100, &Log::default());
        let approved = |api: &QueryApi| api.queries.read().unwrap().approved().len();
        assert_eq!(approved(&api), 0);
        w.set_status("q", "active");
        api.reload().unwrap();
        assert_eq!(approved(&api), 1);
        // A catalog that does not load (a dependency is missing) keeps the
        // last one.
        w.write(
            "broken",
            &crate::serve::query::testing::library(
                "Broken",
                false,
                &[("x", "https://example.org/Library/Nope")],
                "SELECT * FROM x",
            ),
        );
        assert!(api.reload().is_err());
        assert_eq!(approved(&api), 1);
    }
}
