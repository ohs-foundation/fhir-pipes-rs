//! `fhir-pipes load`: FHIR NDJSON -> transaction bundles with `If-Match`. Parents
//! first, a capability preflight, retry through the shared client, and a
//! precise conflict list on 412 or 409. The first failed bundle aborts;
//! committed bundles stay committed, and because every entry is a PUT by
//! id with a version check, re-running is safe.
//!
//! `--concurrency N` posts bundles from N threads. A resource id is in one
//! bundle only (the input may not repeat one), so no two bundles in flight
//! write the same resource, and a bundle starts only once every earlier
//! bundle holding a resource it references is committed, so parents still
//! land first on a server that checks references.

pub mod bundle;
pub mod capability;
pub mod order;

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::cli::LoadArgs;
use crate::error::{PipesError, Result};
use crate::extract::client::{FetchError, FhirClient};
use crate::fhir::ndjson::NdjsonReader;
use crate::load::bundle::{Bundle, dependencies, plan};
use crate::load::capability::check_update_create;
use crate::load::order::order_parents_first;

/// Every line must be an object with string `resourceType` and `id`, and
/// no `<Type>/<id>` may repeat. The whole changeset is held in memory:
/// ordering needs all of it, and it is a changeset, not a registry.
pub fn read_resources(path: &Path) -> Result<Vec<Value>> {
    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let value: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        let resource_type = value.get("resourceType").and_then(Value::as_str);
        let id = value.get("id").and_then(Value::as_str);
        let key = match (resource_type, id) {
            (Some(t), Some(i)) if !t.is_empty() && !i.is_empty() => format!("{t}/{i}"),
            _ => {
                return Err(PipesError::Usage(format!(
                    "{}: line {}: not a FHIR resource (needs a resourceType and a non-empty string id)",
                    path.display(),
                    line.number
                )));
            }
        };
        if !seen.insert(key.clone()) {
            return Err(PipesError::Usage(format!(
                "{}: line {}: duplicate resource {key}",
                path.display(),
                line.number
            )));
        }
        resources.push(value);
    }
    Ok(resources)
}

/// Types of the resources that have no `meta.versionId`: the creates.
fn create_types(resources: &[Value]) -> Vec<String> {
    resources
        .iter()
        .filter(|r| {
            r.get("meta")
                .and_then(|m| m.get("versionId"))
                .and_then(Value::as_str)
                .is_none()
        })
        .filter_map(|r| r.get("resourceType")?.as_str().map(str::to_string))
        .collect()
}

/// After a 412 or 409: read each version-checked entry back and name the
/// ones whose server version differs. Standard reads, nothing vendor
/// specific, so the message is exact on any R4 server.
fn probe_conflicts(client: &FhirClient, base: &str, bundle: &Bundle) -> Vec<String> {
    let mut out = Vec::new();
    for e in &bundle.entries {
        let Some(expected) = &e.version else {
            continue;
        };
        let label = format!("{}/{}", e.resource_type, e.id);
        match client.get(&format!("{base}/{label}")) {
            Ok(fetched) => {
                let actual = serde_json::from_slice::<Value>(&fetched.body)
                    .ok()
                    .and_then(|v| {
                        v.get("meta")?
                            .get("versionId")?
                            .as_str()
                            .map(str::to_string)
                    });
                match actual {
                    Some(a) if &a == expected => {}
                    Some(a) => out.push(format!(
                        "{label} (expected version {expected}, server has {a})"
                    )),
                    None => out.push(format!(
                        "{label} (expected version {expected}, server returned no versionId)"
                    )),
                }
            }
            Err(FetchError::Status {
                status: 404 | 410, ..
            }) => out.push(format!(
                "{label} (expected version {expected}, deleted on the server)"
            )),
            Err(err) => out.push(format!("{label} (could not check: {err})")),
        }
    }
    out
}

/// Posts one bundle; the error says what was and was not written.
fn post(client: &FhirClient, base: &str, b: &Bundle, n: usize, count: usize) -> Result<()> {
    let body = b.to_json().to_string().into_bytes();
    match client.post_json(base, body) {
        Ok(_) => Ok(()),
        Err(FetchError::Status {
            status: status @ (409 | 412),
            body,
        }) => {
            let conflicts = probe_conflicts(client, base, b);
            let detail = if conflicts.is_empty() {
                format!("server response: {body}")
            } else {
                format!(
                    "{} conflict(s):\n  {}",
                    conflicts.len(),
                    conflicts.join("\n  ")
                )
            };
            Err(PipesError::Environment(format!(
                "bundle {n}/{count} rejected with HTTP {status}; nothing in it was written. {detail}\n\
                 These resources changed on the server after the snapshot was taken: \
                 run extract, re-apply the edit, and diff again."
            )))
        }
        Err(e) => Err(PipesError::Environment(format!(
            "bundle {n}/{count} failed: {e}; bundles committed before it stay committed, re-running is safe"
        ))),
    }
}

/// Where a run of bundles sits in a larger plan, for the progress lines:
/// bundle `first + i + 1` of `total`, after `label` ("" for load).
pub struct Progress<'a> {
    pub label: &'a str,
    pub first: usize,
    pub total: usize,
}

/// Posts one bundle, given its number and the total; the error says what
/// was and was not written.
pub type PostFn<'a> = dyn Fn(&FhirClient, &str, &Bundle, usize, usize) -> Result<()> + Sync + 'a;

/// What `post_all` did: which bundles are committed, and the first failure.
pub struct Posted {
    pub committed: Vec<bool>,
    pub failure: Option<PipesError>,
}

/// Posts every bundle, `concurrency` at a time. Workers take bundles in
/// order; each waits until the earlier bundles it references are
/// committed. The first failure stops new bundles from starting; those
/// already in flight finish.
pub fn post_all(
    client: &FhirClient,
    base: &str,
    bundles: &[Bundle],
    concurrency: usize,
    progress: &Progress,
    post: &PostFn,
) -> Posted {
    let count = bundles.len();
    let deps = dependencies(bundles);
    let next = AtomicUsize::new(0);
    // Which bundles are committed, and whether to stop.
    let state = (Mutex::new((vec![false; count], false)), Condvar::new());
    let (tx, rx) = mpsc::channel::<(usize, Result<()>)>();
    let mut failure: Option<PipesError> = None;
    std::thread::scope(|s| {
        for _ in 0..concurrency.clamp(1, count.max(1)) {
            let tx = tx.clone();
            let (next, state, deps) = (&next, &state, &deps);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= count {
                        break;
                    }
                    {
                        let (lock, changed) = state;
                        let mut st = lock.lock().unwrap();
                        while !st.1 && deps[i].iter().any(|&j| !st.0[j]) {
                            st = changed.wait(st).unwrap();
                        }
                        if st.1 {
                            break;
                        }
                    }
                    let r = post(
                        client,
                        base,
                        &bundles[i],
                        progress.first + i + 1,
                        progress.total,
                    );
                    // Recorded before the next bundle is taken, so one worker
                    // stops right after a failure, as a sequential load does.
                    {
                        let mut st = state.0.lock().unwrap();
                        match r {
                            Ok(()) => st.0[i] = true,
                            Err(_) => st.1 = true,
                        }
                    }
                    state.1.notify_all();
                    if tx.send((i, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, r) in rx.iter() {
            match r {
                Ok(()) => eprintln!(
                    "{}bundle {}/{} committed ({} resources)",
                    progress.label,
                    progress.first + i + 1,
                    progress.total,
                    bundles[i].size()
                ),
                Err(e) => {
                    if failure.is_none() {
                        failure = Some(e);
                    }
                }
            }
        }
    });
    let committed = state.0.into_inner().unwrap().0;
    Posted { committed, failure }
}

/// GET `<base>/metadata`, the cheapest check that the URL and credentials
/// work, and check update-as-create for `creates`, the types of resources
/// that may be new on the server.
pub fn preflight(client: &FhirClient, base: &str, creates: &[String]) -> Result<()> {
    let metadata_url = format!("{base}/metadata");
    let capability: Value = match client.get(&metadata_url) {
        Ok(f) => serde_json::from_slice(&f.body).map_err(|e| {
            PipesError::Environment(format!(
                "capability preflight: {metadata_url} returned non-JSON: {e}"
            ))
        })?,
        Err(e) => {
            return Err(PipesError::Environment(format!(
                "capability preflight failed: {e} for {metadata_url}; check the server URL and credentials"
            )));
        }
    };
    let unstated = check_update_create(&capability, creates)?;
    if !unstated.is_empty() {
        eprintln!(
            "fhir-pipes: warning: the server's CapabilityStatement does not state updateCreate for {}; \
             proceeding on the assumption that PUT to a new id creates (HAPI FHIR supports it but omits the flag). \
             If the server does not, the first bundle that creates will fail with the server's own error.",
            unstated.join(", ")
        );
    }
    Ok(())
}

pub fn run_load(args: &LoadArgs) -> Result<()> {
    if args.batch_size == 0 {
        return Err(PipesError::Usage("--batch-size must be at least 1".into()));
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()));
    }
    if !args.input.is_file() {
        return Err(PipesError::Usage(format!(
            "--in {}: not a file",
            args.input.display()
        )));
    }
    let resources = read_resources(&args.input)?;
    if resources.is_empty() {
        println!("Nothing to load: {} is empty", args.input.display());
        return Ok(());
    }
    // Ordering and the cycle check come before any request: a bad input
    // should never cost a network round trip.
    let creates = create_types(&resources);
    let ordered = order_parents_first(resources)?;
    let bundles = plan(ordered, args.batch_size);
    let total: usize = bundles.iter().map(|b| b.entries.len()).sum();

    let base = args.server.trim_end_matches('/').to_string();
    let client = FhirClient::new(
        args.auth.auth()?,
        args.retries,
        Duration::from_secs(args.timeout),
    )?;

    preflight(&client, &base, &creates)?;

    if args.dry_run {
        println!(
            "Dry run: {total} resources in {} bundle(s) for {base}; nothing posted",
            bundles.len()
        );
        for (i, b) in bundles.iter().enumerate() {
            println!("bundle {}/{}: {}", i + 1, bundles.len(), b.describe());
        }
        return Ok(());
    }

    let count = bundles.len();
    let progress = Progress {
        label: "",
        first: 0,
        total: count,
    };
    if let Some(e) = post_all(&client, &base, &bundles, args.concurrency, &progress, &post).failure
    {
        return Err(e);
    }
    println!("Loaded {total} resources in {count} bundle(s) to {base}");
    Ok(())
}
