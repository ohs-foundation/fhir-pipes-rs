//! The Bulk Data `$export` protocol (FHIR Bulk Data Access 2.0) at the system
//! level: the kick-off, polling the status URL until the export is done, and
//! reading the manifest's files line by line as they download. What
//! `export --bulk` does with the lines is in the parent module.

use std::io::BufRead;
use std::time::{Duration, Instant, SystemTime};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::error::{PipesError, Result};
use crate::extract::client::{Exchange, FetchError, FhirClient, parse_retry_after};

/// The longest failure text shown from a status response.
const DISPLAY_CAP: usize = 500;

/// The first wait when the status URL gives no Retry-After; it doubles up to
/// POLL_MAX on each answer without one.
const POLL_FIRST: Duration = Duration::from_secs(1);
const POLL_MAX: Duration = Duration::from_secs(60);
/// The shortest wait between polls, whatever the server's Retry-After says.
const POLL_MIN: Duration = Duration::from_secs(1);

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
    pub started_at: String,
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

#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub url: String,
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
    let base = server.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/$export"))
        .map_err(|_| PipesError::Usage(format!("--server is not a valid URL: {server}")))?;
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
/// `Content-Location`, resolved against the kick-off URL).
pub fn kick_off(
    client: &FhirClient,
    server: &str,
    types: &[String],
    since: Option<&str>,
) -> Result<String> {
    let url = kick_off_url(server, types, since)?;
    let got = client.get_async(url.as_str()).map_err(|e| match e {
        FetchError::Status { status, body } => PipesError::Environment(format!(
            "the bulk export kick-off {url} failed with HTTP {status}: {}",
            outcome_text(body.as_bytes()).unwrap_or(body)
        )),
        other => PipesError::Environment(format!("{url}: {other}")),
    })?;
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
    Ok(status_url.to_string())
}

/// Poll `status_url` until the export is done, has failed, or `deadline`
/// passes. 202 and 429 mean "not yet": the wait is the server's Retry-After,
/// or a backoff from POLL_FIRST doubling to POLL_MAX without one. Each new
/// X-Progress is printed. An error status with a `transient` issue, or a 5xx
/// that is not an OperationOutcome (a gateway's), is waited out the same way.
/// A connection that fails after the client's retries is an error.
pub fn wait(client: &FhirClient, status_url: &str, deadline: Instant) -> Result<Polled> {
    let mut backoff = POLL_FIRST;
    let mut progress: Option<String> = None;
    loop {
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
                retry_after(&got)
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
                retry_after(&got)
            }
        };
        let delay = retry_after
            .unwrap_or_else(|| {
                let d = backoff;
                backoff = (backoff * 2).min(POLL_MAX);
                d
            })
            .max(POLL_MIN);
        let now = Instant::now();
        if now >= deadline {
            return Ok(Polled::TimedOut);
        }
        // The loop checks the deadline again before the next poll.
        std::thread::sleep(delay.min(deadline - now));
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
    for f in m
        .output
        .iter_mut()
        .chain(&mut m.deleted)
        .chain(&mut m.error)
    {
        f.url = base
            .join(&f.url)
            .map_err(|_| PipesError::Environment(format!("{status_url}: {} is not a URL", f.url)))?
            .to_string();
    }
    Ok(m)
}

/// The Retry-After header as a wait: seconds, or an HTTP date.
fn retry_after(got: &Exchange) -> Option<Duration> {
    let value = got.header("retry-after")?;
    if let Some(secs) = parse_retry_after(Some(value)) {
        return Some(Duration::from_secs_f64(secs.max(0.0)));
    }
    let at = httpdate::parse_http_date(value.trim()).ok()?;
    Some(at.duration_since(SystemTime::now()).unwrap_or_default())
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
        let reader = std::io::BufReader::with_capacity(1 << 20, body);
        for line in reader.lines() {
            let line = line?;
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            if let Err(e) = each(line) {
                failed = Some(e);
                return Err(std::io::Error::other("stopped"));
            }
        }
        Ok(())
    });
    if let Some(e) = failed {
        return Err(e);
    }
    got.map_err(|e| PipesError::Environment(format!("downloading {url}: {e}")))
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
}
