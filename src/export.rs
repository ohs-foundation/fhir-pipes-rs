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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cli::ExportArgs;
use crate::error::{PipesError, Result};
use crate::extract::client::FhirClient;
use crate::extract::page::{count_resources, page_resources};
use crate::report::Report;
use crate::snapshot::instant::{capped_watermark, format_utc, later};
use crate::snapshot::merge::{MergeFiles, merge_file};
use crate::snapshot::same_server;
use crate::transform::write_report;

const STATE_FILE: &str = ".export-state.json";

/// What the last export into a directory downloaded, per resource type.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportState {
    server: String,
    #[serde(default)]
    types: BTreeMap<String, TypeState>,
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
            types: BTreeMap::new(),
        };
    }
    let mut report = Report::default();
    for t in types {
        let previous = state.types.get(t).filter(|_| args.incremental).cloned();
        let done = export_type(&client, args, t, previous.as_ref(), &mut report)?;
        state.types.insert(t.to_string(), done);
        // After each type, so an interrupted run keeps what it finished.
        state.write(&args.out)?;
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
    let out = args.out.join(format!("{resource_type}.ndjson"));
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
    let incoming = args.out.join(format!(".{resource_type}.incoming.ndjson"));
    let paged = match page_resources(
        client,
        &args.server,
        resource_type,
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
    )?;
    match count_resources(client, &args.server, resource_type) {
        Some(server_total) if server_total as usize != stats.total => {
            println!(
                "{resource_type}: the server has {server_total} resources and the merged file {}; \
                 downloading in full (a resource was deleted, or the file had drifted)",
                stats.total
            );
            report.add(
                "incremental_count_mismatch",
                resource_type,
                &format!(
                    "the server reports {server_total} {resource_type} resources but the merged file has {}; downloaded in full",
                    stats.total
                ),
            );
            return Ok(None);
        }
        Some(_) => {}
        None => eprintln!(
            "count check unavailable for {resource_type}: the server returned no total, \
             so resources deleted on the server stay in {}",
            out.display()
        ),
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
    let paged = match page_resources(client, &args.server, resource_type, None, &tmp, report) {
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
