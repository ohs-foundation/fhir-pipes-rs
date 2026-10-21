//! The Bulk Data `$export` protocol (FHIR Bulk Data Access 2.0) at the system
//! level: the kick-off, polling the status URL until the export is done, and
//! reading the manifest's files line by line as they download. What
//! `export --bulk` does with the lines is in `bulk_run`.
//!
//! A kick-off answered with 429 is tried again until the deadline, with the
//! same waits as a status poll: a server that runs one export at a time
//! (Aidbox) answers 429 while another is running.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::error::{PipesError, Result};
use crate::extract::page::{server_path, server_url};
use crate::fhir::ndjson::Splitter;
use crate::http::{Exchange, FetchError, FhirClient};
use crate::snapshot::instant::{format_utc, parse_instant};

/// The longest failure text shown from a status response.
const DISPLAY_CAP: usize = 500;

/// The first wait when the status URL gives no Retry-After; it doubles up to
/// POLL_MAX on each answer without one.
const POLL_FIRST: Duration = Duration::from_secs(1);
const POLL_MAX: Duration = Duration::from_secs(60);
/// The shortest wait between polls, whatever the server's Retry-After says.
const POLL_MIN: Duration = Duration::from_secs(1);
/// The longest wait after a 202, whatever its Retry-After says. HAPI sends
/// `Retry-After: 120` on every 202, so an export done in 20 s would
/// otherwise be read two minutes after it started. A status poll is a cheap
/// request; a Retry-After on a 429 or 5xx is honoured in full.
const POLL_RUNNING_MAX: Duration = Duration::from_secs(15);

/// An export the server is running. Saved in the export state as soon as the
/// kick-off answers, so a run that stops before the export is done polls it
/// again next time instead of starting another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub status_url: String,
    /// The `_type` list, sorted.
    pub types: Vec<String>,
    /// The `_since`, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// This machine's clock when the kick-off answered, for display.
    pub started_at: String,
    /// The server's clock then: the kick-off's `Date` header, as a FHIR
    /// instant. None when the response had no valid `Date`, or the state
    /// was saved by an earlier version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_time: Option<String>,
}

/// An export the kick-off started.
#[derive(Debug)]
pub struct Started {
    pub status_url: String,
    /// The kick-off's `Date`, as a FHIR instant (see `Pending::server_time`).
    pub server_time: Option<String>,
}

/// The completion manifest, the parts that are used.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// The server's time when the export ran: everything changed up to it is
    /// in the files, so it is the next export's `_since`.
    pub transaction_time: Option<String>,
    #[serde(default)]
    pub requires_access_token: bool,
    #[serde(default)]
    pub output: Vec<ManifestFile>,
    /// Bundles of `DELETE` entries, one per resource deleted since `_since`.
    #[serde(default)]
    pub deleted: Vec<ManifestFile>,
    /// OperationOutcome NDJSON.
    #[serde(default)]
    pub error: Vec<ManifestFile>,
}

impl Manifest {
    /// Every file the manifest lists: `output`, `deleted`, then `error`.
    pub fn files(&self) -> impl Iterator<Item = &ManifestFile> {
        self.output.iter().chain(&self.deleted).chain(&self.error)
    }

    fn files_mut(&mut self) -> impl Iterator<Item = &mut ManifestFile> {
        self.output
            .iter_mut()
            .chain(&mut self.deleted)
            .chain(&mut self.error)
    }
}

#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub url: String,
    /// The `url` as the manifest gave it, when that was a reference such as
    /// `Binary/123` rather than a URL (see `read_references_from`).
    #[serde(skip)]
    pub reference: Option<String>,
}

/// How waiting for an export ended, when the server answered.
#[derive(Debug)]
pub enum Polled {
    Done(Manifest),
    /// 404 or 410: the server does not know the status URL, because the
    /// export expired or was deleted.
    Gone,
    /// The export failed: the status and the server's OperationOutcome.
    Failed(String),
    /// Still running at the deadline.
    TimedOut,
}

/// `[base]/$export?_type=…`, with `_since` for an incremental export.
pub fn kick_off_url(server: &str, types: &[String], since: Option<&str>) -> Result<Url> {
    let mut url = server_url(server, "$export")?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("_type", &types.join(","));
        if let Some(s) = since {
            qp.append_pair("_since", s);
        }
    }
    Ok(url)
}

/// Start a system-level export and return its status URL (the kick-off's
/// `Content-Location`, resolved against the kick-off URL) and the server's
/// clock. A 429 is tried again until `deadline`, waiting as `wait` does
/// after one. The requests, the client's own retries of a 5xx or a failed
/// connection, and every wait end by `deadline`: None when the server had
/// not accepted the kick-off by then.
pub fn kick_off(
    client: &FhirClient,
    server: &str,
    types: &[String],
    since: Option<&str>,
    deadline: Instant,
) -> Result<Option<Started>> {
    let url = kick_off_url(server, types, since)?;
    let mut backoff = POLL_FIRST;
    let mut busy = false;
    let got = loop {
        let got = match client.get_async(url.as_str(), Some(deadline)) {
            Ok(got) => got,
            // Cut short by the deadline: a failure that would have been
            // tried again, not a final answer.
            Err(e) if Instant::now() >= deadline && retried(&e) => return Ok(None),
            Err(FetchError::Status { status, body }) => {
                return Err(PipesError::Environment(format!(
                    "the bulk export kick-off {url} failed with HTTP {status}: {}{}",
                    outcome_text(body.as_bytes()).unwrap_or_else(|| body.clone()),
                    kick_off_hint(status, &body)
                )));
            }
            Err(other) => return Err(PipesError::Environment(format!("{url}: {other}"))),
        };
        if got.status != 429 {
            break got;
        }
        if !busy {
            let why = outcome_text(&got.body).map_or(String::new(), |t| format!(": {t}"));
            println!("   bulk export: the server is busy (HTTP 429{why}); waiting to start");
            busy = true;
        }
        let delay = next_wait(429, got.retry_after(), &mut backoff);
        let now = Instant::now();
        if now + delay >= deadline {
            std::thread::sleep(deadline.saturating_duration_since(now));
            return Ok(None);
        }
        std::thread::sleep(delay);
    };
    if got.status != 202 {
        return Err(PipesError::Environment(format!(
            "{url}: expected 202 Accepted from a bulk export kick-off, got HTTP {}",
            got.status
        )));
    }
    let location = got.header("content-location").ok_or_else(|| {
        PipesError::Environment(format!(
            "{url}: the bulk export kick-off has no Content-Location"
        ))
    })?;
    let status_url = url.join(location).map_err(|_| {
        PipesError::Environment(format!("{url}: Content-Location {location} is not a URL"))
    })?;
    let server_time = got
        .header("date")
        .and_then(|d| httpdate::parse_http_date(d).ok())
        .map(format_utc);
    Ok(Some(Started {
        status_url: status_url.to_string(),
        server_time,
    }))
}

/// Whether the client tries a request again after `e`: a failed
/// connection, a 429 or a 5xx. Any other status is a final answer.
fn retried(e: &FetchError) -> bool {
    match e {
        FetchError::Transport(_) => true,
        FetchError::Status { status, .. } => *status == 429 || *status >= 500,
    }
}

/// The watermark an export gives its types: its `transactionTime`, or the
/// server's clock at the kick-off when that is earlier. Medplum sets
/// `transactionTime` when the last type has been read, so a resource changed
/// while the export ran can be older than it and missing from the files; the
/// next export's `_since` must not pass over it. The merge by id makes
/// fetching a resource again harmless.
pub fn watermark(transaction_time: &str, server_time: Option<&str>) -> String {
    match (
        parse_instant(transaction_time),
        server_time.and_then(|s| parse_instant(s).map(|at| (s, at))),
    ) {
        (Some(tt), Some((s, at))) if at < tt => s.to_string(),
        _ => transaction_time.to_string(),
    }
}

/// What to do about a kick-off failure whose cause is known, or nothing.
/// Aidbox answers 500 "storage-type not specified" when it has no bulk
/// export storage set up.
fn kick_off_hint(status: u16, body: &str) -> &'static str {
    if status == 500 && body.contains("storage-type") {
        ". Aidbox needs a storage provider for bulk export: set \
         BOX_FHIR_BULK_STORAGE_PROVIDER (gcp, aws or azure) and its account settings"
    } else {
        ""
    }
}

/// Poll `status_url` until the export is done, has failed, or `deadline`
/// passes. 202 and 429 mean "not yet": the wait is the server's Retry-After,
/// or a backoff from POLL_FIRST doubling to POLL_MAX without one; after a
/// 202 it is at most POLL_RUNNING_MAX. Each new
/// X-Progress is printed. An error status with a `transient` issue, or a 5xx
/// that is not an OperationOutcome (a gateway's), is waited out the same way.
/// A connection that fails after the client's retries is an error.
pub fn wait(client: &FhirClient, status_url: &str, deadline: Instant) -> Result<Polled> {
    let mut backoff = POLL_FIRST;
    let mut progress: Option<String> = None;
    loop {
        crate::stop::check()?;
        if Instant::now() >= deadline {
            return Ok(Polled::TimedOut);
        }
        // Bounded by the deadline: a status request still open then is a
        // timeout, which cancels the export, not a transport error.
        let got = match client.exchange_by(Method::GET, status_url, Some(deadline)) {
            Ok(got) => got,
            Err(_) if Instant::now() >= deadline => return Ok(Polled::TimedOut),
            Err(e) => {
                return Err(PipesError::Environment(format!(
                    "polling {status_url}: {e}"
                )));
            }
        };
        let retry_after = match got.status {
            200 => return manifest(&got, status_url).map(Polled::Done),
            202 | 429 => {
                if let Some(p) = got.header("x-progress").filter(|p| !p.is_empty())
                    && progress.as_deref() != Some(p)
                {
                    println!("   bulk export: {p}");
                    progress = Some(p.to_string());
                }
                got.retry_after()
            }
            404 | 410 => return Ok(Polled::Gone),
            status => {
                // A body cut at the limit is not parsed: it is not one
                // OperationOutcome the poller can trust.
                let found = if got.truncated {
                    Vec::new()
                } else {
                    serde_json::from_slice::<Value>(&got.body)
                        .map(|v| issues(&v))
                        .unwrap_or_default()
                };
                let transient = !found.is_empty() && found.iter().all(|i| i.code == "transient");
                // A 5xx that is not an OperationOutcome is a gateway's, not
                // the export's.
                let gateway = found.is_empty() && status >= 500;
                if !(transient || gateway) {
                    // Cut for display only, after the body was classified.
                    let text: String = if found.is_empty() {
                        String::from_utf8_lossy(&got.body).into_owned()
                    } else {
                        join_issues(&found)
                    }
                    .chars()
                    .take(DISPLAY_CAP)
                    .collect();
                    return Ok(Polled::Failed(format!("HTTP {status}: {text}")));
                }
                got.retry_after()
            }
        };
        let delay = next_wait(got.status, retry_after, &mut backoff);
        let now = Instant::now();
        if now >= deadline {
            return Ok(Polled::TimedOut);
        }
        // The loop checks the deadline again before the next poll.
        std::thread::sleep(delay.min(deadline - now));
    }
}

/// The wait before the next poll, after a "not yet" with `status`: the
/// server's Retry-After, else `backoff`, which doubles up to POLL_MAX. At
/// least POLL_MIN, and after a 202 at most POLL_RUNNING_MAX.
fn next_wait(status: u16, retry_after: Option<Duration>, backoff: &mut Duration) -> Duration {
    let delay = retry_after
        .unwrap_or_else(|| {
            let d = *backoff;
            *backoff = (*backoff * 2).min(POLL_MAX);
            d
        })
        .max(POLL_MIN);
    if status == 202 {
        delay.min(POLL_RUNNING_MAX)
    } else {
        delay
    }
}

/// A completion response's manifest, its file URLs resolved against the
/// status URL.
fn manifest(got: &Exchange, status_url: &str) -> Result<Manifest> {
    let mut m: Manifest = serde_json::from_slice(&got.body).map_err(|e| {
        PipesError::Environment(format!(
            "{status_url}: the completed export's manifest is not valid: {e}"
        ))
    })?;
    let base = Url::parse(status_url)
        .map_err(|_| PipesError::Environment(format!("{status_url} is not a URL")))?;
    for f in m.files_mut() {
        if is_reference(&f.url) {
            f.reference = Some(f.url.clone());
        }
        f.url = base
            .join(&f.url)
            .map_err(|_| PipesError::Environment(format!("{status_url}: {} is not a URL", f.url)))?
            .to_string();
    }
    Ok(m)
}

/// `Type/id`: a FHIR reference, relative to the server's base.
fn is_reference(url: &str) -> bool {
    url.split_once('/')
        .is_some_and(|(t, id)| crate::fhir::valid_type(t) && crate::fhir::group::is_valid_id(id))
}

/// Medplum lists each file as a reference, `Binary/<id>`, relative to the
/// FHIR base rather than the status URL, and serves it only with the access
/// token, although the manifest says `requiresAccessToken: false`. When every
/// file is such a reference, they are read from `server` with the token.
pub fn read_references_from(m: &mut Manifest, server: &str) {
    if m.files().next().is_none() || !m.files().all(|f| f.reference.is_some()) {
        return;
    }
    for f in m.files_mut() {
        if let Some(r) = &f.reference {
            f.url = server_path(server, r);
        }
    }
    m.requires_access_token = true;
}

/// DELETE the status URL: cancels a running export, or tells the server it
/// may remove a finished one's files.
pub fn delete(client: &FhirClient, status_url: &str) -> std::result::Result<(), String> {
    match client.exchange(Method::DELETE, status_url) {
        Ok(got) if (200..300).contains(&got.status) => Ok(()),
        Ok(got) => Err(format!("HTTP {}", got.status)),
        Err(e) => Err(e.to_string()),
    }
}

/// The longest line of a Bulk Data file read: far more than any resource.
const MAX_LINE: u64 = 256 << 20;

/// Download `url` and hand each non-blank line to `each`. The bearer token
/// goes with the request only when the manifest says `requiresAccessToken`.
/// `timeout` covers the whole file.
pub fn read_lines(
    client: &FhirClient,
    url: &str,
    authorize: bool,
    timeout: Duration,
    mut each: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    // `each` fails with a PipesError, the download wants an io::Error: the
    // first is kept here and the download stopped with a placeholder.
    let mut failed: Option<PipesError> = None;
    let got = client.download(url, authorize, timeout, |body| {
        // A line is read up to MAX_LINE: a file with no newline would
        // otherwise be held whole.
        let mut lines =
            Splitter::new(std::io::BufReader::with_capacity(1 << 20, body)).with_max_len(MAX_LINE);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let Some(span) = lines.next_into(&mut buf)? else {
                return Ok(());
            };
            let line = std::str::from_utf8(&buf[span.start..span.end])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if let Err(e) = each(line) {
                failed = Some(e);
                return Err(std::io::Error::other("stopped"));
            }
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    got.map_err(|e| {
        PipesError::Environment(format!("downloading {}: {e}", crate::redact::url(url)))
    })
}

/// The type and id a `deleted` Bundle entry removes: its `request.url`,
/// `Patient/123` (an absolute URL or a `_history` suffix is tolerated).
pub fn deleted_entry(entry: &Value) -> Option<(String, String)> {
    let request = entry.get("request")?;
    if !request
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|m| m.eq_ignore_ascii_case("DELETE"))
    {
        return None;
    }
    let url = request.get("url").and_then(Value::as_str)?;
    let path = url.split(['?', '#']).next().unwrap_or("");
    let path = path.split("/_history").next().unwrap_or("");
    let mut parts = path.rsplit('/').filter(|p| !p.is_empty());
    let id = parts.next()?;
    let resource_type = parts.next()?;
    Some((resource_type.to_string(), id.to_string()))
}

/// One OperationOutcome issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: String,
    pub code: String,
    pub text: String,
}

impl Issue {
    /// An error or fatal issue fails the export; a warning or information
    /// does not. A missing severity counts as an error.
    pub fn is_error(&self) -> bool {
        matches!(self.severity.as_str(), "error" | "fatal" | "")
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.severity, self.code, self.text)
    }
}

/// The issues of an OperationOutcome; none for anything else. The text is
/// `diagnostics`, else `details.text`.
pub fn issues(outcome: &Value) -> Vec<Issue> {
    if outcome.get("resourceType").and_then(Value::as_str) != Some("OperationOutcome") {
        return Vec::new();
    }
    let field = |i: &Value, ptr: &str| {
        i.pointer(ptr)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    outcome
        .get("issue")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|i| {
            let mut text = field(i, "/diagnostics");
            if text.is_empty() {
                text = field(i, "/details/text");
            }
            Issue {
                severity: field(i, "/severity"),
                code: field(i, "/code"),
                text,
            }
        })
        .collect()
}

pub fn join_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(Issue::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// An error body's OperationOutcome as text, when it is one.
fn outcome_text(body: &[u8]) -> Option<String> {
    let found = issues(&serde_json::from_slice(body).ok()?);
    (!found.is_empty()).then(|| join_issues(&found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bulk_lines_skip_a_bom_and_blank_lines() {
        use httptest::{Expectation, Server, matchers::*, responders::*};
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/f.ndjson"))
                .respond_with(status_code(200).body("\u{feff}{\"id\":\"a\"}\r\n\n{\"id\":\"b\"}")),
        );
        let client = FhirClient::new(crate::http::Auth::None, 1, Duration::from_secs(5)).unwrap();
        let mut lines = Vec::new();
        read_lines(
            &client,
            &server.url_str("/f.ndjson"),
            false,
            Duration::from_secs(5),
            |l| {
                lines.push(l.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lines, [r#"{"id":"a"}"#, r#"{"id":"b"}"#]);
    }

    #[test]
    fn the_kick_off_lists_the_types_and_encodes_since() {
        let url = kick_off_url(
            "http://h/fhir/",
            &["Observation".into(), "Patient".into()],
            Some("2026-10-01T00:00:00+01:00"),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://h/fhir/$export?_type=Observation%2CPatient&_since=2026-10-01T00%3A00%3A00%2B01%3A00"
        );
        let url = kick_off_url("http://h/fhir", &["Patient".into()], None).unwrap();
        assert_eq!(url.as_str(), "http://h/fhir/$export?_type=Patient");
    }

    #[test]
    fn binary_references_are_read_from_the_fhir_base_with_the_token() {
        let parse = |files: Value| {
            let got = Exchange {
                status: 200,
                headers: Default::default(),
                body: json!({"transactionTime": "t", "output": files})
                    .to_string()
                    .into(),
                truncated: false,
            };
            manifest(&got, "https://m.example/fhir/R4/bulkdata/export/j1").unwrap()
        };
        let mut m = parse(json!([{"type": "Patient", "url": "Binary/b-1"}]));
        assert_eq!(
            m.output[0].url,
            "https://m.example/fhir/R4/bulkdata/export/Binary/b-1"
        );
        read_references_from(&mut m, "https://m.example/fhir/R4/");
        assert_eq!(m.output[0].url, "https://m.example/fhir/R4/Binary/b-1");
        assert!(m.requires_access_token);
        // A URL elsewhere among them: nothing changes.
        let mut m = parse(json!([
            {"type": "Patient", "url": "Binary/b-1"},
            {"type": "Patient", "url": "https://s3.example/b-2?sig=x"}
        ]));
        read_references_from(&mut m, "https://m.example/fhir/R4");
        assert!(m.output[0].url.contains("/bulkdata/export/"));
        assert!(!m.requires_access_token);
        assert!(!is_reference("https://x/Binary/1"));
        assert!(!is_reference("../Binary/1"));
    }

    #[test]
    fn the_watermark_is_the_earlier_of_transaction_time_and_the_kick_off() {
        let tt = "2026-10-15T08:00:00Z";
        assert_eq!(
            watermark(tt, Some("2026-10-15T07:30:00Z")),
            "2026-10-15T07:30:00Z"
        );
        assert_eq!(
            watermark(tt, Some("2026-10-15T09:00:00+02:00")),
            "2026-10-15T09:00:00+02:00"
        );
        assert_eq!(watermark(tt, Some("2026-10-15T08:30:00Z")), tt);
        assert_eq!(watermark(tt, Some("not a time")), tt);
        assert_eq!(watermark(tt, None), tt);
    }

    #[test]
    fn a_deleted_entry_names_the_type_and_id() {
        let e = |url: &str| json!({"request": {"method": "DELETE", "url": url}});
        let got = |url| deleted_entry(&e(url));
        let want = Some(("Patient".to_string(), "123".to_string()));
        assert_eq!(got("Patient/123"), want);
        assert_eq!(got("http://h/fhir/Patient/123"), want);
        assert_eq!(got("Patient/123/_history/4"), want);
        assert_eq!(got("123"), None);
        assert_eq!(
            deleted_entry(&json!({"request": {"method": "PUT", "url": "Patient/1"}})),
            None
        );
    }

    #[test]
    fn issues_are_read_from_diagnostics_or_details() {
        let o = json!({"resourceType": "OperationOutcome", "issue": [
            {"severity": "error", "code": "processing", "diagnostics": "no such type"},
            {"severity": "warning", "code": "informational", "details": {"text": "slow"}},
        ]});
        let found = issues(&o);
        assert_eq!(found.len(), 2);
        assert!(found[0].is_error());
        assert!(!found[1].is_error());
        assert_eq!(
            join_issues(&found),
            "error processing: no such type; warning informational: slow"
        );
        assert!(issues(&json!({"resourceType": "Bundle"})).is_empty());
    }

    #[test]
    fn a_202_waits_at_most_fifteen_seconds_and_a_429_as_long_as_it_asks() {
        let s = Duration::from_secs;
        let mut backoff = POLL_FIRST;
        // HAPI's fixed Retry-After on a running export.
        assert_eq!(next_wait(202, Some(s(120)), &mut backoff), s(15));
        assert_eq!(next_wait(202, Some(s(5)), &mut backoff), s(5));
        assert_eq!(next_wait(202, Some(s(0)), &mut backoff), POLL_MIN);
        assert_eq!(next_wait(429, Some(s(120)), &mut backoff), s(120));
        assert_eq!(next_wait(503, Some(s(120)), &mut backoff), s(120));
        assert_eq!(
            backoff, POLL_FIRST,
            "a Retry-After leaves the backoff alone"
        );
        // Without one: the backoff, capped after a 202 only.
        let waits: Vec<Duration> = (0..8).map(|_| next_wait(202, None, &mut backoff)).collect();
        assert_eq!(
            waits,
            [1, 2, 4, 8, 15, 15, 15, 15].map(s),
            "the backoff doubles to a minute, a 202 waits at most 15 s"
        );
        assert_eq!(next_wait(429, None, &mut backoff), POLL_MAX);
    }

    #[test]
    fn a_huge_retry_after_waits_until_the_deadline_without_panicking() {
        use httptest::{Expectation, Server, matchers::*, responders::*};
        // `Duration::from_secs_f64(1e300)` panics; the shared parser clamps.
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/status"),
                request::headers(contains(("cache-control", "no-cache"))),
            ])
            .times(1)
            .respond_with(status_code(202).insert_header("Retry-After", "1e300")),
        );
        let client = FhirClient::new(crate::http::Auth::None, 1, Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        let got = wait(
            &client,
            &server.url_str("/status"),
            started + Duration::from_millis(500),
        )
        .unwrap();
        assert!(matches!(got, Polled::TimedOut), "{got:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
