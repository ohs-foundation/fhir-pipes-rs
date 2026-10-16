//! `fhir-pipes sync`: copy the per-type NDJSON an `export` wrote to a second
//! FHIR server (the sink), as transaction bundles of `PUT <Type>/<id>`.
//!
//! Unlike `load`, the PUTs carry no `ifMatch`, and `meta.versionId` and
//! `meta.lastUpdated` are dropped: they are the source server's, and the sink
//! sets its own. The rest of `meta` (tags, profiles, security labels) is kept.
//!
//! Only what changed is sent. `state` keeps, per sink and type, the id and
//! `meta.lastUpdated` of every resource the sink has from earlier syncs; a
//! resource is sent when its id is new or its `meta.lastUpdated` differs (or
//! it has none). A resource is recorded only once the bundle holding it is
//! committed, so after a failed bundle the next run sends again everything
//! that was not written. With `--delete`, ids recorded that are no longer in
//! the download are sent as `DELETE <Type>/<id>`, unless that is more than
//! `--max-delete-share` of the type's synced resources.
//!
//! Every type is read and checked before any request. Within a type,
//! resources go parents first by `partOf` (as `load` orders them), one depth
//! at a time; across types, in the order given. Deletions come after every
//! PUT, the types in reverse order, so that a resource is deleted before the
//! resources it references. A file is read once to plan and once per
//! `partOf` depth to send, and at most a window of bundles is held in memory.

mod state;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::cli::SyncArgs;
use crate::error::{PipesError, Result};
use crate::export::valid_type;
use crate::extract::client::FhirClient;
use crate::fhir::ndjson::NdjsonReader;
use crate::load::bundle::{Bundle, plan, plan_deletes};
use crate::load::order::{depths, parent_key};
use crate::load::{Progress, post_all, preflight};
use crate::snapshot::instant::{format_utc, later};
use state::{SyncState, Synced, TypeState};

/// Bundles held and posted together, per worker. A window ends with every
/// bundle in it committed, so a larger one waits less often.
const WINDOW_BUNDLES_PER_WORKER: usize = 4;

/// Delete ids listed in a dry run, per type.
const DRY_RUN_LISTED: usize = 10;

/// A resource to send.
struct Changed {
    id: String,
    /// meta.lastUpdated, "" when none.
    updated: String,
    /// Its line in the file.
    line: usize,
}

/// One type's plan, made before any request.
struct TypePlan {
    resource_type: String,
    path: PathBuf,
    changed: Vec<Changed>,
    /// Depth by `partOf` among the changed resources, per `changed` entry.
    depths: Vec<usize>,
    unchanged: usize,
    /// Synced earlier and no longer in the file, sorted.
    gone: Vec<String>,
    /// Resources the sink had from earlier syncs.
    synced: usize,
}

/// What happened to one type.
#[derive(Default)]
struct Outcome {
    sent: usize,
    deleted: usize,
    failed: usize,
    /// Gone from the file, not deleted: no `--delete`.
    kept: usize,
    /// Gone from the file, not deleted: more than `--max-delete-share`.
    refused: usize,
    error: Option<String>,
}

/// A FHIR id: 1 to 64 letters, digits, `-` and `.`. It goes into a URL and
/// the state file, so nothing else is accepted.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn last_updated(resource: &Value) -> &str {
    resource
        .get("meta")
        .and_then(|m| m.get("lastUpdated"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Drop `meta.versionId` and `meta.lastUpdated`, and `meta` when nothing
/// else is in it.
fn strip_version(resource: &mut Value) {
    let Some(obj) = resource.as_object_mut() else {
        return;
    };
    let empty = match obj.get_mut("meta").and_then(Value::as_object_mut) {
        Some(meta) => {
            meta.remove("versionId");
            meta.remove("lastUpdated");
            meta.is_empty()
        }
        None => false,
    };
    if empty {
        obj.remove("meta");
    }
}

/// The types to sync and their files: `--type` in the order given, else
/// every `<TYPE>.ndjson` in the directory, by name.
fn types(args: &SyncArgs) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    if args.types.is_empty() {
        let entries = std::fs::read_dir(&args.from).map_err(|e| PipesError::io(&args.from, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| PipesError::io(&args.from, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(t) = name.strip_suffix(".ndjson")
                && valid_type(t)
                && entry.path().is_file()
            {
                out.push((t.to_string(), entry.path()));
            }
        }
        out.sort();
        if out.is_empty() {
            return Err(PipesError::Usage(format!(
                "--from {}: no <TYPE>.ndjson files; run export into it first",
                args.from.display()
            )));
        }
        return Ok(out);
    }
    for t in &args.types {
        if !valid_type(t) {
            return Err(PipesError::Usage(format!(
                "--type {t}: not a FHIR resource type name (e.g. Patient)"
            )));
        }
        if out.iter().any(|(o, _)| o == t) {
            continue;
        }
        let path = args.from.join(format!("{t}.ndjson"));
        if !path.is_file() {
            return Err(PipesError::Usage(format!(
                "--type {t}: {} not found; export the type into --from first",
                path.display()
            )));
        }
        out.push((t.clone(), path));
    }
    Ok(out)
}

/// Read one type's file: check every line, and find what changed since
/// `synced` and what is gone from it.
fn plan_type(resource_type: &str, path: &Path, synced: &Synced, full: bool) -> Result<TypePlan> {
    let mut present: HashSet<String> = HashSet::new();
    let mut changed: Vec<Changed> = Vec::new();
    let mut parents: Vec<Option<String>> = Vec::new();
    let mut unchanged = 0usize;
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let at = |what: &str| {
            PipesError::Usage(format!("{}: line {}: {what}", path.display(), line.number))
        };
        let value: Value = serde_json::from_str(&line.text).map_err(|e| at(&e.to_string()))?;
        if value.get("resourceType").and_then(Value::as_str) != Some(resource_type) {
            return Err(at(&format!("not a {resource_type}")));
        }
        let id = value.get("id").and_then(Value::as_str).unwrap_or("");
        if !valid_id(id) {
            return Err(at(&format!(
                "id {id:?} is not a FHIR id (1 to 64 letters, digits, '-' and '.')"
            )));
        }
        if !present.insert(id.to_string()) {
            return Err(at(&format!("duplicate resource {resource_type}/{id}")));
        }
        let updated = last_updated(&value);
        let same = !updated.is_empty() && synced.get(id).is_some_and(|s| s == updated);
        if same && !full {
            unchanged += 1;
            continue;
        }
        // One type per file: a parent of another type is not in this set.
        parents.push(
            parent_key(&value)
                .filter(|(t, _)| *t == resource_type)
                .map(|(_, id)| id.to_string()),
        );
        changed.push(Changed {
            id: id.to_string(),
            updated: updated.to_string(),
            line: line.number,
        });
    }
    let by_id: HashMap<&str, usize> = changed
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    let parents: Vec<Option<usize>> = parents
        .iter()
        .map(|p| p.as_deref().and_then(|p| by_id.get(p).copied()))
        .collect();
    let label = |i: usize| format!("{resource_type}/{}", changed[i].id);
    let depths = depths(&parents, &label)?;
    let mut gone: Vec<String> = synced
        .keys()
        .filter(|id| !present.contains(*id))
        .cloned()
        .collect();
    gone.sort_unstable();
    Ok(TypePlan {
        resource_type: resource_type.to_string(),
        path: path.to_path_buf(),
        changed,
        depths,
        unchanged,
        gone,
        synced: synced.len(),
    })
}

/// More than `--max-delete-share` of the synced resources gone, without
/// `--force-delete`.
fn refuses_deletes(args: &SyncArgs, p: &TypePlan) -> bool {
    !args.force_delete
        && !p.gone.is_empty()
        && p.gone.len() as f64 > args.max_delete_share * p.synced as f64
}

fn refusal(args: &SyncArgs, p: &TypePlan) -> String {
    format!(
        "{}: {} of {} synced resources ({:.0}%) are gone from {}; that is more than \
         --max-delete-share {}, so none were deleted. Check the download, or pass --force-delete",
        p.resource_type,
        p.gone.len(),
        p.synced,
        100.0 * p.gone.len() as f64 / p.synced as f64,
        p.path.display(),
        args.max_delete_share
    )
}

fn summary(synced: &Synced) -> TypeState {
    TypeState {
        count: synced.len(),
        newest: synced.values().fold(None, |w, u| later(w.as_deref(), u)),
        synced_at: format_utc(SystemTime::now()),
        fhir_pipes_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Posts one bundle; a sync bundle has no version checks, so any failure
/// is reported as the server gave it.
fn post_one(
    client: &FhirClient,
    base: &str,
    b: &Bundle,
    n: usize,
    count: usize,
    label: &str,
) -> Result<()> {
    let body = b.to_json().to_string().into_bytes();
    client.post_json(base, body).map(|_| ()).map_err(|e| {
        PipesError::Environment(format!(
            "{label}bundle {n}/{count} failed: {e}; nothing in it was written"
        ))
    })
}

/// Post `bundles`, hand each committed one to `committed`, and return the
/// first failure, if any.
fn post_window(
    client: &FhirClient,
    base: &str,
    args: &SyncArgs,
    bundles: &[Bundle],
    progress: &Progress,
    mut committed: impl FnMut(&Bundle),
) -> Option<PipesError> {
    let label = progress.label;
    let post = |c: &FhirClient, b: &str, bundle: &Bundle, n: usize, count: usize| {
        post_one(c, b, bundle, n, count, label)
    };
    let posted = post_all(client, base, bundles, args.concurrency, progress, &post);
    for (b, ok) in bundles.iter().zip(&posted.committed) {
        if *ok {
            committed(b);
        }
    }
    posted.failure
}

/// Send one type's changed resources, parents first, and record each
/// committed one in the sink's synced copy.
fn send_type(
    client: &FhirClient,
    base: &str,
    args: &SyncArgs,
    p: &TypePlan,
    state: &mut SyncState,
    out: &mut Outcome,
) -> Result<()> {
    if p.changed.is_empty() {
        return Ok(());
    }
    let mut synced = state.synced(&args.from, base, &p.resource_type)?;
    let index: HashMap<&str, usize> = p
        .changed
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    let max_depth = p.depths.iter().copied().max().unwrap_or(0);
    let mut per_depth = vec![0usize; max_depth + 1];
    for &d in &p.depths {
        per_depth[d] += 1;
    }
    let batch = args.batch_size;
    let total: usize = per_depth.iter().map(|n| n.div_ceil(batch)).sum();
    let window = batch * args.concurrency * WINDOW_BUNDLES_PER_WORKER;
    let label = format!("{}: ", p.resource_type);
    let mut first = 0usize;
    let mut failure: Option<PipesError> = None;
    'levels: for depth in 0..=max_depth {
        // The lines to send at this depth, in file order.
        let mut lines: Vec<usize> = p
            .changed
            .iter()
            .zip(&p.depths)
            .filter(|&(_, &d)| d == depth)
            .map(|(c, _)| c.line)
            .collect();
        lines.sort_unstable();
        let mut wanted = lines.into_iter().peekable();
        let mut pending: Vec<Value> = Vec::new();
        let mut reader = NdjsonReader::open(&p.path)?;
        loop {
            let line = match wanted.peek() {
                None => None,
                Some(&n) => loop {
                    match reader.next() {
                        Some(Ok(l)) if l.number == n => break Some(l),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return Err(e),
                        None => {
                            return Err(PipesError::Environment(format!(
                                "{} changed while it was being sent (line {n} is gone)",
                                p.path.display()
                            )));
                        }
                    }
                },
            };
            if let Some(line) = &line {
                wanted.next();
                let mut value: Value = serde_json::from_str(&line.text)?;
                strip_version(&mut value);
                pending.push(value);
            }
            if pending.len() == window || (line.is_none() && !pending.is_empty()) {
                let bundles = plan(std::mem::take(&mut pending), batch);
                let progress = Progress {
                    label: &label,
                    first,
                    total,
                };
                first += bundles.len();
                failure = post_window(client, base, args, &bundles, &progress, |b| {
                    for e in &b.entries {
                        if let Some(&i) = index.get(e.id.as_str()) {
                            synced.insert(e.id.clone(), p.changed[i].updated.clone());
                        }
                        out.sent += 1;
                    }
                });
                if failure.is_some() {
                    break 'levels;
                }
            }
            if line.is_none() {
                break;
            }
        }
    }
    out.failed += p.changed.len() - out.sent;
    if let Some(e) = failure {
        out.error = Some(e.to_string());
    }
    if out.sent > 0 {
        let s = summary(&synced);
        state.save(&args.from, base, &p.resource_type, &synced, s)?;
    }
    Ok(())
}

/// Delete one type's gone resources and drop them from the synced copy.
fn delete_type(
    client: &FhirClient,
    base: &str,
    args: &SyncArgs,
    p: &TypePlan,
    state: &mut SyncState,
    out: &mut Outcome,
) -> Result<()> {
    let bundles = plan_deletes(&p.resource_type, &p.gone, args.batch_size);
    let label = format!("{}: delete ", p.resource_type);
    let progress = Progress {
        label: &label,
        first: 0,
        total: bundles.len(),
    };
    let mut deleted: Vec<String> = Vec::new();
    let failure = post_window(client, base, args, &bundles, &progress, |b| {
        deleted.extend(b.deletes.iter().map(|(_, id)| id.clone()));
    });
    out.deleted = deleted.len();
    out.failed += p.gone.len() - deleted.len();
    if let Some(e) = failure
        && out.error.is_none()
    {
        out.error = Some(e.to_string());
    }
    if !deleted.is_empty() {
        let mut synced = state.synced(&args.from, base, &p.resource_type)?;
        for id in &deleted {
            synced.remove(id);
        }
        let s = summary(&synced);
        state.save(&args.from, base, &p.resource_type, &synced, s)?;
    }
    Ok(())
}

fn print_dry_run(args: &SyncArgs, base: &str, plans: &[TypePlan]) {
    println!("Dry run: nothing sent to {base}");
    for p in plans {
        let mut line = format!(
            "{}: {} to send, {} unchanged",
            p.resource_type,
            p.changed.len(),
            p.unchanged
        );
        if !p.gone.is_empty() {
            if !args.delete {
                line += &format!(
                    ", {} gone from the download (not deleted without --delete)",
                    p.gone.len()
                );
            } else if refuses_deletes(args, p) {
                line += &format!(", {} gone from the download", p.gone.len());
            } else {
                let mut listed: Vec<&str> = p
                    .gone
                    .iter()
                    .take(DRY_RUN_LISTED)
                    .map(String::as_str)
                    .collect();
                if p.gone.len() > DRY_RUN_LISTED {
                    listed.push("…");
                }
                line += &format!(", {} to delete: {}", p.gone.len(), listed.join(", "));
            }
        }
        println!("{line}");
        if args.delete && refuses_deletes(args, p) {
            println!("{}", refusal(args, p));
        }
    }
}

pub fn run_sync(args: &SyncArgs) -> Result<()> {
    if args.batch_size == 0 {
        return Err(PipesError::Usage("--batch-size must be at least 1".into()));
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()));
    }
    if !args.from.is_dir() {
        return Err(PipesError::Usage(format!(
            "--from {}: not a directory",
            args.from.display()
        )));
    }
    let base = args.server.trim_end_matches('/').to_string();
    let types = types(args)?;
    let mut state = SyncState::read(&args.from)?;
    // Every file is read and checked before any request: a bad line should
    // not leave the sink with half the types.
    let mut plans: Vec<TypePlan> = Vec::new();
    for (t, path) in &types {
        let synced = state.synced(&args.from, &base, t)?;
        plans.push(plan_type(t, path, &synced, args.full)?);
    }

    let client = FhirClient::new(
        args.auth.auth()?,
        args.retries,
        Duration::from_secs(args.timeout),
    )?;
    let creates: Vec<String> = plans
        .iter()
        .filter(|p| !p.changed.is_empty())
        .map(|p| p.resource_type.clone())
        .collect();
    preflight(&client, &base, &creates)?;

    if args.dry_run {
        print_dry_run(args, &base, &plans);
        return Ok(());
    }

    let mut outcomes: Vec<Outcome> = plans.iter().map(|_| Outcome::default()).collect();
    for (p, out) in plans.iter().zip(outcomes.iter_mut()) {
        send_type(&client, &base, args, p, &mut state, out)?;
    }
    // Deletions after every PUT, the last type first.
    for (p, out) in plans.iter().zip(outcomes.iter_mut()).rev() {
        if p.gone.is_empty() {
            continue;
        }
        if !args.delete {
            out.kept = p.gone.len();
        } else if refuses_deletes(args, p) {
            out.refused = p.gone.len();
        } else {
            delete_type(&client, &base, args, p, &mut state, out)?;
        }
    }

    let mut failed: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for (p, out) in plans.iter().zip(&outcomes) {
        let mut line = format!(
            "{}: {} sent, {} unchanged, {} deleted, {} failed",
            p.resource_type, out.sent, p.unchanged, out.deleted, out.failed
        );
        if out.kept > 0 {
            line += &format!(
                "; {} gone from the download, not deleted without --delete",
                out.kept
            );
        }
        println!("{line}");
        if out.refused > 0 {
            println!("{}", refusal(args, p));
            refused.push(p.resource_type.clone());
        }
        if let Some(e) = &out.error {
            failed.push(e.clone());
        }
    }
    if !failed.is_empty() {
        return Err(PipesError::Environment(format!(
            "sync to {base} incomplete: {}\n\
             Resources not written are sent again by the next run.",
            failed.join("\n")
        )));
    }
    if !refused.is_empty() {
        return Err(PipesError::Usage(format!(
            "sync to {base}: deletions refused for {}; see above",
            refused.join(", ")
        )));
    }
    println!("Synced {} type(s) to {base}", plans.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_fields_are_dropped_and_other_meta_kept() {
        let mut r = json!({"resourceType": "Patient", "id": "p", "meta": {
            "versionId": "3", "lastUpdated": "2026-01-01T00:00:00Z",
            "tag": [{"code": "x"}], "profile": ["http://p"]}});
        strip_version(&mut r);
        assert_eq!(
            r["meta"],
            json!({"tag": [{"code": "x"}], "profile": ["http://p"]})
        );
        let mut r = json!({"resourceType": "Patient", "id": "p", "meta": {"versionId": "3"}});
        strip_version(&mut r);
        assert_eq!(r, json!({"resourceType": "Patient", "id": "p"}));
    }

    #[test]
    fn ids_are_checked_against_the_fhir_rule() {
        assert!(valid_id("a-b.C9"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a\tb"));
        assert!(!valid_id(&"x".repeat(65)));
    }

    #[test]
    fn changed_resources_are_new_ids_new_times_or_without_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Location.ndjson");
        let lines = [
            json!({"resourceType": "Location", "id": "same", "meta": {"lastUpdated": "2026-01-01T00:00:00Z"}}),
            json!({"resourceType": "Location", "id": "newer", "meta": {"lastUpdated": "2026-02-01T00:00:00Z"},
                   "partOf": {"reference": "Location/new"}}),
            json!({"resourceType": "Location", "id": "new", "meta": {"lastUpdated": "2026-01-01T00:00:00Z"}}),
            json!({"resourceType": "Location", "id": "untimed"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let synced: Synced = [
            ("same", "2026-01-01T00:00:00Z"),
            ("newer", "2026-01-01T00:00:00Z"),
            ("untimed", ""),
            ("gone", "2026-01-01T00:00:00Z"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let p = plan_type("Location", &path, &synced, false).unwrap();
        let ids: Vec<&str> = p.changed.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["newer", "new", "untimed"]);
        // `newer` is under `new`, which is also being sent.
        assert_eq!(p.depths, [1, 0, 0]);
        assert_eq!(p.unchanged, 1);
        assert_eq!(p.gone, ["gone"]);

        let p = plan_type("Location", &path, &synced, true).unwrap();
        assert_eq!(p.changed.len(), 4);
        assert_eq!(p.unchanged, 0);
    }
}
