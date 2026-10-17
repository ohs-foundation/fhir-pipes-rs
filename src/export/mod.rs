//! `fhir-pipes export`: download every resource of the given types from a FHIR
//! server, one NDJSON file per type, each written beside its target and
//! renamed into place.
//!
//! `OUT/.export-state.json` records, per type, the greatest `meta.lastUpdated`
//! downloaded (the watermark). With `--incremental`, a type that has one is
//! searched with `_lastUpdated=ge<watermark>` and the result merged into its
//! file by id, as `extract` does for Locations. A `_lastUpdated` search cannot
//! see deletions, so after the merge the server's count is compared with the
//! file's, and a type that differs is downloaded in full.
//!
//! With `--bulk`, the files come from a Bulk Data `$export` instead (see
//! `bulk`). Types with a watermark are exported together with `_since` the
//! earliest of their watermarks and merged by id; types without one are
//! exported in full in a second export. The export's `deleted` resources are
//! removed from the files, and every type's watermark becomes the manifest's
//! `transactionTime`, or the server's clock at the kick-off (its `Date`)
//! when that is earlier. The count check after a merge is the same as above.
//! The output files download `--concurrency` at a time, through the same
//! limit, and each type's are joined in manifest order.
//!
//! With `--hapi-db`, the files come from a HAPI FHIR JPA server's PostgreSQL
//! database instead of its API (see `hapi_db`), with watermarks of their own.
//!
//! Where `OUT/.changes.json` exists (refresh with `[database] mode =
//! "upsert"`), each merge and each file replaced whole also records the ids
//! that changed or were deleted (see `changes`).
//!
//! Searches run `--concurrency` requests at a time, through one limit in the
//! client: that many types are brought up to date side by side, and a full
//! download of a large type is split into `_lastUpdated` windows paged side
//! by side (see `window`). Each type's file is the same whatever the
//! setting, except that a windowed file is in window order.

mod bulk;
pub(crate) mod changes;
#[cfg(feature = "postgres")]
mod hapi_db;
mod window;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::client::FhirClient;
use crate::extract::page::{
    PageNote, PageResult, ServerKind, count_resources, next_link, page_resources, page_search,
};
use crate::report::Report;
use crate::snapshot::instant::{capped_watermark, format_utc, later, parse_instant};
use crate::snapshot::merge::{MergeFiles, MergeOptions, MergeStats, id_and_updated, merge_file};
use crate::snapshot::same_server;
use crate::transform::write_report;

pub(crate) const STATE_FILE: &str = ".export-state.json";

/// The environment variable `--hapi-db` takes the database's connection
/// string from; it is never read from the command line or a file.
pub const HAPI_DB_URL_ENV: &str = "FHIR_PIPES_HAPI_DATABASE_URL";

/// What the last export into a directory downloaded, per resource type.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportState {
    server: String,
    #[serde(default)]
    types: BTreeMap<String, TypeState>,
    /// A Bulk Data export started and not yet finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bulk: Option<bulk::Pending>,
    /// `--hapi-db`, per type: the watermark is the greatest
    /// `hfj_resource.res_updated` read, as the database has it (no zone).
    /// The server is then the database and schema, so these
    /// and `types` are never both filled.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hapi_db: BTreeMap<String, TypeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypeState {
    /// The greatest meta.lastUpdated in the file; none when no resource had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    count: usize,
    completed_at: String,
    fhir_pipes_version: String,
}

impl ExportState {
    /// The state in `dir`, or an empty one. A state file that cannot be read
    /// or parsed means a full download, with a warning, not a failed run:
    /// the state only lets the next run download less, and a full download
    /// replaces every file it describes.
    fn read(dir: &Path) -> Self {
        let path = dir.join(STATE_FILE);
        let why = match crate::fs::read_state(&path) {
            Ok(state) => return state.unwrap_or_default(),
            Err(crate::fs::StateError::Invalid(e)) => format!("not a valid export state ({e})"),
            Err(crate::fs::StateError::Unreadable(e) | crate::fs::StateError::Io(e)) => {
                format!("not readable ({e})")
            }
        };
        eprintln!("{}: {why}; downloading in full", path.display());
        Self::default()
    }

    /// Written beside the target and renamed into place.
    fn write(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        crate::fs::atomic_write(&dir.join(STATE_FILE), text)
    }
}

/// A FHIR resource type name: an upper-case letter, then letters. It becomes
/// a file name, so nothing else is accepted.
pub(crate) fn valid_type(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphabetic())
}

pub fn run_export(args: &ExportArgs) -> Result<()> {
    let mut types: Vec<&str> = Vec::new();
    for t in &args.types {
        if !valid_type(t) {
            return Err(PipesError::Usage(format!(
                "--type {t}: not a FHIR resource type name (e.g. MeasureReport)"
            )));
        }
        if !types.contains(&t.as_str()) {
            types.push(t);
        }
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()));
    }
    std::fs::create_dir_all(&args.out)
        .map_err(|e| PipesError::Usage(format!("--out {}: {e}", args.out.display())))?;
    if args.hapi_db {
        let mut report = Report::default();
        export_hapi_db(args, &types, &mut report)?;
        if let Some(path) = &args.report {
            write_report(path, &report)?;
        }
        println!("{}", report.summary());
        return Ok(());
    }
    let client = FhirClient::new(
        args.auth.auth()?,
        args.retries,
        Duration::from_secs(args.timeout),
    )?
    .with_concurrency(args.concurrency);
    let mut state = ExportState::read(&args.out);
    if !same_server(&state.server, &args.server) {
        // Files from another server are not merged into; every type starts over.
        state = ExportState {
            server: args.server.clone(),
            ..ExportState::default()
        };
    }
    let mut report = Report::default();
    if args.bulk {
        export_bulk(&client, args, &types, &mut state, &mut report)?;
    } else {
        if let Some(pending) = state.bulk.take() {
            // Searching now; a bulk export left from an earlier run would
            // only be resumed later over newer files.
            cancel(&client, &pending.status_url);
            state.write(&args.out)?;
        }
        export_search(&client, args, &types, &mut state, &mut report)?;
    }
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    println!("{}", report.summary());
    Ok(())
}

/// The `--hapi-db` side of `run_export`.
#[cfg(feature = "postgres")]
fn export_hapi_db(args: &ExportArgs, types: &[&str], report: &mut Report) -> Result<()> {
    let url = std::env::var(HAPI_DB_URL_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            PipesError::Usage(format!(
                "--hapi-db needs the database's connection string in ${HAPI_DB_URL_ENV}"
            ))
        })?;
    hapi_db::export(&url, args, types, report)
}

#[cfg(not(feature = "postgres"))]
fn export_hapi_db(_args: &ExportArgs, _types: &[&str], _report: &mut Report) -> Result<()> {
    Err(PipesError::Usage(
        "--hapi-db needs PostgreSQL support, which this fhir-pipes was built without; \
         build it with `--features postgres`"
            .into(),
    ))
}

/// Bring every type up to date by search, `--concurrency` types at a time.
/// The state is written as each type finishes, so an interrupted run keeps
/// what it finished. A failure stops new types from starting; the ones
/// already running finish. The types' issues go into `report` in the order
/// of `types`, whichever finished first.
fn export_search(
    client: &FhirClient,
    args: &ExportArgs,
    types: &[&str],
    state: &mut ExportState,
    report: &mut Report,
) -> Result<()> {
    let previous: Vec<Option<TypeState>> = types
        .iter()
        .map(|t| state.types.get(*t).filter(|_| args.incremental).cloned())
        .collect();
    let mut reports: Vec<Option<Report>> = (0..types.len()).map(|_| None).collect();
    let mut failure = None;
    crate::work::run(
        args.concurrency.clamp(1, types.len().max(1)),
        crate::work::indices(0, types.len()),
        |i| {
            let mut issues = Report::default();
            let done = export_type(client, args, types[i], previous[i].as_ref(), &mut issues);
            (i, done, issues)
        },
        |(i, done, issues)| {
            reports[i] = Some(issues);
            let written = done.and_then(|done| {
                state.types.insert(types[i].to_string(), done);
                state.write(&args.out)
            });
            if let Err(e) = written {
                failure.get_or_insert(e);
                return false;
            }
            true
        },
    );
    for issues in reports.into_iter().flatten() {
        report.append(issues);
    }
    failure.map_or(Ok(()), Err)
}

/// Bring OUT/<resource_type>.ndjson up to date: incrementally when there is
/// a previous watermark and the file is still there, else in full.
fn export_type(
    client: &FhirClient,
    args: &ExportArgs,
    resource_type: &str,
    previous: Option<&TypeState>,
    report: &mut Report,
) -> Result<TypeState> {
    let out = ndjson_path(&args.out, resource_type);
    let since = previous
        .and_then(|p| p.watermark.as_deref())
        .filter(|_| out.exists());
    if let Some(since) = since
        && let Some(done) = export_changes(client, args, resource_type, since, &out, report)?
    {
        return Ok(done);
    }
    export_all(client, args, resource_type, &out, report)
}

fn type_state(watermark: Option<String>, count: usize) -> TypeState {
    TypeState {
        watermark,
        count,
        completed_at: format_utc(SystemTime::now()),
        fhir_pipes_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Fetch what changed since `since` and merge it into `out` by id. `None`
/// when the merged file's count differs from the server's, so the caller
/// downloads the type in full instead.
fn export_changes(
    client: &FhirClient,
    args: &ExportArgs,
    resource_type: &str,
    since: &str,
    out: &Path,
    report: &mut Report,
) -> Result<Option<TypeState>> {
    let incoming = incoming_path(&args.out, resource_type);
    let paged = match page_resources(
        client,
        &args.server,
        args.server_kind,
        resource_type,
        Some(since),
        &incoming,
        report,
        &[],
    ) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&incoming);
            return Err(e);
        }
    };
    changes::record(
        &args.out,
        resource_type,
        paged.notes.iter().map(|n| n.id.as_str()),
        [],
    )?;
    let stats = merge_file(
        &MergeFiles {
            existing: out.to_path_buf(),
            tmp: tmp_path(&args.out, resource_type),
            incoming,
            unparsed_kind: "export_line_unparsed",
        },
        &paged.notes,
        &MergeOptions::default(),
        report,
    )?;
    if merged_count_differs(
        client,
        &args.server,
        args.server_kind,
        resource_type,
        stats.total,
        out,
        report,
    ) {
        return Ok(None);
    }
    println!(
        // `updated` counts every id fetched again. The search is `ge` the
        // watermark, so resources stamped with that exact instant (a batch
        // loaded in one transaction) come back each run unchanged.
        "{resource_type}: {} new, {} updated or fetched again since {since}, {} in all, {} pages -> {}",
        stats.added,
        stats.updated,
        stats.total,
        paged.pages,
        out.display()
    );
    let watermark = capped_watermark(stats.watermark, paged.started.as_deref())
        .or_else(|| Some(since.to_string()));
    Ok(Some(type_state(watermark, stats.total)))
}

/// Whether the server's count of `resource_type` differs from the merged
/// file's `total`, which means the type must be downloaded in full. A server
/// that gives no count, or a count that may be an estimate, is taken at its
/// word, with a warning.
fn merged_count_differs(
    client: &FhirClient,
    server: &str,
    kind: ServerKind,
    resource_type: &str,
    total: usize,
    out: &Path,
    report: &mut Report,
) -> bool {
    match count_resources(client, server, resource_type) {
        Some(server_total)
            if server_total as usize != total && kind.count_is_estimate(server_total) =>
        {
            eprintln!(
                "count check unavailable for {resource_type}: the server's count ({server_total}) is an estimate, \
                 so resources deleted on the server stay in {}",
                out.display()
            );
            false
        }
        Some(server_total) if server_total as usize != total => {
            println!(
                "{resource_type}: the server has {server_total} resources and the merged file {total}; \
                 downloading in full (a resource was deleted, or the file had drifted)"
            );
            report.add(
                "incremental_count_mismatch",
                resource_type,
                &format!(
                    "the server reports {server_total} {resource_type} resources but the merged file has {total}; downloaded in full"
                ),
            );
            true
        }
        Some(_) => false,
        None => {
            eprintln!(
                "count check unavailable for {resource_type}: the server returned no total, \
                 so resources deleted on the server stay in {}",
                out.display()
            );
            false
        }
    }
}

fn ndjson_path(dir: &Path, resource_type: &str) -> PathBuf {
    dir.join(format!("{resource_type}.ndjson"))
}

fn incoming_path(dir: &Path, resource_type: &str) -> PathBuf {
    dir.join(format!(".{resource_type}.incoming.ndjson"))
}

fn tmp_path(dir: &Path, resource_type: &str) -> PathBuf {
    dir.join(format!(".{resource_type}.ndjson.tmp"))
}

/// Page `<server>/<resource_type>` into `out`. The file is written beside
/// the target and renamed into place, so a failed run leaves the previous
/// download intact.
fn export_all(
    client: &FhirClient,
    args: &ExportArgs,
    resource_type: &str,
    out: &Path,
    report: &mut Report,
) -> Result<TypeState> {
    let tmp = crate::fs::AtomicFile::adopt(out, tmp_path(&args.out, resource_type));
    let (paged, windows) = download_all(client, args, resource_type, tmp.tmp(), report)?;
    changes::record_replaced(&args.out, resource_type, out, tmp.tmp())?;
    tmp.commit()?;
    let total = paged.notes.len();
    // As in extract: a last page that was full with no next link may mean
    // the server stopped paging early, so only then is the count worth a
    // request (a count over a large table is not free).
    if paged.last_page_full {
        match count_resources(client, &args.server, resource_type) {
            Some(server_total) if args.server_kind.count_is_estimate(server_total) => eprintln!(
                "count check unavailable for {resource_type}: the server's count ({server_total}) is an estimate"
            ),
            Some(server_total) if server_total as usize != total => report.add(
                "count_mismatch",
                resource_type,
                &format!(
                    "the server reports {server_total} {resource_type} resources but the export paged {total}; \
                     the server stopped paging early (no next link) or its count is stale"
                ),
            ),
            Some(_) => {}
            None => eprintln!(
                "count check unavailable for {resource_type}: the server returned no total"
            ),
        }
    }
    match windows {
        Some(n) => println!(
            "{resource_type}: {total} resources, {} pages in {n} _lastUpdated windows -> {}",
            paged.pages,
            out.display()
        ),
        None => println!(
            "{resource_type}: {total} resources, {} pages -> {}",
            paged.pages,
            out.display()
        ),
    }
    let watermark = paged
        .notes
        .iter()
        .filter_map(|n| n.last_updated.as_deref())
        .fold(None, |w, u| later(w.as_deref(), u));
    Ok(type_state(
        capped_watermark(watermark, paged.started.as_deref()),
        total,
    ))
}

/// Page `resource_type` into `tmp`, as one stream or, for a type of at least
/// `window::MIN_RESOURCES` with `--concurrency` above 1, as `_lastUpdated`
/// windows side by side. A type larger than the server kind's `max_window`
/// is paged in windows whatever the concurrency. The first page tells which:
/// a type that fits on it costs no more requests than before; for one that
/// does not, the count comes from the page's `total` or a count request.
/// Returns how many windows were used, if any.
fn download_all(
    client: &FhirClient,
    args: &ExportArgs,
    resource_type: &str,
    tmp: &Path,
    report: &mut Report,
) -> Result<(PageResult, Option<usize>)> {
    let kind = args.server_kind;
    let side_by_side = args.concurrency >= 2;
    let mut large = None;
    let mut issues = Report::default();
    let stream = page_search(
        client,
        &args.server,
        kind,
        resource_type,
        &[],
        tmp,
        &mut issues,
        &[],
        &mut |first| {
            if !(side_by_side || kind.max_window().is_some()) || next_link(first).is_none() {
                return true;
            }
            large = first
                .get("total")
                .and_then(Value::as_u64)
                .or_else(|| count_resources(client, &args.server, resource_type))
                .filter(|&n| {
                    (side_by_side && n >= window::MIN_RESOURCES)
                        || kind.max_window().is_some_and(|most| n > most)
                });
            large.is_none()
        },
    )?;
    if let Some(paged) = stream {
        report.append(issues);
        return Ok((paged, None));
    }
    let options = window::Options {
        server: &args.server,
        kind,
        max_window: kind.max_window(),
        resource_type,
        total: large.expect("paging stops for a large type"),
        workers: args.concurrency,
        dir: &args.out,
    };
    match window::download(client, &options, tmp, report)? {
        window::Windowed::Done { paged, windows } => Ok((paged, Some(windows))),
        window::Windowed::Unsuitable(why) => {
            println!("{resource_type}: {why}; paging it as one stream");
            let paged = page_resources(
                client,
                &args.server,
                kind,
                resource_type,
                None,
                tmp,
                report,
                &[],
            )?;
            Ok((paged, None))
        }
    }
}

/// One `$export` a bulk run asks for: its types, sorted, and `_since` unless
/// it is a full download.
#[derive(Debug)]
struct Job {
    types: Vec<String>,
    since: Option<String>,
}

impl Job {
    fn matches(&self, pending: &bulk::Pending) -> bool {
        pending.types == self.types && pending.since == self.since
    }
}

/// The bulk side of `run_export`: an export of the types with a watermark,
/// `_since` the earliest one, and an export of the rest in full. A saved
/// export that matches one of them is resumed; one that matches neither is
/// cancelled. A type whose count is off after its merge is added to the
/// full export, or gets one of its own.
fn export_bulk(
    client: &FhirClient,
    args: &ExportArgs,
    types: &[&str],
    state: &mut ExportState,
    report: &mut Report,
) -> Result<()> {
    let mut changed = Job {
        types: Vec::new(),
        since: None,
    };
    let mut full = Job {
        types: Vec::new(),
        since: None,
    };
    let mut earliest: Option<((i64, u32), String)> = None;
    for &t in types {
        let watermark = state
            .types
            .get(t)
            .filter(|_| args.incremental && ndjson_path(&args.out, t).exists())
            .and_then(|p| p.watermark.as_deref())
            .and_then(|w| parse_instant(w).map(|at| (at, w.to_string())));
        match watermark {
            Some(w) => {
                if earliest.as_ref().is_none_or(|e| w.0 < e.0) {
                    earliest = Some(w);
                }
                changed.types.push(t.to_string());
            }
            None => full.types.push(t.to_string()),
        }
    }
    changed.since = earliest.map(|(_, w)| w);
    let mut jobs: VecDeque<Job> = [changed, full]
        .into_iter()
        .filter(|j| !j.types.is_empty())
        .map(|mut j| {
            j.types.sort();
            j
        })
        .collect();
    if let Some(pending) = state.bulk.clone() {
        match jobs.iter().position(|j| j.matches(&pending)) {
            // First, so that starting the other export does not replace it
            // in the state.
            Some(i) => {
                let job = jobs.remove(i).expect("the position is in range");
                jobs.push_front(job);
            }
            None => {
                println!(
                    "the bulk export saved by an earlier run asked for other types or another _since; cancelling it"
                );
                cancel(client, &pending.status_url);
                state.bulk = None;
                state.write(&args.out)?;
            }
        }
    }
    while let Some(job) = jobs.pop_front() {
        let redo = run_job(client, args, &job, state, report)?;
        if redo.is_empty() {
            continue;
        }
        match jobs.iter_mut().find(|j| j.since.is_none()) {
            Some(j) => {
                j.types.extend(redo);
                j.types.sort();
            }
            None => jobs.push_back(Job {
                types: redo,
                since: None,
            }),
        }
    }
    Ok(())
}

/// DELETE a bulk export's status URL, saying so when that fails.
fn cancel(client: &FhirClient, status_url: &str) {
    if let Err(e) = bulk::delete(client, status_url) {
        eprintln!("cancelling the bulk export at {status_url}: {e}");
    }
}

/// Start `job`'s export (or resume the saved one), wait for it, and bring
/// each of its types' files up to date from the manifest. Returns the types
/// whose merged count differs from the server's, to export again in full.
///
/// The saved status URL is cleared when the export failed, timed out (it is
/// cancelled) or was read; it stays when the server could not be reached or
/// a download failed, so the next run picks the same export up. A kick-off
/// the server answers with 429 is tried again until `--bulk-timeout`; the
/// wait counts towards that timeout, and nothing is saved until the server
/// accepts the kick-off.
fn run_job(
    client: &FhirClient,
    args: &ExportArgs,
    job: &Job,
    state: &mut ExportState,
    report: &mut Report,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(args.bulk_timeout);
    let names = job.types.join(", ");
    let what = match &job.since {
        Some(s) => format!("changes since {s}"),
        None => "in full".to_string(),
    };
    let (status_url, server_time, mut manifest) = loop {
        let (status_url, server_time, resumed) =
            match state.bulk.as_ref().filter(|p| job.matches(p)) {
                Some(p) => {
                    println!(
                        "{names}: resuming the bulk export ({what}) started at {}",
                        p.started_at
                    );
                    (p.status_url.clone(), p.server_time.clone(), true)
                }
                None => {
                    let Some(started) = bulk::kick_off(
                        client,
                        &args.server,
                        &job.types,
                        job.since.as_deref(),
                        deadline,
                    )?
                    else {
                        return Err(PipesError::Environment(format!(
                            "the server did not accept the bulk export kick-off within \
                             --bulk-timeout ({} s); no export was started",
                            args.bulk_timeout
                        )));
                    };
                    let url = started.status_url;
                    println!("{names}: bulk export ({what}), status at {url}");
                    state.bulk = Some(bulk::Pending {
                        status_url: url.clone(),
                        types: job.types.clone(),
                        since: job.since.clone(),
                        started_at: format_utc(SystemTime::now()),
                        server_time: started.server_time.clone(),
                    });
                    // Before waiting, so a run that stops now resumes this export.
                    state.write(&args.out)?;
                    (url, started.server_time, false)
                }
            };
        let failed = match bulk::wait(client, &status_url, deadline)? {
            bulk::Polled::Done(m) => break (status_url, server_time, m),
            bulk::Polled::Gone if resumed => {
                println!("the server no longer has the saved bulk export; starting another");
                // Written now, so a kick-off that times out does not leave
                // the gone export saved.
                state.bulk = None;
                state.write(&args.out)?;
                continue;
            }
            bulk::Polled::Gone => {
                format!(
                    "the server does not know the bulk export status URL {status_url} (404 or 410)"
                )
            }
            bulk::Polled::Failed(text) => format!("the bulk export failed: {text}"),
            bulk::Polled::TimedOut => {
                cancel(client, &status_url);
                format!(
                    "the bulk export did not finish within --bulk-timeout ({} s); cancelled it",
                    args.bulk_timeout
                )
            }
        };
        state.bulk = None;
        state.write(&args.out)?;
        return Err(PipesError::Environment(failed));
    };

    if args.server_kind == ServerKind::Medplum {
        bulk::read_references_from(&mut manifest, &args.server);
    }
    let timeout = Duration::from_secs(args.bulk_timeout);
    let rejected = match &manifest.transaction_time {
        None => Some("the bulk export's manifest has no transactionTime".to_string()),
        Some(_) => manifest_errors(client, &manifest, timeout, report)?,
    };
    if let Some(message) = rejected {
        cancel(client, &status_url);
        state.bulk = None;
        state.write(&args.out)?;
        return Err(PipesError::Environment(message));
    }
    let transaction_time = bulk::watermark(
        manifest.transaction_time.as_deref().unwrap_or_default(),
        server_time.as_deref(),
    );
    for f in manifest
        .output
        .iter()
        .filter(|f| !job.types.contains(&f.resource_type))
    {
        report.add(
            "bulk_output_unrequested",
            &f.resource_type,
            &format!("{}: not a type this export asked for; skipped", f.url),
        );
    }
    let deleted = deleted_ids(client, &manifest, &job.types, timeout, report)?;

    // Removed by the merges that read them, and on any failure.
    let _incoming = window::Remove(
        job.types
            .iter()
            .map(|t| incoming_path(&args.out, t))
            .collect(),
    );
    let mut downloaded = download_job(client, args, job, &manifest)?;
    let mut done = Vec::new();
    let mut redo = Vec::new();
    for t in &job.types {
        let out = ndjson_path(&args.out, t);
        let d = downloaded
            .remove(t)
            .expect("every type of the job was downloaded");
        let files = d.files;
        let stats = bulk_type(args, job, t, d, deleted.get(t), report)?;
        let (total, removed) = (stats.total, stats.removed);
        if job.since.is_some()
            && merged_count_differs(
                client,
                &args.server,
                args.server_kind,
                t,
                total,
                &out,
                report,
            )
        {
            redo.push(t.clone());
            continue;
        }
        match &job.since {
            Some(since) => println!(
                "{t}: {} new, {} updated or fetched again since {since}, {removed} deleted, {total} in all, {files} files -> {}",
                stats.added,
                stats.updated,
                out.display()
            ),
            None => println!("{t}: {total} resources, {files} files -> {}", out.display()),
        }
        done.push((t.clone(), type_state(Some(transaction_time.clone()), total)));
    }
    // A type to export again in full loses its watermark, so a run that
    // stops before that export still downloads it in full.
    for t in &redo {
        state.types.remove(t);
    }
    state.types.extend(done);
    state.bulk = None;
    state.write(&args.out)?;
    // The files are read: the server may remove them.
    let _ = bulk::delete(client, &status_url);
    Ok(redo)
}

/// Read the export's error files. Issues that are errors come back as one
/// message, which fails the run; warnings and information go in the report.
fn manifest_errors(
    client: &FhirClient,
    manifest: &bulk::Manifest,
    timeout: Duration,
    report: &mut Report,
) -> Result<Option<String>> {
    let mut errors: Vec<bulk::Issue> = Vec::new();
    for f in &manifest.error {
        bulk::read_lines(
            client,
            &f.url,
            manifest.requires_access_token,
            timeout,
            |line| {
                let found = serde_json::from_str::<Value>(line)
                    .map(|v| bulk::issues(&v))
                    .unwrap_or_default();
                if found.is_empty() {
                    // Not an OperationOutcome, but in an error file all the same.
                    errors.push(bulk::Issue {
                        severity: "error".into(),
                        code: "unknown".into(),
                        text: line.chars().take(500).collect(),
                    });
                }
                for issue in found {
                    if issue.is_error() {
                        errors.push(issue);
                    } else {
                        report.add("bulk_export_issue", &f.url, &issue.to_string());
                    }
                }
                Ok(())
            },
        )?;
    }
    if errors.is_empty() {
        return Ok(None);
    }
    const SHOWN: usize = 5;
    Ok(Some(format!(
        "the bulk export reported {} error(s): {}{}",
        errors.len(),
        bulk::join_issues(&errors[..errors.len().min(SHOWN)]),
        if errors.len() > SHOWN { "; …" } else { "" }
    )))
}

/// The ids the export's `deleted` Bundles remove, per type, for `types`.
fn deleted_ids(
    client: &FhirClient,
    manifest: &bulk::Manifest,
    types: &[String],
    timeout: Duration,
    report: &mut Report,
) -> Result<HashMap<String, HashSet<String>>> {
    let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
    for f in &manifest.deleted {
        let mut n = 0usize;
        bulk::read_lines(
            client,
            &f.url,
            manifest.requires_access_token,
            timeout,
            |line| {
                n += 1;
                let Ok(bundle) = serde_json::from_str::<Value>(line) else {
                    report.add(
                        "bulk_deleted_unparsed",
                        &format!("{} line {n}", f.url),
                        "not JSON",
                    );
                    return Ok(());
                };
                let entries = bundle.get("entry").and_then(Value::as_array);
                for entry in entries.into_iter().flatten() {
                    if let Some((t, id)) = bulk::deleted_entry(entry)
                        && types.contains(&t)
                    {
                        ids.entry(t).or_default().insert(id);
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(ids)
}

/// One type's output files, downloaded into its incoming file.
struct Downloaded {
    notes: Vec<PageNote>,
    files: usize,
    /// The lines skipped, per file in manifest order.
    report: Report,
}

/// Download every output file of `job`'s types, `--concurrency` at a time
/// through the client's limit, each into a part file of its own. Each
/// type's parts are then joined in manifest order into its incoming file,
/// so the file is the same as one downloaded a file at a time. The first
/// failure stops new downloads, and the parts are removed. No type's file is
/// touched here.
fn download_job(
    client: &FhirClient,
    args: &ExportArgs,
    job: &Job,
    manifest: &bulk::Manifest,
) -> Result<HashMap<String, Downloaded>> {
    let files: Vec<(&str, &str)> = manifest
        .output
        .iter()
        .filter(|f| job.types.contains(&f.resource_type))
        .map(|f| (f.resource_type.as_str(), f.url.as_str()))
        .collect();
    let parts = window::Remove(
        files
            .iter()
            .enumerate()
            .map(|(i, (t, _))| args.out.join(format!(".{t}.part-{i}.ndjson")))
            .collect(),
    );
    let timeout = Duration::from_secs(args.bulk_timeout);
    let failed = AtomicBool::new(false);
    let indexes: Vec<usize> = (0..files.len()).collect();
    let results = window::parallel(&indexes, args.concurrency, |&i| {
        if failed.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (_, url) = files[i];
        let got = download_part(
            client,
            url,
            manifest.requires_access_token,
            timeout,
            &parts.0[i],
        );
        if got.is_err() {
            failed.store(true, Ordering::Relaxed);
        }
        got.map(Some)
    });
    // Every download ran when none failed.
    let mut downloaded = results.into_iter().collect::<Result<Vec<_>>>()?;
    let mut by_type: HashMap<String, Downloaded> = HashMap::new();
    for t in &job.types {
        let mine: Vec<usize> = (0..files.len()).filter(|&i| files[i].0 == t).collect();
        let paths: Vec<PathBuf> = mine.iter().map(|&i| parts.0[i].clone()).collect();
        window::concatenate(&paths, &incoming_path(&args.out, t))?;
        let mut d = Downloaded {
            notes: Vec::new(),
            files: mine.len(),
            report: Report::default(),
        };
        for &i in &mine {
            let (notes, report) = downloaded[i].take().expect("every file was downloaded");
            d.notes.extend(notes);
            d.report.append(report);
        }
        by_type.insert(t.clone(), d);
    }
    Ok(by_type)
}

/// Merge one type's downloaded files into OUT/<TYPE>.ndjson by id
/// (replacing it, for a full export), leaving out, in the same pass, the
/// resources listed as deleted. Returns the merge's numbers, with the
/// resources removed.
fn bulk_type(
    args: &ExportArgs,
    job: &Job,
    resource_type: &str,
    downloaded: Downloaded,
    deleted: Option<&HashSet<String>>,
    report: &mut Report,
) -> Result<MergeStats> {
    report.append(downloaded.report);
    // A resource in this export's output existed at its transactionTime,
    // whatever the deleted list says; the merge keeps every id it reads.
    let gone: HashSet<&str> = deleted.into_iter().flatten().map(String::as_str).collect();
    let existing = ndjson_path(&args.out, resource_type);
    let incoming = incoming_path(&args.out, resource_type);
    if job.since.is_none() {
        changes::record_replaced(&args.out, resource_type, &existing, &incoming)?;
    } else {
        changes::record(
            &args.out,
            resource_type,
            downloaded.notes.iter().map(|n| n.id.as_str()),
            gone.iter().copied(),
        )?;
    }
    merge_file(
        &MergeFiles {
            existing,
            tmp: tmp_path(&args.out, resource_type),
            incoming,
            unparsed_kind: "export_line_unparsed",
        },
        &downloaded.notes,
        &MergeOptions {
            full: job.since.is_none(),
            drop: Some(&gone),
            boundaries: None,
        },
        report,
    )
}

/// Write every line of the file at `url` to `part`, noting each resource's
/// id and meta.lastUpdated for the merge. A line without an id is reported
/// and left out.
fn download_part(
    client: &FhirClient,
    url: &str,
    authorize: bool,
    timeout: Duration,
    part: &Path,
) -> Result<(Vec<PageNote>, Report)> {
    let file = std::fs::File::create(part).map_err(|e| PipesError::io(part, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut notes = Vec::new();
    let mut report = Report::default();
    let mut n = 0usize;
    bulk::read_lines(client, url, authorize, timeout, |line| {
        n += 1;
        let Some((id, last_updated)) = id_and_updated(line) else {
            report.add(
                "bulk_line_skipped",
                &format!("{url} line {n}"),
                "not a JSON resource with an id",
            );
            return Ok(());
        };
        notes.push(PageNote {
            id,
            last_updated,
            boundary_url: None,
        });
        writeln!(out, "{line}").map_err(|e| PipesError::io(part, e))
    })?;
    out.flush().map_err(|e| PipesError::io(part, e))?;
    Ok((notes, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_file_that_cannot_be_read_means_a_full_download() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [&b"{nope"[..], &b"\xff\xfe"[..]] {
            std::fs::write(dir.path().join(STATE_FILE), bytes).unwrap();
            let state = ExportState::read(dir.path());
            assert!(state.server.is_empty() && state.types.is_empty());
        }
    }

    #[test]
    fn type_names_are_plain_words() {
        assert!(valid_type("MeasureReport"));
        assert!(valid_type("Location"));
        assert!(!valid_type(""));
        assert!(!valid_type("measureReport"));
        assert!(!valid_type("../Location"));
        assert!(!valid_type("Location?_count=1"));
    }
}
