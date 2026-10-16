//! Phase two: resolve the distinct boundary URLs on a worker pool into the
//! cache (or memory with --no-cache). Workers only fetch and write cache
//! entries; the main thread consumes outcomes in submission order so the
//! counters and any reports are deterministic. The results channel is
//! bounded (`workers * 4`), so the `pending` reorder buffer is bounded by
//! that capacity plus one in-flight item per worker, not by the worker
//! count alone.
//!
//! A worker panicking (as opposed to a fetch merely failing) unwinds out of
//! `thread::scope` and propagates to the caller of `fetch_boundaries`; that
//! is deliberate; a panic is a bug in this module, not a per-URL data
//! condition, and should not be swallowed into a `Failed` outcome.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use base64::Engine;

use crate::error::{PipesError, Result};
use crate::extract::cache::Cache;
use crate::extract::client::FhirClient;
use crate::report::Report;

const PROGRESS_MIN_ITEMS: usize = 50;

pub struct FetchOptions {
    pub concurrency: usize,
    /// None = --no-cache: bytes are kept in `FetchSummary::in_memory`.
    pub cache: Option<Cache>,
    pub refresh: bool,
    /// 0 disables the breaker.
    pub max_consecutive_failures: usize,
}

#[derive(Debug)]
enum Outcome {
    Cached,
    /// Fetched; `Some(bytes)` when they must be kept in memory (no cache, or the cache write failed).
    Fetched(Option<Vec<u8>>),
    Failed(String),
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub cached: usize,
    pub fetched: usize,
    pub failed: usize,
    /// url -> failure detail, for the merge to report per resource.
    pub failures: HashMap<String, String>,
    /// url -> bytes, for --no-cache or when a cache write failed.
    pub in_memory: HashMap<String, Vec<u8>>,
}

/// Cheap pre-check before paying for a full JSON parse: only bodies that
/// start with `{` (ignoring leading whitespace) and mention `"resourceType"`
/// within their first 4 KiB can possibly be a FHIR Binary. This lets large
/// non-JSON attachments (images, big GeoJSON blobs) skip the parse entirely.
fn looks_like_binary_json(body: &[u8]) -> bool {
    if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return false;
    }
    const NEEDLE: &[u8] = b"\"resourceType\"";
    let scan = &body[..body.len().min(4096)];
    scan.len() >= NEEDLE.len() && scan.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

/// A 200 body: a FHIR Binary is base64-decoded; anything else is the bytes.
/// xs:base64Binary permits line wrapping, so whitespace is stripped from
/// `data` before decoding.
pub fn extract_payload(body: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if looks_like_binary_json(body)
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(body)
        && v.get("resourceType").and_then(|t| t.as_str()) == Some("Binary")
    {
        let data = v
            .get("data")
            .and_then(|d| d.as_str())
            .filter(|d| !d.is_empty())
            .ok_or("Binary has no data")?;
        let cleaned: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        return base64::engine::general_purpose::STANDARD
            .decode(&cleaned)
            .map_err(|e| format!("Binary data is not valid base64: {e}"));
    }
    Ok(body.to_vec())
}

/// GET a boundary URL and take its payload (see `extract_payload`).
fn fetch_payload(client: &FhirClient, url: &str) -> std::result::Result<Vec<u8>, String> {
    let body = client.get(url).map_err(|e| e.to_string())?.body;
    extract_payload(&body)
}

/// A cached entry is taken from its meta file, without reading and
/// hashing the bytes here: the merge reads them once, with
/// `read_for_merge`.
fn resolve_one(
    client: &FhirClient,
    url: &str,
    opts: &FetchOptions,
    cache_errors: &mut Vec<String>,
) -> Outcome {
    if let (Some(cache), false) = (&opts.cache, opts.refresh) {
        match cache.contains(url) {
            Ok(true) => return Outcome::Cached,
            Ok(false) => {}
            Err(e) => cache_errors.push(e),
        }
    }
    let bytes = match fetch_payload(client, url) {
        Ok(b) => b,
        Err(e) => return Outcome::Failed(e),
    };
    match &opts.cache {
        Some(cache) => match cache.write(url, &bytes) {
            Ok(()) => Outcome::Fetched(None),
            Err(e) => {
                cache_errors.push(e);
                Outcome::Fetched(Some(bytes))
            }
        },
        None => Outcome::Fetched(Some(bytes)),
    }
}

/// The cached bytes of `url` for the merge. An entry that fails its
/// sha256 check (damaged after `fetch_boundaries` found it) is fetched
/// again and written back, as the fetch phase would have done; the error
/// comes back too, for the report's `cache_error`.
pub fn read_for_merge(
    client: &FhirClient,
    cache: &Cache,
    url: &str,
) -> (Option<Vec<u8>>, Option<String>) {
    let damaged = match cache.read(url) {
        Ok(found) => return (found, None),
        Err(e) => e,
    };
    match fetch_payload(client, url) {
        Ok(bytes) => {
            let rewritten = match cache.write(url, &bytes) {
                Ok(()) => "fetched again".to_string(),
                Err(e) => format!("fetched again; writing it back: {e}"),
            };
            (Some(bytes), Some(format!("{damaged}; {rewritten}")))
        }
        Err(e) => (None, Some(format!("{damaged}; fetching it again: {e}"))),
    }
}

pub fn fetch_boundaries(
    client: &FhirClient,
    urls: &[String],
    opts: &FetchOptions,
    report: &mut Report,
) -> Result<FetchSummary> {
    let total = urls.len();
    let workers = opts.concurrency.max(1).min(total.max(1));
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::sync_channel::<(usize, Outcome, Vec<String>)>(workers * 4);

    let mut summary = FetchSummary::default();
    let interval = (total / 20).max(1);
    let mut consecutive = 0usize;
    let mut any_success = false;
    let mut pending: HashMap<usize, (Outcome, Vec<String>)> = HashMap::new();
    let mut cursor = 0usize;
    let mut breaker: Option<String> = None;

    std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let stop = &stop;
            s.spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    let mut errs = Vec::new();
                    let outcome = resolve_one(client, &urls[i], opts, &mut errs);
                    if tx.send((i, outcome, errs)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, outcome, errs) in rx.iter() {
            pending.insert(i, (outcome, errs));
            while let Some((outcome, errs)) = pending.remove(&cursor) {
                let url = &urls[cursor];
                for e in errs {
                    report.add("cache_error", url, &e);
                }
                match outcome {
                    Outcome::Cached => {
                        summary.cached += 1;
                        any_success = true;
                        consecutive = 0;
                    }
                    Outcome::Fetched(bytes) => {
                        summary.fetched += 1;
                        any_success = true;
                        consecutive = 0;
                        if let Some(b) = bytes {
                            summary.in_memory.insert(url.clone(), b);
                        }
                    }
                    Outcome::Failed(detail) => {
                        summary.failed += 1;
                        consecutive += 1;
                        summary.failures.insert(url.clone(), detail.clone());
                        if opts.max_consecutive_failures > 0
                            && !any_success
                            && consecutive >= opts.max_consecutive_failures
                            && breaker.is_none()
                        {
                            stop.store(true, Ordering::Relaxed);
                            breaker = Some(format!(
                                "{consecutive} consecutive boundary fetches failed with zero successes; this looks like a systematic problem (server unreachable, wrong token, or a bad base URL), not a handful of bad boundary URLs. Last failure: {detail}. Pass --max-consecutive-failures 0 to disable this check."
                            ));
                        }
                    }
                }
                cursor += 1;
                if total >= PROGRESS_MIN_ITEMS
                    && (cursor.is_multiple_of(interval) || cursor == total)
                {
                    eprintln!(
                        "resolved {cursor}/{total} boundaries ({} failed)",
                        summary.failed
                    );
                }
            }
            if breaker.is_some() {
                break;
            }
        }
        // Drain whatever the still-running workers send after the trip so they can exit.
        if breaker.is_some() {
            for _ in rx.iter() {}
        }
    });

    eprintln!(
        "boundaries: {} cached, {} fetched, {} failed",
        summary.cached, summary.fetched, summary.failed
    );
    match breaker {
        Some(msg) => Err(PipesError::Environment(msg)),
        None => Ok(summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::cache::Cache;
    use crate::extract::client::{Auth, FhirClient};
    use crate::report::Report;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    fn client(retries: usize) -> FhirClient {
        FhirClient::new(Auth::None, retries, std::time::Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn binary_resources_are_decoded_and_raw_bodies_pass_through() {
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"{\"type\":\"Point\"}",
        );
        let bin = format!(
            r#"{{"resourceType":"Binary","contentType":"application/geo+json","data":"{b64}"}}"#
        );
        assert_eq!(
            extract_payload(bin.as_bytes()).unwrap(),
            b"{\"type\":\"Point\"}"
        );
        assert_eq!(
            extract_payload(b"{\"type\":\"Polygon\"}").unwrap(),
            b"{\"type\":\"Polygon\"}"
        );
        assert_eq!(
            extract_payload(b"not json at all").unwrap(),
            b"not json at all"
        );
        assert!(
            extract_payload(br#"{"resourceType":"Binary"}"#)
                .unwrap_err()
                .contains("no data")
        );
        assert!(
            extract_payload(br#"{"resourceType":"Binary","data":"!!"}"#)
                .unwrap_err()
                .contains("base64")
        );
    }

    #[test]
    fn fetches_into_the_cache_and_counts_in_order() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/ok"))
                .times(1)
                .respond_with(status_code(200).body("{\"type\":\"Point\"}")),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/missing"))
                .times(1)
                .respond_with(status_code(404)),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        cache
            .write(&server.url("/cached").to_string(), b"c")
            .unwrap();
        let urls = vec![
            server.url("/ok").to_string(),
            server.url("/missing").to_string(),
            server.url("/cached").to_string(),
        ];
        let mut report = Report::default();
        let opts = FetchOptions {
            concurrency: 2,
            cache: Some(cache.clone()),
            refresh: false,
            max_consecutive_failures: 50,
        };
        let summary = fetch_boundaries(&client(1), &urls, &opts, &mut report).unwrap();
        assert_eq!((summary.cached, summary.fetched, summary.failed), (1, 1, 1));
        assert_eq!(
            cache.read(&urls[0]).unwrap(),
            Some(b"{\"type\":\"Point\"}".to_vec())
        );
        assert!(summary.failures[&urls[1]].contains("404"));
        assert!(summary.in_memory.is_empty());
        assert_eq!(
            report.count("boundary_fetch_failed"),
            0,
            "failures are reported per resource by the merge, not here"
        );
    }

    #[test]
    fn refresh_skips_cache_reads_but_writes_back() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/b"))
                .times(1)
                .respond_with(status_code(200).body("new")),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let url = server.url("/b").to_string();
        cache.write(&url, b"old").unwrap();
        let opts = FetchOptions {
            concurrency: 1,
            cache: Some(cache.clone()),
            refresh: true,
            max_consecutive_failures: 0,
        };
        let summary = fetch_boundaries(
            &client(1),
            std::slice::from_ref(&url),
            &opts,
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!((summary.cached, summary.fetched), (0, 1));
        assert_eq!(cache.read(&url).unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn no_cache_keeps_bytes_in_memory() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/ok"))
                .respond_with(status_code(200).body("g")),
        );
        let urls = vec![server.url("/ok").to_string()];
        let opts = FetchOptions {
            concurrency: 1,
            cache: None,
            refresh: false,
            max_consecutive_failures: 0,
        };
        let summary = fetch_boundaries(&client(1), &urls, &opts, &mut Report::default()).unwrap();
        assert_eq!(
            summary.in_memory.get(&urls[0]).map(|v| v.as_slice()),
            Some(&b"g"[..])
        );
    }

    #[test]
    fn a_cached_entry_is_read_once_and_a_damaged_one_fetched_again_for_the_merge() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/d"))
                .times(1)
                .respond_with(status_code(200).body("fresh")),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let url = server.url("/d").to_string();
        cache.write(&url, b"x").unwrap();
        std::fs::write(
            dir.path()
                .join(format!("{}.bin", crate::extract::cache::cache_key(&url))),
            b"tampered",
        )
        .unwrap();
        let mut report = Report::default();
        let opts = FetchOptions {
            concurrency: 1,
            cache: Some(cache.clone()),
            refresh: false,
            max_consecutive_failures: 0,
        };
        let summary =
            fetch_boundaries(&client(1), std::slice::from_ref(&url), &opts, &mut report).unwrap();
        assert_eq!(
            (summary.cached, summary.fetched),
            (1, 0),
            "from the meta file"
        );
        let (bytes, error) = read_for_merge(&client(1), &cache, &url);
        assert_eq!(bytes.as_deref(), Some(&b"fresh"[..]));
        let error = error.unwrap();
        assert!(
            error.contains("sha256") && error.contains("fetched again"),
            "{error}"
        );
        assert_eq!(
            cache.read(&url).unwrap(),
            Some(b"fresh".to_vec()),
            "rewritten"
        );
        assert_eq!(
            read_for_merge(&client(1), &cache, &url),
            (Some(b"fresh".to_vec()), None)
        );
    }

    #[test]
    fn a_cache_entry_without_its_bin_is_reported_and_refetched() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/c"))
                .times(1)
                .respond_with(status_code(200).body("fresh")),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let url = server.url("/c").to_string();
        cache.write(&url, b"x").unwrap();
        std::fs::remove_file(
            dir.path()
                .join(format!("{}.bin", crate::extract::cache::cache_key(&url))),
        )
        .unwrap();
        let mut report = Report::default();
        let opts = FetchOptions {
            concurrency: 1,
            cache: Some(cache.clone()),
            refresh: false,
            max_consecutive_failures: 0,
        };
        let summary =
            fetch_boundaries(&client(1), std::slice::from_ref(&url), &opts, &mut report).unwrap();
        assert_eq!(summary.fetched, 1);
        assert_eq!(report.count("cache_error"), 1);
        assert_eq!(
            cache.read(&url).unwrap(),
            Some(b"fresh".to_vec()),
            "rewritten"
        );
    }

    #[test]
    fn breaker_trips_on_consecutive_failures_with_no_success() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/dead"))
                .times(..)
                .respond_with(status_code(500)),
        );
        let urls: Vec<String> = (0..10)
            .map(|i| server.url(&format!("/dead?i={i}")).to_string())
            .collect();
        let opts = FetchOptions {
            concurrency: 2,
            cache: None,
            refresh: false,
            max_consecutive_failures: 3,
        };
        let err = fetch_boundaries(&client(1), &urls, &opts, &mut Report::default()).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains("--max-consecutive-failures"),
            "{err}"
        );
    }

    #[test]
    fn breaker_does_not_trip_after_one_success_and_zero_disables_it() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/ok"))
                .respond_with(status_code(200).body("g")),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/dead"))
                .times(..)
                .respond_with(status_code(500)),
        );
        let mut urls = vec![server.url("/ok").to_string()];
        urls.extend((0..5).map(|i| server.url(&format!("/dead?i={i}")).to_string()));
        let opts = FetchOptions {
            concurrency: 1,
            cache: None,
            refresh: false,
            max_consecutive_failures: 2,
        };
        let summary = fetch_boundaries(&client(1), &urls, &opts, &mut Report::default()).unwrap();
        assert_eq!((summary.fetched, summary.failed), (1, 5));
        let dead: Vec<String> = (0..5)
            .map(|i| server.url(&format!("/dead?i={i}")).to_string())
            .collect();
        let opts = FetchOptions {
            concurrency: 2,
            cache: None,
            refresh: false,
            max_consecutive_failures: 0,
        };
        let summary = fetch_boundaries(&client(1), &dead, &opts, &mut Report::default()).unwrap();
        assert_eq!(summary.failed, 5);
    }

    #[test]
    fn empty_work_list_is_fine() {
        let opts = FetchOptions {
            concurrency: 4,
            cache: None,
            refresh: false,
            max_consecutive_failures: 5,
        };
        let summary = fetch_boundaries(&client(1), &[], &opts, &mut Report::default()).unwrap();
        assert_eq!((summary.cached, summary.fetched, summary.failed), (0, 0, 0));
    }

    /// A slow-to-fail head-of-line URL must not let fast trailing outcomes
    /// (including a success) be counted ahead of it: the breaker's
    /// consecutive-failure count is driven by submission order, not receipt
    /// order. Run several times since the whole point is to survive
    /// scheduling variance.
    #[test]
    fn outcomes_are_applied_in_submission_order() {
        for _ in 0..5 {
            let server = Server::run();
            server.expect(
                Expectation::matching(request::method_path("GET", "/slow"))
                    .times(1)
                    .respond_with(delay_and_then(
                        std::time::Duration::from_millis(300),
                        status_code(500),
                    )),
            );
            server.expect(
                Expectation::matching(request::method_path("GET", "/fast1"))
                    .times(1)
                    .respond_with(status_code(500)),
            );
            server.expect(
                Expectation::matching(request::method_path("GET", "/fast2"))
                    .times(1)
                    .respond_with(status_code(500)),
            );
            server.expect(
                Expectation::matching(request::method_path("GET", "/fast3"))
                    .times(1)
                    .respond_with(status_code(200).body("g")),
            );
            let urls = vec![
                server.url("/slow").to_string(),
                server.url("/fast1").to_string(),
                server.url("/fast2").to_string(),
                server.url("/fast3").to_string(),
            ];
            let opts = FetchOptions {
                concurrency: 4,
                cache: None,
                refresh: false,
                max_consecutive_failures: 3,
            };
            let err =
                fetch_boundaries(&client(1), &urls, &opts, &mut Report::default()).unwrap_err();
            assert!(
                err.to_string().contains("--max-consecutive-failures"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_failed_cache_write_keeps_bytes_in_memory() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/x"))
                .times(1)
                .respond_with(status_code(200).body("payload")),
        );
        // An existing regular file where the cache expects a directory: writes fail.
        let file = tempfile::NamedTempFile::new().unwrap();
        let cache = Cache::new(file.path());
        let url = server.url("/x").to_string();
        let mut report = Report::default();
        // refresh: true isolates the write failure -- with a cache dir that
        // is really a file, a read would fail too, which is not what this
        // test is about.
        let opts = FetchOptions {
            concurrency: 1,
            cache: Some(cache),
            refresh: true,
            max_consecutive_failures: 0,
        };
        let summary =
            fetch_boundaries(&client(1), std::slice::from_ref(&url), &opts, &mut report).unwrap();
        assert_eq!(summary.fetched, 1);
        assert_eq!(
            summary.in_memory.get(&url).map(|v| v.as_slice()),
            Some(&b"payload"[..])
        );
        assert_eq!(report.count("cache_error"), 1);
    }

    #[test]
    fn breaker_stops_workers_early() {
        let server = Server::run();
        let requested = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A small per-request delay gives the consumer thread a real (if
        // brief) window to observe failures and flip `stop` before the
        // worker pool could otherwise race through every URL; without it
        // this test is flaky under CPU contention (e.g. the full suite
        // running in parallel), since 10 in-process loopback requests can
        // complete before the consumer is ever scheduled.
        let counted_responder = {
            let requested = requested.clone();
            move || {
                requested.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                status_code(500)
            }
        };
        server.expect(
            Expectation::matching(request::method_path("GET", "/dead"))
                .times(..)
                .respond_with(counted_responder),
        );
        let urls: Vec<String> = (0..10)
            .map(|i| server.url(&format!("/dead?i={i}")).to_string())
            .collect();
        let opts = FetchOptions {
            concurrency: 2,
            cache: None,
            refresh: false,
            max_consecutive_failures: 3,
        };
        fetch_boundaries(&client(1), &urls, &opts, &mut Report::default()).unwrap_err();
        let seen = requested.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            seen < 10,
            "breaker should have stopped workers early, but the server saw all {seen} of 10 requests"
        );
    }

    #[test]
    fn line_wrapped_binary_data_decodes() {
        let payload = b"{\"type\":\"Point\"}";
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload);
        // xs:base64Binary permits line wrapping; simulate a server that wraps
        // at 8 chars using an *escaped* newline so the JSON stays valid --
        // once parsed, the `data` string itself contains real newlines.
        let wrapped: String = b64
            .as_bytes()
            .chunks(8)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\\n");
        let bin = format!(r#"{{"resourceType":"Binary","data":"{wrapped}"}}"#);
        assert_eq!(extract_payload(bin.as_bytes()).unwrap(), payload);
    }
}
