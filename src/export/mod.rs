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
//! `bulk_run`, and `bulk` for the protocol). Types with a watermark are
//! exported together with `_since` the earliest of their watermarks and
//! merged by id; types without one are exported in full in a second export. The export's `deleted` resources are
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
mod bulk_run;
pub(crate) mod changes;
#[cfg(feature = "postgres")]
mod hapi_db;
mod window;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::page::{
    CountCheck, PageResult, Search, ServerKind, check_count, count_resources, next_link,
    page_resources, page_search,
};
use crate::fhir::valid_type;
use crate::http::FhirClient;
use crate::report::{Report, write_report};
use crate::snapshot::instant::{capped_watermark, format_utc, later};
use crate::snapshot::merge::{MergeFiles, MergeOptions, merge_file};
use crate::snapshot::same_server;

use bulk_run::{cancel, export_bulk};

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

pub fn run_export(args: &ExportArgs) -> anyhow::Result<()> {
    let mut types: Vec<&str> = Vec::new();
    for t in &args.types {
        if !valid_type(t) {
            return Err(PipesError::Usage(format!(
                "--type {t}: not a FHIR resource type name (e.g. MeasureReport)"
            ))
            .into());
        }
        if !types.contains(&t.as_str()) {
            types.push(t);
        }
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()).into());
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
    .for_server(&args.server)
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
        &Search::new(&args.server, args.server_kind, resource_type),
        Some(since),
        &incoming,
        report,
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
    match check_count(client, server, kind, resource_type, total) {
        CountCheck::Differs(server_total) => {
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
        CountCheck::Same => false,
        CountCheck::Unavailable(why) => {
            eprintln!(
                "count check unavailable for {resource_type}: {why}, \
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
        match check_count(
            client,
            &args.server,
            args.server_kind,
            resource_type,
            total,
        ) {
            CountCheck::Differs(server_total) => report.add(
                "count_mismatch",
                resource_type,
                &format!(
                    "the server reports {server_total} {resource_type} resources but the export paged {total}; \
                     the server stopped paging early (no next link) or its count is stale"
                ),
            ),
            CountCheck::Same => {}
            CountCheck::Unavailable(why) => {
                eprintln!("count check unavailable for {resource_type}: {why}")
            }
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
        &Search::new(&args.server, kind, resource_type),
        tmp,
        &mut issues,
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
                &Search::new(&args.server, kind, resource_type),
                None,
                tmp,
                report,
            )?;
            Ok((paged, None))
        }
    }
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
}
