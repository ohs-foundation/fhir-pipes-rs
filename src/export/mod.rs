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
//! `transactionTime`. The count check after a merge is the same as above.

mod bulk;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::client::FhirClient;
use crate::extract::page::{PageNote, count_resources, page_resources};
use crate::fhir::ndjson::NdjsonReader;
use crate::report::Report;
use crate::snapshot::instant::{capped_watermark, format_utc, later, parse_instant};
use crate::snapshot::merge::{MergeFiles, MergeStats, id_and_updated, merge_file};
use crate::snapshot::same_server;
use crate::transform::write_report;

const STATE_FILE: &str = ".export-state.json";

/// What the last export into a directory downloaded, per resource type.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportState {
    server: String,
    #[serde(default)]
    types: BTreeMap<String, TypeState>,
    /// A Bulk Data export started and not yet finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bulk: Option<bulk::Pending>,
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
    /// The state in `dir`, or an empty one. An unreadable state file means
    /// a full download, not a failed run.
    fn read(dir: &Path) -> Self {
        let path = dir.join(STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!(
                    "{}: not a valid export state ({e}); downloading in full",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Written beside the target and renamed into place.
    fn write(&self, dir: &Path) -> Result<()> {
        let path = dir.join(STATE_FILE);
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(&tmp, text).map_err(|e| PipesError::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| PipesError::io(&path, e))
    }
}

/// A FHIR resource type name: an upper-case letter, then letters. It becomes
/// a file name, so nothing else is accepted.
fn valid_type(t: &str) -> bool {
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
    std::fs::create_dir_all(&args.out)
        .map_err(|e| PipesError::Usage(format!("--out {}: {e}", args.out.display())))?;
    let client = FhirClient::new(
        args.auth.auth(),
        args.retries,
        Duration::from_secs(args.timeout),
    )?;
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
        for t in types {
            let previous = state.types.get(t).filter(|_| args.incremental).cloned();
            let done = export_type(&client, args, t, previous.as_ref(), &mut report)?;
            state.types.insert(t.to_string(), done);
            // After each type, so an interrupted run keeps what it finished.
            state.write(&args.out)?;
        }
    }
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    println!("{}", report.summary());
    Ok(())
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
    let stats = merge_file(
        &MergeFiles {
            existing: out.to_path_buf(),
            tmp: tmp_path(&args.out, resource_type),
            incoming,
            unparsed_kind: "export_line_unparsed",
        },
        &paged.notes,
        false,
        &|_| None,
        &HashMap::new(),
        report,
        &[],
    )?;
    if merged_count_differs(
        client,
        &args.server,
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
/// that gives no count is taken at its word, with a warning.
fn merged_count_differs(
    client: &FhirClient,
    server: &str,
    resource_type: &str,
    total: usize,
    out: &Path,
    report: &mut Report,
) -> bool {
    match count_resources(client, server, resource_type) {
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
    let tmp = tmp_path(&args.out, resource_type);
    let paged = match page_resources(client, &args.server, resource_type, None, &tmp, report, &[]) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, out).map_err(|e| PipesError::io(out, e))?;
    let total = paged.notes.len();
    // As in extract: a last page that was full with no next link may mean
    // the server stopped paging early, so only then is the count worth a
    // request (a count over a large table is not free).
    if paged.last_page_full {
        match count_resources(client, &args.server, resource_type) {
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
    println!(
        "{resource_type}: {total} resources, {} pages -> {}",
        paged.pages,
        out.display()
    );
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
/// a download failed, so the next run picks the same export up.
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
    let (status_url, manifest) = loop {
        let (status_url, resumed) = match state.bulk.as_ref().filter(|p| job.matches(p)) {
            Some(p) => {
                println!(
                    "{names}: resuming the bulk export ({what}) started at {}",
                    p.started_at
                );
                (p.status_url.clone(), true)
            }
            None => {
                let url = bulk::kick_off(client, &args.server, &job.types, job.since.as_deref())?;
                println!("{names}: bulk export ({what}), status at {url}");
                state.bulk = Some(bulk::Pending {
                    status_url: url.clone(),
                    types: job.types.clone(),
                    since: job.since.clone(),
                    started_at: format_utc(SystemTime::now()),
                });
                // Before waiting, so a run that stops now resumes this export.
                state.write(&args.out)?;
                (url, false)
            }
        };
        let failed = match bulk::wait(client, &status_url, deadline)? {
            bulk::Polled::Done(m) => break (status_url, m),
            bulk::Polled::Gone if resumed => {
                println!("the server no longer has the saved bulk export; starting another");
                state.bulk = None;
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
    let transaction_time = manifest.transaction_time.clone().unwrap_or_default();
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

    let mut done = Vec::new();
    let mut redo = Vec::new();
    for t in &job.types {
        let out = ndjson_path(&args.out, t);
        let (stats, removed, files) =
            bulk_type(client, args, job, t, &manifest, deleted.get(t), report)?;
        let total = stats.total - removed;
        if job.since.is_some() && merged_count_differs(client, &args.server, t, total, &out, report)
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

/// Download one type's output files and merge them into OUT/<TYPE>.ndjson by
/// id (replacing it, for a full export), then remove the resources listed
/// as deleted. Returns the merge's numbers, how many resources were
/// removed, and how many files were read.
fn bulk_type(
    client: &FhirClient,
    args: &ExportArgs,
    job: &Job,
    resource_type: &str,
    manifest: &bulk::Manifest,
    deleted: Option<&HashSet<String>>,
    report: &mut Report,
) -> Result<(MergeStats, usize, usize)> {
    let out = ndjson_path(&args.out, resource_type);
    let tmp = tmp_path(&args.out, resource_type);
    let incoming = incoming_path(&args.out, resource_type);
    let files: Vec<&str> = manifest
        .output
        .iter()
        .filter(|f| f.resource_type == resource_type)
        .map(|f| f.url.as_str())
        .collect();
    let timeout = Duration::from_secs(args.bulk_timeout);
    let notes = match download_outputs(
        client,
        &files,
        manifest.requires_access_token,
        timeout,
        &incoming,
        report,
    ) {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&incoming);
            return Err(e);
        }
    };
    let stats = merge_file(
        &MergeFiles {
            existing: out.clone(),
            tmp: tmp.clone(),
            incoming,
            unparsed_kind: "export_line_unparsed",
        },
        &notes,
        job.since.is_none(),
        &|_| None,
        &HashMap::new(),
        report,
        &[],
    )?;
    let removed = match deleted {
        Some(ids) => {
            // A resource in this export's output existed at its
            // transactionTime, whatever the deleted list says.
            let exported: HashSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();
            let gone: HashSet<&str> = ids
                .iter()
                .map(String::as_str)
                .filter(|id| !exported.contains(id))
                .collect();
            remove_ids(&out, &tmp, &gone)?
        }
        None => 0,
    };
    Ok((stats, removed, files.len()))
}

/// Write every line of `files` to `incoming`, noting each resource's id and
/// meta.lastUpdated for the merge. A line without an id is reported and
/// left out.
fn download_outputs(
    client: &FhirClient,
    files: &[&str],
    authorize: bool,
    timeout: Duration,
    incoming: &Path,
    report: &mut Report,
) -> Result<Vec<PageNote>> {
    let file = std::fs::File::create(incoming).map_err(|e| PipesError::io(incoming, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut notes = Vec::new();
    for url in files {
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
            writeln!(out, "{line}").map_err(|e| PipesError::io(incoming, e))
        })?;
    }
    out.flush().map_err(|e| PipesError::io(incoming, e))?;
    Ok(notes)
}

/// Rewrite `path` without the resources whose id is in `ids`, through `tmp`.
/// Returns how many were removed.
fn remove_ids(path: &Path, tmp: &Path, ids: &HashSet<&str>) -> Result<usize> {
    if ids.is_empty() || !path.exists() {
        return Ok(0);
    }
    let result = (|| {
        let file = std::fs::File::create(tmp).map_err(|e| PipesError::io(tmp, e))?;
        let mut out = std::io::BufWriter::new(file);
        let mut removed = 0usize;
        for line in NdjsonReader::open(path)? {
            let line = line?;
            if id_and_updated(&line.text).is_some_and(|(id, _)| ids.contains(id.as_str())) {
                removed += 1;
                continue;
            }
            writeln!(out, "{}", line.text).map_err(|e| PipesError::io(tmp, e))?;
        }
        out.flush().map_err(|e| PipesError::io(tmp, e))?;
        drop(out);
        std::fs::rename(tmp, path).map_err(|e| PipesError::io(path, e))?;
        Ok(removed)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
