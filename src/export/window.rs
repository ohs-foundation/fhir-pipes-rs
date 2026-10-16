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
//! type's; when they do not, the server is not filtering by `_lastUpdated` as
//! asked, and the caller pages the type as one stream.
//!
//! Each window is paged into its own file. A window whose paged count is not
//! its count is counted and paged again; a second difference is reported as
//! `count_mismatch`. The files are then joined in window order, oldest
//! first, each in the server's order, keeping the last copy of an id: a
//! resource updated while the windows were paged is in a later window, with
//! its newer version.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;
use url::Url;

use crate::error::{PipesError, Result};
use crate::extract::client::FhirClient;
use crate::extract::page::{
    PAGE_SIZE, PageNote, PageResult, count_matching, count_resources, page_search,
};
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
    let (lo, hi, started) = match probe(client, o.server, o.resource_type) {
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

    // Halve the windows holding more than their share, largest first, so
    // updates bunched in time do not leave one window with most of the type.
    let share = (o.total / (2 * workers as u64)).max(PAGE_SIZE as u64);
    let most = 8 * workers;
    loop {
        let mut big: Vec<usize> = (0..windows.len())
            .filter(|&i| {
                let (a, b) = windows[i].span(lo, hi);
                counts[i] > share && b - a >= 2
            })
            .collect();
        big.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
        big.truncate(most.saturating_sub(windows.len()));
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

    let sum: u64 = counts.iter().sum();
    // Counted again before giving up, in case resources were added since.
    if sum != o.total && count_resources(client, o.server, o.resource_type) != Some(sum) {
        return Ok(Windowed::Unsuitable(format!(
            "its _lastUpdated windows count {sum} resources but the server counts {} in all",
            o.total
        )));
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
    let files = Remove(
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
/// an exclusive bound, and the server's `Date` on the first probe.
fn probe(
    client: &FhirClient,
    server: &str,
    resource_type: &str,
) -> std::result::Result<(i64, i64, Option<String>), String> {
    let first = |sort: &str| -> std::result::Result<(i64, Option<String>), String> {
        let base = server.trim_end_matches('/');
        let mut url = Url::parse(&format!("{base}/{resource_type}"))
            .map_err(|_| format!("--server is not a valid URL: {server}"))?;
        url.query_pairs_mut()
            .append_pair("_sort", sort)
            .append_pair("_count", "1");
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
        if distinct != expected {
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
fn join(files: &[PathBuf], paged: &[Paged], out: &Path) -> Result<Vec<PageNote>> {
    let mut last: HashMap<&str, (usize, usize)> = HashMap::new();
    for (w, p) in paged.iter().enumerate() {
        for (n, note) in p.notes.iter().enumerate() {
            last.insert(note.id.as_str(), (w, n));
        }
    }
    let mut kept = Vec::with_capacity(last.len());
    let file = std::fs::File::create(out).map_err(|e| PipesError::io(out, e))?;
    let mut writer = BufWriter::new(file);
    for (w, (path, p)) in files.iter().zip(paged).enumerate() {
        let reader =
            BufReader::new(std::fs::File::open(path).map_err(|e| PipesError::io(path, e))?);
        for (n, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| PipesError::io(path, e))?;
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

/// `f` over `items` on up to `workers` threads; the results in item order.
fn parallel<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    results.lock().unwrap_or_else(PoisonError::into_inner)[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|r| r.expect("every item was run"))
        .collect()
}

/// Files removed when dropped, whether the download worked or not.
struct Remove(Vec<PathBuf>);

impl Drop for Remove {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
