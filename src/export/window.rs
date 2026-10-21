//! A full download of one large type as `_lastUpdated` windows paged side by
//! side (`export --concurrency`).
//!
//! The oldest and newest meta.lastUpdated (`_sort=_lastUpdated&_count=1`, and
//! descending) give the range, which is cut at whole seconds into windows
//! searched with `_lastUpdated=ge<from>&_lastUpdated=lt<to>`. The first window
//! has no lower bound and the last no upper bound, so a resource outside the
//! probed range (one updated during the run) is still in a window, and a
//! server that ignores `_sort` only makes the windows uneven. A resource
//! stamped exactly at a boundary is in the window that starts there (`ge`),
//! not the one that ends there (`lt`).
//!
//! Each window is counted, and one holding more than its share is halved
//! until the windows are even or a second wide. The counts must add up to the
//! type's, before the halving and after it; when they do not, the server is
//! not filtering by `_lastUpdated` as asked, and the caller pages the type as
//! one stream. A type count that may be an estimate (Medplum's, from a million
//! on) is not compared. A window holding more than `max_window` (the server
//! kind's) is halved past the usual number of windows, for as long as its
//! halves count up to it and it is two seconds wide or more, so the limit is
//! a target: one second's resources are never split.
//!
//! Each window is paged into its own file. A window whose paged count is not
//! its count is counted and paged again; a second difference is reported as
//! `count_mismatch`. The files are then joined in window order, oldest
//! first, each in the server's order, keeping the last copy of an id: a
//! resource updated while the windows were paged is in a later window, with
//! its newer version.

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::extract::page::{
    PAGE_SIZE, PageNote, PageResult, ServerKind, count_matching, count_resources, page_search,
    server_url,
};
use crate::fhir::ndjson::NdjsonReader;
use crate::http::FhirClient;
use crate::report::Report;
use crate::snapshot::instant::{format_utc, parse_instant};

/// A type with fewer resources than this is paged as one stream: windows
/// cost two probes and a count each, worth it only over many pages.
pub const MIN_RESOURCES: u64 = 10 * PAGE_SIZE as u64;

/// How many times a window is paged before a count difference is reported.
const ATTEMPTS: usize = 2;

/// What to download, and how.
pub struct Options<'a> {
    pub server: &'a str,
    pub kind: ServerKind,
    /// The most resources a window should hold: the kind's `max_window`.
    pub max_window: Option<u64>,
    pub resource_type: &'a str,
    /// The type's count, from the server.
    pub total: u64,
    /// Requests in flight at once, and the number of windows to start with.
    pub workers: usize,
    /// Where the window files are written while they are paged.
    pub dir: &'a Path,
}

pub enum Windowed {
    /// Every window paged and joined into the output file.
    Done { paged: PageResult, windows: usize },
    /// The type cannot be windowed on this server, and why.
    Unsuitable(String),
}

/// A `_lastUpdated` window in whole seconds since the epoch: `from`
/// included, `to` not; `None` is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    from: Option<i64>,
    to: Option<i64>,
}

impl Window {
    fn params(&self) -> Vec<(&'static str, String)> {
        let mut params = Vec::new();
        if let Some(from) = self.from {
            params.push(("_lastUpdated", format!("ge{}", instant(from))));
        }
        if let Some(to) = self.to {
            params.push(("_lastUpdated", format!("lt{}", instant(to))));
        }
        params
    }

    fn describe(&self) -> String {
        match (self.from, self.to) {
            (Some(from), Some(to)) => format!("from {} to {}", instant(from), instant(to)),
            (Some(from), None) => format!("from {}", instant(from)),
            (None, Some(to)) => format!("before {}", instant(to)),
            (None, None) => "at any time".to_string(),
        }
    }

    /// The seconds it covers, with the open ends at the probed range's.
    fn span(&self, lo: i64, hi: i64) -> (i64, i64) {
        (self.from.unwrap_or(lo), self.to.unwrap_or(hi))
    }
}

fn instant(secs: i64) -> String {
    format_utc(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

/// `n` windows of about equal time over [lo, hi), the first and last open.
fn split(lo: i64, hi: i64, n: usize) -> Vec<Window> {
    let mut cuts: Vec<i64> = (1..n as i64)
        .map(|k| lo + (hi - lo) * k / n as i64)
        .filter(|c| *c > lo && *c < hi)
        .collect();
    cuts.dedup();
    let mut windows = Vec::with_capacity(cuts.len() + 1);
    let mut from = None;
    for cut in cuts {
        windows.push(Window {
            from,
            to: Some(cut),
        });
        from = Some(cut);
    }
    windows.push(Window { from, to: None });
    windows
}

/// Page `o.resource_type` as windows into `out`.
pub fn download(
    client: &FhirClient,
    o: &Options,
    out: &Path,
    report: &mut Report,
) -> Result<Windowed> {
    let (lo, hi, started) = match probe(client, o.server, o.kind, o.resource_type) {
        Ok(range) => range,
        Err(why) => return Ok(Windowed::Unsuitable(why)),
    };
    if hi - lo < 2 {
        return Ok(Windowed::Unsuitable(
            "its resources were all updated within one second".to_string(),
        ));
    }
    let workers = o.workers.max(2);
    let mut windows = split(lo, hi, workers);
    let Some(mut counts) = count_all(client, o, &windows) else {
        return Ok(no_count());
    };
    // The windows' counts must add up to the type's (counted again before
    // giving up, in case resources were added since), here and after the
    // halving. A count that may be an estimate is not compared.
    let unsuitable = |counts: &[u64]| -> Option<Windowed> {
        let sum: u64 = counts.iter().sum();
        (sum != o.total
            && !o.kind.count_is_estimate(o.total)
            && count_resources(client, o.server, o.resource_type) != Some(sum))
        .then(|| {
            Windowed::Unsuitable(format!(
                "its _lastUpdated windows count {sum} resources but the server counts {} in all",
                o.total
            ))
        })
    };
    // Checked before any halving, so a server that does not filter by
    // _lastUpdated is found with the first counts.
    if let Some(why) = unsuitable(&counts) {
        return Ok(why);
    }

    // Halve the windows holding more than their share, largest first, so
    // updates bunched in time do not leave one window with most of the type.
    // Up to `most` windows, except that one over the kind's limit is halved
    // past that, for as long as each window's halves add up to it.
    let share = (o.total / (2 * workers as u64)).max(PAGE_SIZE as u64);
    let most = 8 * workers;
    let over = |c: u64| o.max_window.is_some_and(|m| c > m);
    loop {
        let mut big: Vec<usize> = (0..windows.len())
            .filter(|&i| {
                let (a, b) = windows[i].span(lo, hi);
                (counts[i] > share || over(counts[i])) && b - a >= 2
            })
            .collect();
        big.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
        let room = most.saturating_sub(windows.len());
        let forced = big.iter().filter(|&&i| over(counts[i])).count();
        big.truncate(room.max(forced));
        if big.is_empty() {
            break;
        }
        let halves: Vec<Window> = big
            .iter()
            .flat_map(|&i| {
                let w = windows[i];
                let (a, b) = w.span(lo, hi);
                let mid = a + (b - a) / 2;
                [
                    Window {
                        from: w.from,
                        to: Some(mid),
                    },
                    Window {
                        from: Some(mid),
                        to: w.to,
                    },
                ]
            })
            .collect();
        let Some(half_counts) = count_all(client, o, &halves) else {
            return Ok(no_count());
        };
        // Past `most`, halves that do not add up to their window mean the
        // server's counts cannot be trusted to shrink: stop halving, and
        // leave it to the check below.
        if big.len() > room
            && big
                .iter()
                .enumerate()
                .any(|(k, &i)| half_counts[2 * k] + half_counts[2 * k + 1] != counts[i])
        {
            break;
        }
        let at: HashMap<usize, usize> = big.iter().enumerate().map(|(k, &i)| (i, k)).collect();
        let (mut next_windows, mut next_counts) = (Vec::new(), Vec::new());
        for (i, (w, c)) in windows.iter().zip(&counts).enumerate() {
            match at.get(&i) {
                Some(&k) => {
                    next_windows.extend_from_slice(&halves[2 * k..2 * k + 2]);
                    next_counts.extend_from_slice(&half_counts[2 * k..2 * k + 2]);
                }
                None => {
                    next_windows.push(*w);
                    next_counts.push(*c);
                }
            }
        }
        (windows, counts) = (next_windows, next_counts);
    }

    if let Some(why) = unsuitable(&counts) {
        return Ok(why);
    }
    // An empty window is left out, but never the last: a resource updated
    // during the run lands there.
    let last = windows.len() - 1;
    let jobs: Vec<(Window, u64)> = windows
        .into_iter()
        .zip(counts)
        .enumerate()
        .filter(|(i, (_, c))| *c > 0 || *i == last)
        .map(|(_, job)| job)
        .collect();
    let files = crate::fs::RemoveOnDrop(
        (0..jobs.len())
            .map(|i| {
                o.dir
                    .join(format!(".{}.window-{i}.ndjson", o.resource_type))
            })
            .collect(),
    );

    // The largest windows first, so the last to finish is a small one.
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(jobs[i].1));
    let failed = AtomicBool::new(false);
    let results = parallel(&order, workers, |&i| {
        if failed.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let paged = page_window(client, o, jobs[i], &files.0[i]);
        if paged.is_err() {
            failed.store(true, Ordering::Relaxed);
        }
        paged.map(Some)
    });
    let mut paged: Vec<Option<Paged>> = (0..jobs.len()).map(|_| None).collect();
    for (&i, result) in order.iter().zip(results) {
        if let Some(p) = result? {
            paged[i] = Some(p);
        }
    }
    let paged: Vec<Paged> = paged
        .into_iter()
        .map(|p| p.expect("every window was paged"))
        .collect();

    let notes = join(&files.0, &paged, out)?;
    let pages = paged.iter().map(|p| p.pages).sum();
    for p in paged {
        report.append(p.report);
    }
    Ok(Windowed::Done {
        paged: PageResult {
            notes,
            pages,
            // Each window's count was checked.
            last_page_full: false,
            started,
        },
        windows: jobs.len(),
    })
}

fn no_count() -> Windowed {
    Windowed::Unsuitable("the server gave no count for a _lastUpdated window".to_string())
}

/// The first and last second of the type's meta.lastUpdated, the second as
/// an exclusive bound, and the server's `Date` on the first probe. The
/// kind's page parameters are sent too, other than its `_sort`.
fn probe(
    client: &FhirClient,
    server: &str,
    kind: ServerKind,
    resource_type: &str,
) -> std::result::Result<(i64, i64, Option<String>), String> {
    let first = |sort: &str| -> std::result::Result<(i64, Option<String>), String> {
        let mut url = server_url(server, resource_type).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("_sort", sort)
            .append_pair("_count", "1")
            .extend_pairs(kind.page_params().iter().filter(|(k, _)| *k != "_sort"));
        let fetched = client
            .get(url.as_str())
            .map_err(|e| format!("the server did not sort by _lastUpdated ({e})"))?;
        let bundle: Value = serde_json::from_slice(&fetched.body)
            .map_err(|_| "the server's answer to _sort=_lastUpdated is not JSON".to_string())?;
        let at = bundle
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| e.get("resource"))
            .find(|r| r.get("resourceType").and_then(Value::as_str) == Some(resource_type))
            .and_then(|r| r.pointer("/meta/lastUpdated"))
            .and_then(Value::as_str)
            .and_then(parse_instant)
            .ok_or_else(|| {
                format!("the first {resource_type} sorted by _lastUpdated has no meta.lastUpdated")
            })?;
        Ok((at.0, fetched.date))
    };
    let (oldest, started) = first("_lastUpdated")?;
    let (newest, _) = first("-_lastUpdated")?;
    if newest < oldest {
        return Err(
            "the server did not sort by _lastUpdated (the newest came back older than the oldest)"
                .to_string(),
        );
    }
    Ok((oldest, newest + 1, started))
}

/// Each window's count, or `None` when the server gives no count for one.
fn count_all(client: &FhirClient, o: &Options, windows: &[Window]) -> Option<Vec<u64>> {
    parallel(windows, o.workers, |w| {
        count_matching(client, o.server, o.resource_type, &w.params())
    })
    .into_iter()
    .collect()
}

/// One window's download.
struct Paged {
    notes: Vec<PageNote>,
    pages: usize,
    report: Report,
}

/// Page one window into `file`, again when its paged count is not its count.
fn page_window(
    client: &FhirClient,
    o: &Options,
    (window, count): (Window, u64),
    file: &Path,
) -> Result<Paged> {
    let t = o.resource_type;
    let mut expected = count;
    let mut attempt = 1;
    loop {
        let mut report = Report::default();
        let paged = page_search(
            client,
            o.server,
            o.kind,
            t,
            &window.params(),
            file,
            &mut report,
            &[],
            &mut |_| true,
        )?
        .expect("paging stops early only when told to");
        let distinct = paged
            .notes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<HashSet<_>>()
            .len() as u64;
        // A count that may be an estimate is not compared.
        if distinct != expected && !o.kind.count_is_estimate(expected) {
            if attempt < ATTEMPTS {
                println!(
                    "{t}: the window {} paged {distinct} resources but the server counted {expected}; downloading it again",
                    window.describe()
                );
                attempt += 1;
                expected =
                    count_matching(client, o.server, t, &window.params()).unwrap_or(expected);
                continue;
            }
            report.add(
                "count_mismatch",
                t,
                &format!(
                    "the server counts {expected} {t} resources updated {} but the export paged {distinct}, \
                     {ATTEMPTS} times; resources are being added or deleted, or the server's count is stale",
                    window.describe()
                ),
            );
        }
        return Ok(Paged {
            notes: paged.notes,
            pages: paged.pages,
            report,
        });
    }
}

/// Write the windows' files to `out` in order, keeping the last copy of each
/// id. Line n of a window's file is its note n. Returns the kept notes.
/// When no id repeats, which is the usual case, the files are concatenated
/// as they are, without reading them line by line.
fn join(files: &[PathBuf], paged: &[Paged], out: &Path) -> Result<Vec<PageNote>> {
    let mut last: HashMap<&str, (usize, usize)> = HashMap::new();
    for (w, p) in paged.iter().enumerate() {
        for (n, note) in p.notes.iter().enumerate() {
            last.insert(note.id.as_str(), (w, n));
        }
    }
    let lines: usize = paged.iter().map(|p| p.notes.len()).sum();
    if last.len() == lines {
        concatenate(files, out)?;
        return Ok(paged.iter().flat_map(|p| p.notes.clone()).collect());
    }
    let mut kept = Vec::with_capacity(last.len());
    let file = crate::fs::create_private(out).map_err(|e| PipesError::io(out, e))?;
    let mut writer = BufWriter::new(file);
    for (w, (path, p)) in files.iter().zip(paged).enumerate() {
        for (n, line) in NdjsonReader::open(path)?.enumerate() {
            let line = line?.text;
            let note = p.notes.get(n).ok_or_else(|| {
                PipesError::Environment(format!(
                    "{}: more lines than resources paged",
                    path.display()
                ))
            })?;
            if last.get(note.id.as_str()) == Some(&(w, n)) {
                writeln!(writer, "{line}").map_err(|e| PipesError::io(out, e))?;
                kept.push(note.clone());
            }
        }
    }
    writer.flush().map_err(|e| PipesError::io(out, e))?;
    Ok(kept)
}

/// `files` one after another into `out`: the first renamed, the rest
/// appended to it, each removed once copied.
pub(super) fn concatenate(files: &[PathBuf], out: &Path) -> Result<()> {
    let Some((first, rest)) = files.split_first() else {
        crate::fs::create_private(out).map_err(|e| PipesError::io(out, e))?;
        return Ok(());
    };
    std::fs::rename(first, out).map_err(|e| PipesError::io(out, e))?;
    if rest.is_empty() {
        return Ok(());
    }
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(out)
        .map_err(|e| PipesError::io(out, e))?;
    for path in rest {
        let mut reader = std::fs::File::open(path).map_err(|e| PipesError::io(path, e))?;
        std::io::copy(&mut reader, &mut writer).map_err(|e| PipesError::io(out, e))?;
        drop(reader);
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// `f` over `items` on up to `workers` threads; the results in item order.
pub(super) fn parallel<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let mut results: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    crate::work::run(
        workers.clamp(1, items.len().max(1)),
        crate::work::indices(0, items.len()),
        |i| (i, f(&items[i])),
        |(i, r)| {
            results[i] = Some(r);
            true
        },
    );
    results
        .into_iter()
        .map(|r| r.expect("every item was run"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Auth;
    use httptest::bytes::Bytes;
    use httptest::http;
    use httptest::responders::Responder;
    use httptest::{Expectation, Server, matchers::request};
    use serde_json::Value;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    const T0: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

    /// A FHIR server holding `data` (id, meta.lastUpdated) in server order:
    /// `_lastUpdated` ge/lt filters, `_sort` on `_lastUpdated`, `_summary=count`,
    /// and `_count` paging with an `_offset` in the next link.
    #[derive(Clone)]
    struct Fake {
        data: Arc<Vec<(String, String)>>,
        sorts: bool,
        filters: bool,
        /// Counts doubled for a window bounded on both sides and narrower
        /// than this many seconds.
        inflate_below: Option<i64>,
        /// First pages of a window to serve one resource short.
        shorten: Arc<AtomicUsize>,
        delay: Duration,
        in_flight: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
        queries: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        fn new(data: Vec<(String, String)>) -> Self {
            Fake {
                data: Arc::new(data),
                sorts: true,
                filters: true,
                inflate_below: None,
                shorten: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
                in_flight: Arc::new(AtomicUsize::new(0)),
                most: Arc::new(AtomicUsize::new(0)),
                queries: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn answer(&self, path: &str, query: &str) -> (u16, String) {
            self.queries.lock().unwrap().push(query.to_string());
            let params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();
            let get = |k: &str| -> Vec<&str> {
                params
                    .iter()
                    .filter(|(n, _)| n == k)
                    .map(|(_, v)| v.as_str())
                    .collect()
            };
            let at = |s: &str| parse_instant(s).unwrap();
            let mut rows: Vec<&(String, String)> = self.data.iter().collect();
            let windowed = !get("_lastUpdated").is_empty();
            if self.filters {
                for v in get("_lastUpdated") {
                    let (op, bound) = v.split_at(2);
                    let bound = at(bound);
                    rows.retain(|r| match op {
                        "ge" => at(&r.1) >= bound,
                        "lt" => at(&r.1) < bound,
                        _ => panic!("{v}"),
                    });
                }
            }
            if get("_summary") == ["count"] {
                let bound = |op: &str| {
                    get("_lastUpdated")
                        .iter()
                        .find(|v| v.starts_with(op))
                        .map(|v| at(&v[2..]).0)
                };
                let narrow = match (bound("ge"), bound("lt"), self.inflate_below) {
                    (Some(a), Some(b), Some(n)) => b - a < n,
                    _ => false,
                };
                let total = rows.len() * if narrow { 2 } else { 1 };
                return (
                    200,
                    serde_json::json!({"resourceType":"Bundle","total":total}).to_string(),
                );
            }
            if let Some(sort) = get("_sort").first() {
                if !self.sorts {
                    return (400, r#"{"resourceType":"OperationOutcome"}"#.into());
                }
                rows.sort_by_key(|r| at(&r.1));
                if sort.starts_with('-') {
                    rows.reverse();
                }
            }
            let count: usize = get("_count").first().map_or(1000, |c| c.parse().unwrap());
            let offset: usize = get("_offset").first().map_or(0, |c| c.parse().unwrap());
            let mut page: Vec<&(String, String)> =
                rows.iter().skip(offset).take(count).copied().collect();
            if windowed && offset == 0 && !page.is_empty() && take_one(&self.shorten) {
                page.remove(0);
            }
            let mut bundle = serde_json::json!({
                "resourceType": "Bundle",
                "type": "searchset",
                "entry": page.iter().map(|(id, u)| serde_json::json!({"resource": {
                    "resourceType": "Observation", "id": id, "meta": {"lastUpdated": u}
                }})).collect::<Vec<_>>(),
            });
            if offset + count < rows.len() {
                let mut next = url::form_urlencoded::Serializer::new(String::new());
                for (k, v) in params.iter().filter(|(k, _)| k != "_offset") {
                    next.append_pair(k, v);
                }
                next.append_pair("_offset", &(offset + count).to_string());
                bundle["link"] = serde_json::json!([{"relation":"next","url":format!("{path}?{}", next.finish())}]);
            }
            (200, bundle.to_string())
        }
    }

    impl Responder for Fake {
        fn respond<'a>(
            &mut self,
            req: &'a http::Request<Bytes>,
        ) -> Pin<Box<dyn Future<Output = http::Response<Bytes>> + Send + 'a>> {
            let fake = self.clone();
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            Box::pin(async move {
                let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                fake.most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(fake.delay).await;
                let (status, body) = fake.answer(&path, &query);
                fake.in_flight.fetch_sub(1, Ordering::SeqCst);
                http::Response::builder()
                    .status(status)
                    .body(Bytes::from(body))
                    .unwrap()
            })
        }
    }

    fn serve(fake: &Fake) -> Server {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Observation"))
                .times(..)
                .respond_with(fake.clone()),
        );
        server
    }

    fn stamp(secs: i64, millis: u32) -> String {
        let s = instant(secs);
        format!("{}.{millis:03}Z", s.trim_end_matches('Z'))
    }

    /// `n` resources over `span` seconds from T0, in an order that is not
    /// their time order.
    fn spread(n: usize, span: i64) -> Vec<(String, String)> {
        (0..n)
            .map(|i| {
                let k = (i * 7919) % n;
                (
                    format!("r{i:05}"),
                    stamp(T0 + k as i64 * span / n as i64, (k % 1000) as u32),
                )
            })
            .collect()
    }

    struct Run {
        result: Windowed,
        lines: Vec<(String, String)>,
        report: Report,
    }

    fn run(fake: &Fake, server: &Server, workers: usize, limit: usize) -> Run {
        run_with(fake, server, workers, limit, None)
    }

    fn run_with(
        fake: &Fake,
        server: &Server,
        workers: usize,
        limit: usize,
        max_window: Option<u64>,
    ) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.ndjson");
        let client = FhirClient::new(Auth::None, 1, Duration::from_secs(10))
            .unwrap()
            .with_concurrency(limit);
        let mut report = Report::default();
        let base = server.url("/fhir").to_string();
        let result = download(
            &client,
            &Options {
                server: &base,
                kind: ServerKind::Generic,
                max_window,
                resource_type: "Observation",
                total: fake.data.len() as u64,
                workers,
                dir: dir.path(),
            },
            &out,
            &mut report,
        )
        .unwrap();
        let lines = std::fs::read_to_string(&out)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["meta"]["lastUpdated"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        // Only the output is left: the window files are removed.
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.len() <= 1, "{left:?}");
        Run {
            result,
            lines,
            report,
        }
    }

    fn windows(r: &Run) -> usize {
        match &r.result {
            Windowed::Done { windows, .. } => *windows,
            Windowed::Unsuitable(why) => panic!("not windowed: {why}"),
        }
    }

    fn sorted_ids(lines: &[(String, String)]) -> Vec<String> {
        let mut ids: Vec<String> = lines.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids
    }

    #[test]
    fn windows_without_a_repeated_id_are_concatenated_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, ids: &[&str]| {
            let path = dir.path().join(name);
            let text: String = ids
                .iter()
                .map(|id| format!("{{\"id\":\"{id}\",\"n\":\"{name}\"}}\n"))
                .collect();
            std::fs::write(&path, text).unwrap();
            let notes = ids
                .iter()
                .map(|id| PageNote {
                    id: id.to_string(),
                    last_updated: None,
                    boundary_url: None,
                })
                .collect();
            (
                path,
                Paged {
                    notes,
                    pages: 1,
                    report: Report::default(),
                },
            )
        };
        let run = |sets: &[&[&str]], out: &str| {
            let (files, paged): (Vec<_>, Vec<_>) = sets
                .iter()
                .enumerate()
                .map(|(i, ids)| write(&format!("{out}-w{i}"), ids))
                .unzip();
            let out = dir.path().join(out);
            let kept = join(&files, &paged, &out).unwrap();
            let kept: Vec<String> = kept.into_iter().map(|n| n.id).collect();
            (std::fs::read_to_string(&out).unwrap(), kept, files)
        };
        let (text, kept, files) = run(&[&["a", "b"], &[], &["c"]], "plain");
        assert_eq!(
            text,
            "{\"id\":\"a\",\"n\":\"plain-w0\"}\n{\"id\":\"b\",\"n\":\"plain-w0\"}\n{\"id\":\"c\",\"n\":\"plain-w2\"}\n"
        );
        assert_eq!(kept, ["a", "b", "c"]);
        assert!(files.iter().all(|f| !f.exists()), "the windows are used up");
        // A repeat is read line by line, keeping the later copy.
        let (text, kept, _) = run(&[&["a", "b"], &["a"]], "repeat");
        assert_eq!(
            text,
            "{\"id\":\"b\",\"n\":\"repeat-w0\"}\n{\"id\":\"a\",\"n\":\"repeat-w1\"}\n"
        );
        assert_eq!(kept, ["b", "a"]);
    }

    #[test]
    fn windows_are_half_open_and_cover_everything() {
        let w = split(T0, T0 + 4, 4);
        assert_eq!(w.len(), 4);
        assert_eq!(
            w[0].params(),
            [("_lastUpdated", "lt2026-01-01T00:00:01Z".into())]
        );
        assert_eq!(
            w[1].params(),
            [
                ("_lastUpdated", "ge2026-01-01T00:00:01Z".into()),
                ("_lastUpdated", "lt2026-01-01T00:00:02Z".into())
            ]
        );
        assert_eq!(
            w[3].params(),
            [("_lastUpdated", "ge2026-01-01T00:00:03Z".into())]
        );
        // A range too short for every window gives fewer, never empty ones.
        assert_eq!(split(T0, T0 + 2, 8).len(), 2);
    }

    #[test]
    fn a_windowed_download_has_every_resource_once_in_window_order() {
        let fake = Fake::new(spread(5000, 86_400));
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        assert!(windows(&r) >= 4, "{}", windows(&r));
        assert_eq!(r.lines.len(), 5000);
        let mut want: Vec<String> = fake.data.iter().map(|(id, _)| id.clone()).collect();
        want.sort();
        assert_eq!(sorted_ids(&r.lines), want);
        // The windows paged, from the server's log, oldest first.
        let mut bounds: Vec<(Option<i64>, Option<i64>)> = fake
            .queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| {
                q.contains("_lastUpdated=") && !q.contains("_summary") && !q.contains("_offset")
            })
            .map(|q| {
                let mut b = (None, None);
                for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
                    if k == "_lastUpdated" {
                        let at = Some(parse_instant(&v[2..]).unwrap().0);
                        if v.starts_with("ge") {
                            b.0 = at
                        } else {
                            b.1 = at
                        }
                    }
                }
                b
            })
            .collect();
        bounds.sort();
        bounds.dedup();
        assert_eq!(bounds.len(), windows(&r), "{bounds:?}");
        // Window by window, and the server's order within each.
        let position: HashMap<&str, usize> = fake
            .data
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        let mut previous = (0, 0);
        for (id, updated) in &r.lines {
            let t = parse_instant(updated).unwrap().0;
            let w = bounds
                .iter()
                .position(|(ge, lt)| ge.is_none_or(|g| t >= g) && lt.is_none_or(|l| t < l))
                .unwrap();
            let here = (w, position[id.as_str()]);
            assert!(
                here > previous || previous == (0, 0),
                "{id} at {here:?} after {previous:?}"
            );
            previous = here;
        }
        assert_eq!(r.report.counts().len(), 0, "{:?}", r.report.counts());
        let Windowed::Done { paged, .. } = &r.result else {
            unreachable!()
        };
        assert_eq!(paged.notes.len(), 5000);
    }

    #[test]
    fn resources_stamped_on_a_boundary_land_in_the_window_that_starts_there() {
        // 300 resources at each of T0, T0+1, T0+2 and T0+3: with four
        // windows the cuts fall exactly on the last three.
        let mut data = Vec::new();
        for k in 0..4 {
            for i in 0..300 {
                data.push((format!("s{k}-{i:03}"), instant(T0 + k)));
            }
        }
        // Server order is not time order.
        data.reverse();
        let fake = Fake::new(data);
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        assert_eq!(windows(&r), 4);
        assert_eq!(r.lines.len(), 1200);
        // Window by window: the first 300 lines are T0's, then T0+1's, …
        for (n, (id, _)) in r.lines.iter().enumerate() {
            assert!(id.starts_with(&format!("s{}-", n / 300)), "line {n}: {id}");
        }
        let queries = fake.queries.lock().unwrap();
        assert!(
            queries.iter().any(|q| q.contains(
                "_lastUpdated=ge2026-01-01T00%3A00%3A01Z&_lastUpdated=lt2026-01-01T00%3A00%3A02Z"
            )),
            "{queries:?}"
        );
    }

    #[test]
    fn updates_bunched_in_time_are_split_further() {
        // 3000 resources in the first ten seconds of a day, 300 over the rest.
        let mut data: Vec<(String, String)> = (0..3000)
            .map(|i| {
                (
                    format!("a{i:04}"),
                    stamp(T0 + (i % 10) as i64, i as u32 % 1000),
                )
            })
            .collect();
        data.extend((0..300).map(|i| (format!("b{i:03}"), instant(T0 + 3600 + i * 200))));
        let fake = Fake::new(data);
        let server = serve(&fake);
        let r = run(&fake, &server, 2, 2);
        // Two windows of time would put 3000 in the first; halving gives more.
        assert!(windows(&r) > 2, "{}", windows(&r));
        assert_eq!(r.lines.len(), 3300);
    }

    #[test]
    fn no_window_holds_more_than_the_limit() {
        // Two workers make at most 16 windows; 6000 resources at no more
        // than 250 a window need at least 24.
        let fake = Fake::new(spread(6000, 86_400));
        let server = serve(&fake);
        let r = run_with(&fake, &server, 2, 2, Some(250));
        assert!(windows(&r) >= 24, "{}", windows(&r));
        assert_eq!(r.lines.len(), 6000);
        // Each window paged, by its bounds, holds at most 250.
        let queries = fake.queries.lock().unwrap();
        let firsts: Vec<&String> = queries
            .iter()
            .filter(|q| q.contains("_count=1000") && !q.contains("_offset"))
            .collect();
        assert_eq!(firsts.len(), windows(&r));
        for q in firsts {
            let mut held: Vec<i64> = fake
                .data
                .iter()
                .map(|(_, u)| parse_instant(u).unwrap().0)
                .collect();
            for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
                if k == "_lastUpdated" {
                    let at = parse_instant(&v[2..]).unwrap().0;
                    if v.starts_with("ge") {
                        held.retain(|t| *t >= at);
                    } else {
                        held.retain(|t| *t < at);
                    }
                }
            }
            assert!(held.len() <= 250, "{q}: {}", held.len());
        }
    }

    #[test]
    fn a_resource_in_two_windows_keeps_its_later_version() {
        // `moved` was updated while the windows were paged: the first window
        // saw its old version and the last its new one.
        let mut data = spread(3000, 3600);
        data.push(("moved".into(), stamp(T0 + 1, 0)));
        data.push(("moved".into(), stamp(T0 + 3599, 0)));
        let fake = Fake::new(data);
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        windows(&r);
        assert_eq!(r.lines.len(), 3001);
        let moved: Vec<&(String, String)> =
            r.lines.iter().filter(|(id, _)| id == "moved").collect();
        assert_eq!(moved, [&("moved".to_string(), stamp(T0 + 3599, 0))]);
    }

    #[test]
    fn a_window_short_of_its_count_is_downloaded_again() {
        let fake = Fake::new(spread(3000, 3600));
        fake.shorten.store(1, Ordering::SeqCst);
        let server = serve(&fake);
        let r = run(&fake, &server, 3, 3);
        windows(&r);
        assert_eq!(r.lines.len(), 3000, "the second download has them all");
        assert_eq!(r.report.count("count_mismatch"), 0);
        // One window's first page was asked for twice.
        let queries = fake.queries.lock().unwrap();
        let firsts: Vec<&String> = queries
            .iter()
            .filter(|q| {
                q.contains("_lastUpdated") && q.contains("_count=1000") && !q.contains("_offset")
            })
            .collect();
        let distinct: HashSet<&&String> = firsts.iter().collect();
        assert_eq!(firsts.len(), distinct.len() + 1, "{firsts:?}");
    }

    #[test]
    fn a_window_that_stays_short_is_reported() {
        let fake = Fake::new(spread(3000, 3600));
        fake.shorten.store(usize::MAX, Ordering::SeqCst);
        let server = serve(&fake);
        let r = run(&fake, &server, 3, 3);
        let n = windows(&r);
        assert_eq!(r.report.count("count_mismatch"), n);
        assert_eq!(r.lines.len(), 3000 - n);
    }

    #[test]
    fn a_server_that_cannot_sort_by_last_updated_is_not_windowed() {
        let mut fake = Fake::new(spread(3000, 3600));
        fake.sorts = false;
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        match r.result {
            Windowed::Unsuitable(why) => assert!(why.contains("did not sort"), "{why}"),
            Windowed::Done { .. } => panic!("windowed"),
        }
    }

    #[test]
    fn windows_whose_counts_do_not_add_up_are_not_used() {
        // A server that ignores _lastUpdated: every window counts everything.
        let mut fake = Fake::new(spread(3000, 3600));
        fake.filters = false;
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        match r.result {
            Windowed::Unsuitable(why) => assert!(why.contains("server counts 3000"), "{why}"),
            Windowed::Done { .. } => panic!("windowed"),
        }
    }

    #[test]
    fn a_server_that_ignores_the_window_bounds_is_found_before_halving() {
        // Every window counts the whole type, so every one is over the
        // limit; halving them down to a second each would take thousands of
        // counts.
        let mut fake = Fake::new(spread(3000, 3600));
        fake.filters = false;
        let server = serve(&fake);
        let r = run_with(&fake, &server, 2, 2, Some(500));
        match r.result {
            Windowed::Unsuitable(why) => assert!(why.contains("server counts 3000"), "{why}"),
            Windowed::Done { .. } => panic!("windowed"),
        }
        let counts = fake
            .queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("_summary=count"))
            .count();
        assert!(counts <= 3, "{counts} counts");
    }

    #[test]
    fn halves_that_do_not_add_up_stop_the_halving_past_the_usual_limit() {
        // Two workers halve up to 16 windows of 225 s, about 190 resources
        // each, which is over the limit of 100. A window narrower than 200 s
        // counts double, so the next halves do not add up: the halving stops
        // there instead of going on to one-second windows.
        let mut fake = Fake::new(spread(3000, 3600));
        fake.inflate_below = Some(200);
        let server = serve(&fake);
        let r = run_with(&fake, &server, 2, 2, Some(100));
        assert_eq!(windows(&r), 16);
        assert_eq!(r.lines.len(), 3000);
        let counts = fake
            .queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("_summary=count"))
            .count();
        assert!(counts <= 100, "{counts} counts");
    }

    #[test]
    fn one_second_of_updates_is_not_windowed() {
        let data = (0..2000)
            .map(|i| (format!("r{i}"), stamp(T0, i % 1000)))
            .collect();
        let fake = Fake::new(data);
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 4);
        assert!(matches!(r.result, Windowed::Unsuitable(_)));
    }

    #[test]
    fn requests_in_flight_stay_within_the_clients_limit() {
        let mut fake = Fake::new(spread(6000, 86_400));
        fake.delay = Duration::from_millis(20);
        let server = serve(&fake);
        let r = run(&fake, &server, 4, 2);
        windows(&r);
        assert_eq!(r.lines.len(), 6000);
        assert_eq!(fake.most.load(Ordering::SeqCst), 2);
    }

    /// Take one from `n` if it is above zero. A compare-exchange loop rather
    /// than `fetch_update`, which newer toolchains deprecate under another name.
    fn take_one(n: &AtomicUsize) -> bool {
        let mut now = n.load(Ordering::SeqCst);
        while now > 0 {
            match n.compare_exchange(now, now - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(seen) => now = seen,
            }
        }
        false
    }
}
