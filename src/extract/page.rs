//! Phase one: walk the Location search and append every resource to the
//! incoming file, noting per line what the later phases need.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use serde_json::Value;
use url::Url;

use crate::error::{PipesError, Result};
use crate::extract::client::FhirClient;
use crate::fhir::location::is_boundary_extension;
use crate::report::Report;

pub const PAGE_SIZE: usize = 1000;
pub const MAX_PAGES: usize = 10_000;

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
    /// The last page held a full `PAGE_SIZE` of entries and the server gave
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
pub fn boundary_url(resource: &Value) -> Option<String> {
    for ext in resource.get("extension")?.as_array()? {
        let url = ext.get("url").and_then(Value::as_str).unwrap_or("");
        if !is_boundary_extension(url, &[]) {
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

pub fn page_locations(
    client: &FhirClient,
    server: &str,
    since: Option<&str>,
    incoming: &Path,
    report: &mut Report,
) -> Result<PageResult> {
    page_resources(client, server, "Location", since, incoming, report)
}

/// Walk `<server>/<resource_type>` with `_count` paging, appending every
/// resource to `incoming` and noting what the merge needs. Boundary URLs
/// are noted for any type; only Locations carry them.
pub fn page_resources(
    client: &FhirClient,
    server: &str,
    resource_type: &str,
    since: Option<&str>,
    incoming: &Path,
    report: &mut Report,
) -> Result<PageResult> {
    let base = server.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/{resource_type}"))
        .map_err(|_| PipesError::Usage(format!("--server is not a valid URL: {server}")))?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("_count", &PAGE_SIZE.to_string());
        if let Some(s) = since {
            qp.append_pair("_lastUpdated", &format!("ge{s}"));
        }
    }
    if let Some(parent) = incoming.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PipesError::io(parent, e))?;
    }
    let file = std::fs::File::create(incoming).map_err(|e| PipesError::io(incoming, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut seen: HashSet<String> = HashSet::new();
    let mut notes = Vec::new();
    let mut pages = 0usize;
    let mut last_page_len;
    let mut started: Option<String> = None;

    loop {
        let current = url.to_string();
        if !seen.insert(current.clone()) {
            return Err(PipesError::Environment(format!(
                "cyclic pagination: the server repeated the next link {current}"
            )));
        }
        if pages >= MAX_PAGES {
            return Err(PipesError::Environment(format!(
                "more than {MAX_PAGES} pages ({} resources so far); the server's pagination looks broken or _count is being ignored",
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
                boundary_url: boundary_url(resource),
            });
            serde_json::to_writer(&mut out, resource).map_err(|e| match e.io_error_kind() {
                Some(kind) => PipesError::io(incoming, std::io::Error::from(kind)),
                None => PipesError::Json(e),
            })?;
            out.write_all(b"\n")
                .map_err(|e| PipesError::io(incoming, e))?;
        }
        match next_link(&bundle) {
            Some(next) => {
                url = url.join(&next).map_err(|_| {
                    PipesError::Environment(format!(
                        "the server's next link could not be resolved ({next})"
                    ))
                })?;
            }
            None => break,
        }
    }
    out.flush().map_err(|e| PipesError::io(incoming, e))?;
    Ok(PageResult {
        notes,
        pages,
        last_page_full: last_page_len >= PAGE_SIZE,
        started,
    })
}

/// The server's total for `resource_type`, or `None` when it will not say.
/// Tries `_summary=count` first, then a zero-row search with
/// `_total=accurate` (what the Google Healthcare API supports, which
/// rejects `_summary`). Never an error: the count is a cross-check, not a
/// phase.
pub fn count_resources(client: &FhirClient, server: &str, resource_type: &str) -> Option<u64> {
    let base = server.trim_end_matches('/');
    let total = |query: &str| -> Option<u64> {
        let fetched = client
            .get(&format!("{base}/{resource_type}?{query}"))
            .ok()?;
        let bundle: Value = serde_json::from_slice(&fetched.body).ok()?;
        bundle.get("total")?.as_u64()
    };
    total("_summary=count").or_else(|| total("_count=0&_total=accurate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::client::{Auth, FhirClient};

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
            &server.url("/fhir").to_string(),
            "Organization",
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
        let res = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        let res = page_locations(
            &client(),
            &server.url("/fhir/").to_string(),
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
        assert_eq!(boundary_url(&r), None);
        let r = serde_json::json!({"extension":[{"url":"http://hl7.org/fhir/StructureDefinition/location-boundary-geojson","valueAttachment":{"url":"https://x"}}]});
        assert_eq!(boundary_url(&r).as_deref(), Some("https://x"));
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
        let err = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        let err = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        let err = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        let err = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        let res = page_locations(
            &client(),
            &server.url("/fhir").to_string(),
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
        page_locations(
            &client(),
            &server.url("/fhir").to_string(),
            None,
            &out,
            &mut Report::default(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
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
        assert_eq!(boundary_url(&r).as_deref(), Some("https://x"));
    }
}
