//! `export --bulk`: which `$export`s a run asks for, and how each one's
//! manifest brings the files up to date. The protocol itself is in `bulk`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::page::{PageNote, ServerKind};
use crate::http::FhirClient;
use crate::report::Report;
use crate::snapshot::instant::{format_utc, parse_instant};
use crate::snapshot::merge::{MergeFiles, MergeOptions, MergeStats, id_and_updated, merge_file};

use super::{
    ExportState, bulk, changes, incoming_path, merged_count_differs, ndjson_path, tmp_path,
    type_state,
};

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
pub(super) fn export_bulk(
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
pub(super) fn cancel(client: &FhirClient, status_url: &str) {
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
    let finished = start_or_resume(client, args, job, state)?;
    apply_manifest(client, args, job, finished, state, report)
}

/// A finished export: its status URL, the server's clock at the kick-off,
/// and its manifest.
struct Finished {
    status_url: String,
    server_time: Option<String>,
    manifest: bulk::Manifest,
}

/// Start `job`'s export, or resume the saved one, and wait until it is done.
/// A saved export the server no longer has is replaced by a new one; an
/// export that failed or timed out (and is then cancelled) clears the saved
/// status URL and fails the run.
fn start_or_resume(
    client: &FhirClient,
    args: &ExportArgs,
    job: &Job,
    state: &mut ExportState,
) -> Result<Finished> {
    let deadline = Instant::now() + Duration::from_secs(args.bulk_timeout);
    let names = job.types.join(", ");
    let what = match &job.since {
        Some(s) => format!("changes since {s}"),
        None => "in full".to_string(),
    };
    loop {
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
            bulk::Polled::Done(manifest) => {
                return Ok(Finished {
                    status_url,
                    server_time,
                    manifest,
                });
            }
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
    }
}

/// Bring each of `job`'s types' files up to date from the finished export's
/// manifest; see `run_job`. A manifest with no transactionTime, or with
/// errors, cancels the export and fails the run.
fn apply_manifest(
    client: &FhirClient,
    args: &ExportArgs,
    job: &Job,
    finished: Finished,
    state: &mut ExportState,
    report: &mut Report,
) -> Result<Vec<String>> {
    let Finished {
        status_url,
        server_time,
        mut manifest,
    } = finished;
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
            &format!(
                "{}: not a type this export asked for; skipped",
                crate::redact::url(&f.url)
            ),
        );
    }
    let deleted = deleted_ids(client, &manifest, &job.types, timeout, report)?;

    // Removed by the merges that read them, and on any failure.
    let _incoming = crate::fs::RemoveOnDrop(
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
                        report.add(
                            "bulk_export_issue",
                            &crate::redact::url(&f.url),
                            &issue.to_string(),
                        );
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
    let parts = crate::fs::RemoveOnDrop(
        files
            .iter()
            .enumerate()
            .map(|(i, (t, _))| args.out.join(format!(".{t}.part-{i}.ndjson")))
            .collect(),
    );
    let timeout = Duration::from_secs(args.bulk_timeout);
    let failed = AtomicBool::new(false);
    let indexes: Vec<usize> = (0..files.len()).collect();
    let results = crate::work::parallel(&indexes, args.concurrency, |&i| {
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
        crate::fs::concatenate(&paths, &incoming_path(&args.out, t))?;
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
    let file = crate::fs::create_private(part).map_err(|e| PipesError::io(part, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut notes = Vec::new();
    let mut report = Report::default();
    let mut n = 0usize;
    bulk::read_lines(client, url, authorize, timeout, |line| {
        n += 1;
        let Some((id, last_updated)) = id_and_updated(line) else {
            report.add(
                "bulk_line_skipped",
                &format!("{} line {n}", crate::redact::url(url)),
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
