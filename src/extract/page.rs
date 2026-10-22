//! Phase one: walk the Location search and append every resource to the
//! incoming file, noting per line what the later phases need.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use serde_json::Value;
use url::Url;

use crate::error::{PipesError, Result};
use crate::fhir::location::is_boundary_extension;
use crate::http::FhirClient;
use crate::report::Report;

pub const PAGE_SIZE: usize = 1000;
pub const MAX_PAGES: usize = 10_000;

/// Medplum answers a count with an estimate from this many resources on
/// (its `accurateCountThreshold`), and with an exact count below it.
pub const MEDPLUM_EXACT_COUNT_BELOW: u64 = 1_000_000;

/// The most resources an Aidbox `_lastUpdated` window should hold: Aidbox
/// pages with an offset (`_page`), which slows down deep into a large result.
pub const AIDBOX_MAX_WINDOW: u64 = 50_000;

/// The kind of FHIR server (`--server-kind`, `server.kind`). It changes only
/// the search: the parameters every page asks for, the size of a
/// `_lastUpdated` window, and which counts are exact. `generic` and `hapi`
/// search the same way.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    /// Any FHIR R4 server: no extra parameters.
    #[default]
    Generic,
    /// HAPI FHIR: the same as generic.
    Hapi,
    /// Medplum: `_sort=_lastUpdated` on every page, so it pages with
    /// `_cursor` links instead of an `_offset` it refuses past 10,000.
    Medplum,
    /// Aidbox: `_total=none` on every page, so a page does not count the
    /// whole result.
    Aidbox,
}

impl ServerKind {
    pub fn name(self) -> &'static str {
        match self {
            ServerKind::Generic => "generic",
            ServerKind::Hapi => "hapi",
            ServerKind::Medplum => "medplum",
            ServerKind::Aidbox => "aidbox",
        }
    }

    /// Added to every page search, and kept on the server's next links.
    pub fn page_params(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ServerKind::Generic | ServerKind::Hapi => &[],
            ServerKind::Medplum => &[("_sort", "_lastUpdated")],
            ServerKind::Aidbox => &[("_total", "none")],
        }
    }

    /// The most resources one `_lastUpdated` window should hold: windows over
    /// it are halved, though never below one second. A type larger than this
    /// is paged in windows even with `--concurrency 1`. Medplum's limit keeps
    /// each window's count exact.
    pub fn max_window(self) -> Option<u64> {
        match self {
            ServerKind::Generic | ServerKind::Hapi => None,
            ServerKind::Medplum => Some(MEDPLUM_EXACT_COUNT_BELOW - 1),
            ServerKind::Aidbox => Some(AIDBOX_MAX_WINDOW),
        }
    }

    /// Whether the server's count `n` may be an estimate rather than exact.
    pub fn count_is_estimate(self, n: u64) -> bool {
        self == ServerKind::Medplum && n >= MEDPLUM_EXACT_COUNT_BELOW
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageNote {
    pub id: String,
    pub last_updated: Option<String>,
    /// A boundary attachment `url` with no `data`, if the resource has one.
    pub boundary_url: Option<String>,
}

#[derive(Debug)]
pub struct PageResult {
    pub notes: Vec<PageNote>,
    pub pages: usize,
    /// The last page was as full as the first (or held `PAGE_SIZE`, when it
    /// was the only one) and the server gave
    /// no next link. A result set that ends on a full page is possible but
    /// rare; a server that stops paging early looks exactly like this, so
    /// the caller cross-checks the count when it sees it.
    pub last_page_full: bool,
    /// The server's HTTP `Date` on the first page: when the search began, by
    /// the server's clock. Caps the watermark (see `capped_watermark`).
    pub started: Option<String>,
}

pub fn next_link(bundle: &Value) -> Option<String> {
    bundle.get("link")?.as_array()?.iter().find_map(|l| {
        if l.get("relation")?.as_str()? != "next" {
            return None;
        }
        l.get("url")?
            .as_str()
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    })
}

/// The first boundary extension whose attachment has a `url` and no `data`.
/// `extra` is the profile's boundary extensions.
pub fn boundary_url(resource: &Value, extra: &[String]) -> Option<String> {
    for ext in resource.get("extension")?.as_array()? {
        let url = ext.get("url").and_then(Value::as_str).unwrap_or("");
        if !is_boundary_extension(url, extra) {
            continue;
        }
        let Some(att) = ext.get("valueAttachment").and_then(Value::as_object) else {
            continue;
        };
        if att
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|d| !d.is_empty())
        {
            return None;
        }
        if let Some(u) = att
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
        {
            return Some(u.to_string());
        }
    }
    None
}

/// One search to page: the server and its kind, the resource type, any
/// search parameters, and the profile's boundary extensions, recognized
/// beside the HL7 one.
#[derive(Clone, Copy)]
pub struct Search<'a> {
    pub server: &'a str,
    pub kind: ServerKind,
    pub resource_type: &'a str,
    /// Search parameters; `_count` and the server kind's are added.
    pub params: &'a [(&'a str, String)],
    pub boundary_extensions: &'a [String],
}

impl<'a> Search<'a> {
    /// Every `resource_type` on `server`, with no boundary extensions of
    /// the profile's.
    pub fn new(server: &'a str, kind: ServerKind, resource_type: &'a str) -> Self {
        Self {
            server,
            kind,
            resource_type,
            params: &[],
            boundary_extensions: &[],
        }
    }
}

/// Walk `search` with `_count` paging, appending every resource to
/// `incoming` and noting what the merge needs; with `since`, only what was
/// updated from then on. Boundary URLs are noted for any type; only
/// Locations carry them.
pub fn page_resources(
    client: &FhirClient,
    search: &Search,
    since: Option<&str>,
    incoming: &Path,
    report: &mut Report,
) -> Result<PageResult> {
    let mut params = search.params.to_vec();
    params.extend(since.map(|s| ("_lastUpdated", format!("ge{s}"))));
    let search = Search {
        params: &params,
        ..*search
    };
    let paged = page_search(client, &search, incoming, report, &mut |_| true)?;
    Ok(paged.expect("paging stops early only when told to"))
}

/// `page_resources` with a look at the first page: when `after_first`
/// returns false, paging stops there and the result is `None`.
pub fn page_search(
    client: &FhirClient,
    search: &Search,
    incoming: &Path,
    report: &mut Report,
    after_first: &mut dyn FnMut(&Value) -> bool,
) -> Result<Option<PageResult>> {
    let Search {
        server,
        kind,
        resource_type,
        params,
        boundary_extensions,
    } = *search;
    let mut url = server_url(server, resource_type)?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("_count", &PAGE_SIZE.to_string());
        qp.extend_pairs(kind.page_params());
        for (k, v) in params {
            qp.append_pair(k, v);
        }
    }
    if let Some(parent) = incoming.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PipesError::io(parent, e))?;
    }
    // Search results can be patient data: readable only by the owner.
    let file = crate::fs::create_private(incoming).map_err(|e| PipesError::io(incoming, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut seen: HashSet<String> = HashSet::new();
    let mut notes = Vec::new();
    let mut pages = 0usize;
    let mut last_page_len;
    // The size of a full page: the first page's, which shows what the
    // server gives (many cap `_count` below PAGE_SIZE).
    let mut page_size = PAGE_SIZE;
    // At most this many pages: MAX_PAGES, or twice what the first page's
    // `total` needs, whichever is more.
    let mut page_limit = MAX_PAGES;
    let mut started: Option<String> = None;
    let origin = url.origin();

    loop {
        crate::stop::check()?;
        let current = url.to_string();
        if !seen.insert(current.clone()) {
            return Err(PipesError::Environment(format!(
                "cyclic pagination: the server repeated the next link {current}"
            )));
        }
        if pages >= page_limit {
            return Err(PipesError::Environment(format!(
                "more than {page_limit} pages ({} resources so far); the server's pagination looks broken or _count is being ignored",
                notes.len()
            )));
        }
        let fetched = client
            .get(&current)
            .map_err(|e| PipesError::Environment(format!("fetching {current}: {e}")))?;
        pages += 1;
        if pages == 1 {
            started = fetched.date.clone();
        }
        let bundle: Value = serde_json::from_slice(&fetched.body).map_err(|e| {
            PipesError::Environment(format!("{current}: response is not JSON: {e}"))
        })?;
        if let Some(rt) = bundle.get("resourceType").and_then(Value::as_str)
            && rt != "Bundle"
        {
            return Err(PipesError::Environment(format!(
                "{current}: expected a Bundle, got resourceType {rt}"
            )));
        }
        let entries = bundle.get("entry").and_then(Value::as_array);
        last_page_len = entries.map_or(0, Vec::len);
        if pages == 1 && last_page_len > 0 {
            page_size = last_page_len.min(PAGE_SIZE);
            if let Some(total) = bundle.get("total").and_then(Value::as_u64) {
                page_limit = page_limit.max((total as usize).div_ceil(page_size) * 2);
            }
        }
        for (i, entry) in entries.into_iter().flatten().enumerate() {
            let Some(resource) = entry.get("resource").filter(|r| r.is_object()) else {
                report.add(
                    "page_resource_skipped",
                    &format!("page {pages} entry {i}"),
                    "entry has no resource object",
                );
                continue;
            };
            let Some(id) = resource
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                report.add(
                    "page_resource_skipped",
                    &format!("page {pages} entry {i}"),
                    "resource has no id",
                );
                continue;
            };
            notes.push(PageNote {
                id: id.to_string(),
                last_updated: resource
                    .pointer("/meta/lastUpdated")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                boundary_url: boundary_url(resource, boundary_extensions),
            });
            serde_json::to_writer(&mut out, resource).map_err(|e| match e.io_error_kind() {
                Some(kind) => PipesError::io(incoming, std::io::Error::from(kind)),
                None => PipesError::Json(e),
            })?;
            out.write_all(b"\n")
                .map_err(|e| PipesError::io(incoming, e))?;
        }
        if pages == 1 && !after_first(&bundle) {
            return Ok(None);
        }
        match next_link(&bundle) {
            Some(next) => {
                url = url.join(&next).map_err(|_| {
                    PipesError::Environment(format!(
                        "the server's next link could not be resolved ({next})"
                    ))
                })?;
                keep_params(&mut url, kind.page_params());
                // The credentials go only to the server's origin, so a link
                // elsewhere would fail without them, or be followed with
                // nothing to stop it from leaving the server.
                if url.origin() != origin {
                    return Err(PipesError::Environment(format!(
                        "the server's next link {url} is not on {}; check the base URL the \
                         server is configured with (behind a proxy, HAPI needs \
                         X-Forwarded-Host and X-Forwarded-Proto)",
                        origin.ascii_serialization()
                    )));
                }
            }
            None => break,
        }
    }
    out.flush().map_err(|e| PipesError::io(incoming, e))?;
    Ok(Some(PageResult {
        notes,
        pages,
        // One page is full only at PAGE_SIZE: there is no other page to
        // tell the server's size by.
        last_page_full: if pages > 1 {
            last_page_len >= page_size
        } else {
            last_page_len >= PAGE_SIZE
        },
        started,
    }))
}

/// Add each of `params` that `url` lacks: a next link that dropped the
/// server kind's parameters gets them back.
fn keep_params(url: &mut Url, params: &[(&str, &str)]) {
    let missing: Vec<(&str, &str)> = params
        .iter()
        .filter(|(k, _)| !url.query_pairs().any(|(name, _)| name == *k))
        .copied()
        .collect();
    if !missing.is_empty() {
        url.query_pairs_mut().extend_pairs(missing);
    }
}

/// The server's total for `resource_type`, or `None` when it will not say.
/// Tries `_summary=count` first, then a zero-row search with
/// `_total=accurate` (what the Google Healthcare API supports, which
/// rejects `_summary`). Never an error: the count is a cross-check, not a
/// phase.
pub fn count_resources(client: &FhirClient, server: &str, resource_type: &str) -> Option<u64> {
    count_matching(client, server, resource_type, &[])
}

/// `<server>/<path>`, one `/` between them: `path` is a resource type (its
/// search) or an operation (`$export`) or a reference (`Binary/1`).
pub fn server_path(server: &str, path: &str) -> String {
    format!("{}/{path}", server.trim_end_matches('/'))
}

/// `server_path`, parsed: an error names `--server`.
pub fn server_url(server: &str, path: &str) -> Result<Url> {
    Url::parse(&server_path(server, path))
        .map_err(|_| PipesError::Usage(format!("--server is not a valid URL: {server}")))
}

/// The server's count of a type set against the count fhir-pipes has.
#[derive(Debug, PartialEq, Eq)]
pub enum CountCheck {
    Same,
    /// The server has this many.
    Differs(u64),
    /// No count to go by, and why not: no total, or a count that differs
    /// but may be an estimate.
    Unavailable(String),
}

/// `count_resources` of `resource_type`, set against `total`.
pub fn check_count(
    client: &FhirClient,
    server: &str,
    kind: ServerKind,
    resource_type: &str,
    total: usize,
) -> CountCheck {
    match count_resources(client, server, resource_type) {
        Some(n) if n as usize == total => CountCheck::Same,
        Some(n) if kind.count_is_estimate(n) => {
            CountCheck::Unavailable(format!("the server's count ({n}) is an estimate"))
        }
        Some(n) => CountCheck::Differs(n),
        None => CountCheck::Unavailable("the server returned no total".to_string()),
    }
}

/// `count_resources` for a search with `params`.
pub fn count_matching(
    client: &FhirClient,
    server: &str,
    resource_type: &str,
    params: &[(&str, String)],
) -> Option<u64> {
    let mut url = server_url(server, resource_type).ok()?;
    url.query_pairs_mut().extend_pairs(params);
    let total = |extra: &[(&str, &str)]| -> Option<u64> {
        let mut url = url.clone();
        url.query_pairs_mut().extend_pairs(extra);
        let fetched = client.get(url.as_str()).ok()?;
        let bundle: Value = serde_json::from_slice(&fetched.body).ok()?;
        bundle.get("total")?.as_u64()
    };
    total(&[("_summary", "count")]).or_else(|| total(&[("_count", "0"), ("_total", "accurate")]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Auth, FhirClient};

    #[test]
    fn count_resources_reads_the_bundle_total_or_gives_up() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains(("_summary", "count")))),
            ])
            .respond_with(
                status_code(200).body(r#"{"resourceType":"Bundle","type":"searchset","total":42}"#),
            ),
        );
        // A server that rejects _summary (the Google Healthcare API does)
        // but answers a zero-row search with _total=accurate.
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Organization"),
                request::query(url_decoded(contains(("_summary", "count")))),
            ])
            .respond_with(status_code(400).body(r#"{"resourceType":"OperationOutcome"}"#)),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Organization"),
                request::query(url_decoded(contains(("_total", "accurate")))),
                request::query(url_decoded(contains(("_count", "0")))),
            ])
            .respond_with(
                status_code(200).body(r#"{"resourceType":"Bundle","type":"searchset","total":7}"#),
            ),
        );
        // A server that answers neither with a total.
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Patient"))
                .times(2)
                .respond_with(
                    status_code(200).body(r#"{"resourceType":"Bundle","type":"searchset"}"#),
                ),
        );
        let client = FhirClient::new(Auth::None, 1, std::time::Duration::from_secs(5)).unwrap();
        let base = server.url("/fhir").to_string();
        assert_eq!(count_resources(&client, &base, "Location"), Some(42));
        assert_eq!(count_resources(&client, &base, "Organization"), Some(7));
        assert_eq!(count_resources(&client, &base, "Patient"), None);
        assert_eq!(
            count_resources(&client, "http://127.0.0.1:9/nothing", "Location"),
            None
        );
    }

    use crate::report::Report;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    #[test]
    fn a_count_is_checked_and_an_estimate_matters_only_when_it_differs() {
        let server = Server::run();
        let big = MEDPLUM_EXACT_COUNT_BELOW;
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location"))
                .times(4)
                .respond_with(status_code(200).body(format!(
                    r#"{{"resourceType":"Bundle","type":"searchset","total":{big}}}"#
                ))),
        );
        let client = FhirClient::new(Auth::None, 1, std::time::Duration::from_secs(5)).unwrap();
        let base = server.url("/fhir").to_string();
        let check = |kind, total| check_count(&client, &base, kind, "Location", total);
        assert_eq!(check(ServerKind::Medplum, big as usize), CountCheck::Same);
        assert!(matches!(
            check(ServerKind::Medplum, 3),
            CountCheck::Unavailable(why) if why.contains("estimate")
        ));
        assert_eq!(check(ServerKind::Generic, 3), CountCheck::Differs(big));
        assert_eq!(check(ServerKind::Generic, big as usize), CountCheck::Same);
        assert_eq!(
            server_path("http://h/fhir/", "$export"),
            "http://h/fhir/$export"
        );
        assert!(server_url("not a url", "Patient").is_err());
    }

    #[test]
    fn page_resources_walks_the_named_type() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Organization"))
                .times(1)
                .respond_with(status_code(200).body(bundle(
                    &[serde_json::json!({"resourceType":"Organization","id":"org-1","meta":{"lastUpdated":"2026-01-01T00:00:00Z"}})],
                    None,
                ))),
        );
        let dir = tempfile::tempdir().unwrap();
        let incoming = dir.path().join("inc.ndjson");
        let client = FhirClient::new(Auth::None, 1, std::time::Duration::from_secs(5)).unwrap();
        let mut report = Report::default();
        let got = page_resources(
            &client,
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Organization",
            ),
            None,
            &incoming,
            &mut report,
        )
        .unwrap();
        assert_eq!(got.notes.len(), 1);
        assert_eq!(got.notes[0].id, "org-1");
        assert_eq!(got.notes[0].boundary_url, None);
        assert!(
            std::fs::read_to_string(&incoming)
                .unwrap()
                .contains("\"id\":\"org-1\"")
        );
    }

    fn bundle(entries: &[serde_json::Value], next: Option<&str>) -> String {
        let mut b = serde_json::json!({"resourceType":"Bundle","type":"searchset","entry": entries.iter().map(|r| serde_json::json!({"resource": r})).collect::<Vec<_>>()});
        if let Some(n) = next {
            b["link"] =
                serde_json::json!([{"relation":"self","url":"x"},{"relation":"next","url":n}]);
        }
        b.to_string()
    }
    fn loc(id: &str, updated: &str, url: Option<&str>) -> serde_json::Value {
        let mut r =
            serde_json::json!({"resourceType":"Location","id":id,"meta":{"lastUpdated":updated}});
        if let Some(u) = url {
            r["extension"] = serde_json::json!([{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"contentType":"application/geo+json","url":u}}]);
        }
        r
    }
    fn client() -> FhirClient {
        FhirClient::new(Auth::None, 1, std::time::Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn a_next_link_to_another_origin_is_refused() {
        let server = Server::run();
        let elsewhere = Server::run();
        let next = elsewhere.url("/fhir/Organization?page=2").to_string();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Organization")).respond_with(
                status_code(200).body(bundle(
                    &[serde_json::json!({"resourceType":"Organization","id":"org-1"})],
                    Some(&next),
                )),
            ),
        );
        let dir = tempfile::tempdir().unwrap();
        let err = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Organization",
            ),
            None,
            &dir.path().join("inc.ndjson"),
            &mut Report::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not on"), "{err}");
    }

    #[test]
    fn a_last_page_as_full_as_the_first_is_suspicious() {
        // A server that caps pages at 2: [2, 2] ends on a full page, [2, 1]
        // does not.
        for (second, full) in [(2, true), (1, false)] {
            let server = Server::run();
            let p2 = server.url("/fhir/Organization?page=2").to_string();
            let org = |id: &str| serde_json::json!({"resourceType":"Organization","id":id});
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", "/fhir/Organization"),
                    request::query(url_decoded(not(contains(("page", "2"))))),
                ])
                .respond_with(status_code(200).body(bundle(&[org("a"), org("b")], Some(&p2)))),
            );
            let rest: Vec<_> = ["c", "d"][..second].iter().map(|i| org(i)).collect();
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", "/fhir/Organization"),
                    request::query(url_decoded(contains(("page", "2")))),
                ])
                .respond_with(status_code(200).body(bundle(&rest, None))),
            );
            let dir = tempfile::tempdir().unwrap();
            let got = page_resources(
                &client(),
                &Search::new(
                    &server.url("/fhir").to_string(),
                    ServerKind::Generic,
                    "Organization",
                ),
                None,
                &dir.path().join("inc.ndjson"),
                &mut Report::default(),
            )
            .unwrap();
            assert_eq!(got.pages, 2);
            assert_eq!(got.last_page_full, full, "{second}");
        }
    }

    #[test]
    fn follows_next_links_and_records_notes() {
        let server = Server::run();
        let p2 = server.url("/fhir/Location?page=2").to_string();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains(("_count", "1000"))))
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(
                &[
                    loc("a", "2026-01-01T00:00:00Z", Some("https://b/a.geojson")),
                    serde_json::json!({"resourceType":"Location"}),
                    serde_json::json!(42),
                ],
                Some(&p2),
            ))),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains(("page", "2"))))
            ])
            .times(1)
            .respond_with(
                status_code(200).body(bundle(&[loc("b", "2026-02-01T00:00:00Z", None)], None)),
            ),
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(".incoming.ndjson");
        let mut report = Report::default();
        let res = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &out,
            &mut report,
        )
        .unwrap();
        assert_eq!(res.pages, 2);
        assert_eq!(res.notes.len(), 2);
        assert_eq!(res.notes[0].id, "a");
        assert_eq!(
            res.notes[0].boundary_url.as_deref(),
            Some("https://b/a.geojson")
        );
        assert_eq!(
            res.notes[1].last_updated.as_deref(),
            Some("2026-02-01T00:00:00Z")
        );
        assert_eq!(
            report.count("page_resource_skipped"),
            2,
            "no id, and not an object"
        );
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .next()
                .unwrap()
                .starts_with("{\"resourceType\":\"Location\",\"id\":\"a\"")
        );
    }

    #[test]
    fn incremental_adds_last_updated_ge_and_trailing_slash_is_tolerated() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains((
                    "_lastUpdated",
                    "ge2026-01-01T00:00:00Z"
                ))))
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(&[], None))),
        );
        let dir = tempfile::tempdir().unwrap();
        let res = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir/").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            Some("2026-01-01T00:00:00Z"),
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!(res.notes.len(), 0);
        assert!(
            dir.path().join("i").exists(),
            "the incoming file exists even when empty"
        );
    }

    #[test]
    fn a_boundary_with_inline_data_is_not_noted_for_fetching() {
        let r = serde_json::json!({"extension":[{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x","data":"abc"}}]});
        assert_eq!(boundary_url(&r, &[]), None);
        let r = serde_json::json!({"extension":[{"url":"http://hl7.org/fhir/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x"}}]});
        assert_eq!(boundary_url(&r, &[]).as_deref(), Some("https://x"));
    }

    #[test]
    fn a_profile_boundary_extension_is_noted_only_with_the_profile() {
        let ext = "https://example.org/StructureDefinition/service-area";
        let r =
            serde_json::json!({"extension":[{"url":ext,"valueAttachment":{"url":"https://x"}}]});
        assert_eq!(boundary_url(&r, &[]), None);
        assert_eq!(
            boundary_url(&r, &[ext.to_string()]).as_deref(),
            Some("https://x")
        );
    }

    #[test]
    fn a_repeated_next_link_is_an_error() {
        let server = Server::run();
        let me = server.url("/fhir/Location?loop=1").to_string();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location"))
                .times(..)
                .respond_with(status_code(200).body(bundle(&[], Some(&me)))),
        );
        let dir = tempfile::tempdir().unwrap();
        let err = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cyclic"), "{err}");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn a_non_200_page_is_an_environment_error_with_the_status() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location"))
                .respond_with(status_code(401).body("nope")),
        );
        let dir = tempfile::tempdir().unwrap();
        let err = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[test]
    fn a_non_json_page_is_an_environment_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location"))
                .respond_with(status_code(200).body("<html>")),
        );
        let dir = tempfile::tempdir().unwrap();
        let err = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not JSON"), "{err}");
    }

    #[test]
    fn a_non_bundle_response_is_an_environment_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location")).respond_with(
                status_code(200)
                    .body(serde_json::json!({"resourceType":"OperationOutcome"}).to_string()),
            ),
        );
        let dir = tempfile::tempdir().unwrap();
        let err = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected a Bundle"), "{err}");
    }

    #[test]
    fn since_with_an_offset_is_percent_encoded() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains((
                    "_lastUpdated",
                    "ge2026-01-01T00:00:00+01:00"
                ))))
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(&[], None))),
        );
        let dir = tempfile::tempdir().unwrap();
        page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            Some("2026-01-01T00:00:00+01:00"),
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap();
    }

    #[test]
    fn a_relative_next_link_is_resolved() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains(("_count", "1000"))))
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(
                &[loc("a", "2026-01-01T00:00:00Z", None)],
                Some("/fhir/Location?page=2"),
            ))),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/fhir/Location"),
                request::query(url_decoded(contains(("page", "2"))))
            ])
            .times(1)
            .respond_with(status_code(200).body(bundle(&[], None))),
        );
        let dir = tempfile::tempdir().unwrap();
        let res = page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &dir.path().join("i"),
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!(res.pages, 2);
        assert_eq!(res.notes.len(), 1);
    }

    #[test]
    fn an_existing_incoming_file_is_truncated() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Location")).respond_with(
                status_code(200).body(bundle(&[loc("a", "2026-01-01T00:00:00Z", None)], None)),
            ),
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("i");
        std::fs::write(&out, "leftover junk from a previous run\nmore junk\n").unwrap();
        page_resources(
            &client(),
            &Search::new(
                &server.url("/fhir").to_string(),
                ServerKind::Generic,
                "Location",
            ),
            None,
            &out,
            &mut Report::default(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
    }

    #[test]
    fn the_server_kinds_parameters_are_on_every_page() {
        for (kind, param) in [
            (ServerKind::Medplum, ("_sort", "_lastUpdated")),
            (ServerKind::Aidbox, ("_total", "none")),
        ] {
            let server = Server::run();
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", "/fhir/Location"),
                    request::query(url_decoded(contains(param))),
                    request::query(url_decoded(not(contains(key("page"))))),
                ])
                .times(1)
                // The next link leaves the parameter out; it is added back.
                .respond_with(status_code(200).body(bundle(
                    &[loc("a", "2026-01-01T00:00:00Z", None)],
                    Some("/fhir/Location?_count=1000&page=2"),
                ))),
            );
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", "/fhir/Location"),
                    request::query(url_decoded(contains(param))),
                    request::query(url_decoded(contains(("page", "2")))),
                ])
                .times(1)
                .respond_with(
                    status_code(200).body(bundle(&[loc("b", "2026-01-01T00:00:00Z", None)], None)),
                ),
            );
            let dir = tempfile::tempdir().unwrap();
            let res = page_resources(
                &client(),
                &Search::new(&server.url("/fhir").to_string(), kind, "Location"),
                None,
                &dir.path().join("i"),
                &mut Report::default(),
            )
            .unwrap();
            assert_eq!(res.notes.len(), 2, "{kind:?}");
        }
        assert!(ServerKind::Generic.page_params().is_empty());
        assert_eq!(
            ServerKind::Hapi.page_params(),
            ServerKind::Generic.page_params()
        );
    }

    #[test]
    fn only_medplums_large_counts_are_estimates() {
        assert!(!ServerKind::Medplum.count_is_estimate(999_999));
        assert!(ServerKind::Medplum.count_is_estimate(1_000_000));
        assert!(!ServerKind::Generic.count_is_estimate(5_000_000));
        assert!(!ServerKind::Aidbox.count_is_estimate(5_000_000));
    }

    #[test]
    fn next_link_ignores_an_empty_url() {
        let b = bundle(&[], Some(""));
        let parsed: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(next_link(&parsed), None);
    }

    #[test]
    fn a_boundary_extension_without_attachment_is_skipped_for_the_next_one() {
        let r = serde_json::json!({"extension":[
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson"},
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x"}}
        ]});
        assert_eq!(boundary_url(&r, &[]).as_deref(), Some("https://x"));
    }
}
