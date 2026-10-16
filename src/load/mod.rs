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
//!
//! The input is read twice. The first pass checks every line and keeps
//! only what ordering and the plan need: each resource's type, id,
//! `meta.versionId`, line number and `partOf` parent. The second reads each
//! bundle's resources again as a worker takes the bundle, so only the
//! bundles being posted are held. Within Locations, and within the other
//! types, the resources at one `partOf` depth go in file order, so the
//! second pass reads the file once per such group.

pub mod bundle;
pub mod capability;
pub mod order;

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Condvar, Mutex, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::cli::LoadArgs;
use crate::error::{PipesError, Result};
use crate::extract::client::{FetchError, FhirClient};
use crate::fhir::head::Head;
use crate::fhir::ndjson::{Line, NdjsonReader};
use crate::load::bundle::{Bundle, dependencies, depends_on, describe_put, put_bundle};
use crate::load::capability::check_update_create;
use crate::load::order::{Node, parents_first, reference_key};

/// One resource of the input, as `load` keeps it between the two passes.
struct Planned {
    /// Index into `LoadPlan::types`.
    resource_type: usize,
    id: String,
    /// `meta.versionId`; `None` is a create.
    version: Option<String>,
    line: usize,
}

/// The input after the first pass.
pub struct LoadPlan {
    types: Vec<String>,
    resources: Vec<Planned>,
    /// Indexes into `resources`, in the order they are loaded.
    order: Vec<usize>,
}

impl LoadPlan {
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Types of the resources that have no `meta.versionId`: the creates.
    fn create_types(&self) -> Vec<String> {
        let mut types: Vec<usize> = self
            .resources
            .iter()
            .filter(|r| r.version.is_none())
            .map(|r| r.resource_type)
            .collect();
        types.sort_unstable();
        types.dedup();
        types.into_iter().map(|t| self.types[t].clone()).collect()
    }

    /// The bundle of `batch_size` each resource goes in, by `<Type>/<id>`.
    fn bundle_of_each(&self, batch_size: usize) -> HashMap<String, usize> {
        self.order
            .iter()
            .enumerate()
            .map(|(at, &i)| {
                let r = &self.resources[i];
                (
                    format!("{}/{}", self.types[r.resource_type], r.id),
                    at / batch_size,
                )
            })
            .collect()
    }

    /// One line per bundle of `batch_size`: its entries as
    /// `describe_put` names them.
    fn describe(&self, batch_size: usize) -> Vec<String> {
        self.order
            .chunks(batch_size)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|&i| {
                        let r = &self.resources[i];
                        describe_put(&self.types[r.resource_type], &r.id, r.version.as_deref())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect()
    }
}

/// The first pass. Every line must be an object with string `resourceType`
/// and `id`, and no `<Type>/<id>` may repeat; the plan is ordered parents
/// first (`order::parents_first`), and a `partOf` cycle is an error.
pub fn read_plan(path: &Path) -> Result<LoadPlan> {
    let mut types: Vec<String> = Vec::new();
    let mut type_index: HashMap<String, usize> = HashMap::new();
    let mut resources: Vec<Planned> = Vec::new();
    // `partOf.reference` of each resource, until the order is known.
    let mut part_of: Vec<Option<String>> = Vec::new();
    let mut seen: HashSet<(usize, String)> = HashSet::new();
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let head = Head::parse(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        let not_a_resource = || {
            PipesError::Usage(format!(
                "{}: line {}: not a FHIR resource (needs a resourceType and a non-empty string id)",
                path.display(),
                line.number
            ))
        };
        let head = head.ok_or_else(not_a_resource)?;
        let (Some(resource_type), Some(id)) = (head.resource_type, head.id) else {
            return Err(not_a_resource());
        };
        if resource_type.is_empty() || id.is_empty() {
            return Err(not_a_resource());
        }
        let t = match type_index.get(&resource_type) {
            Some(&t) => t,
            None => {
                types.push(resource_type.clone());
                type_index.insert(resource_type.clone(), types.len() - 1);
                types.len() - 1
            }
        };
        if !seen.insert((t, id.clone())) {
            return Err(PipesError::Usage(format!(
                "{}: line {}: duplicate resource {resource_type}/{id}",
                path.display(),
                line.number
            )));
        }
        part_of.push(head.part_of);
        resources.push(Planned {
            resource_type: t,
            id,
            version: head.version_id,
            line: line.number,
        });
    }
    drop(seen);
    let nodes: Vec<Node> = resources
        .iter()
        .zip(&part_of)
        .map(|(r, p)| {
            let t = types[r.resource_type].as_str();
            Node {
                resource_type: t,
                id: &r.id,
                parent: p.as_deref().and_then(|p| reference_key(p, Some(t))),
            }
        })
        .collect();
    let order = parents_first(&nodes)?;
    drop(nodes);
    Ok(LoadPlan {
        types,
        resources,
        order,
    })
}

/// The second pass: the resources read again by line number. A line before
/// the one last read starts the file again from the top.
struct Reread<'a> {
    path: &'a Path,
    reader: Option<NdjsonReader>,
    /// The number of the last line read.
    at: usize,
}

impl<'a> Reread<'a> {
    fn new(path: &'a Path) -> Self {
        Reread {
            path,
            reader: None,
            at: 0,
        }
    }

    /// The resource on line `r.line`, checked to be the one planned.
    fn resource(&mut self, plan: &LoadPlan, r: &Planned) -> Result<Value> {
        let resource_type = &plan.types[r.resource_type];
        let changed = || {
            PipesError::Environment(format!(
                "{} changed while it was being loaded: line {} is no longer {resource_type}/{}; \
                 bundles committed before this stay committed, re-running is safe",
                self.path.display(),
                r.line,
                r.id
            ))
        };
        let line = self.line(r.line)?.ok_or_else(changed)?;
        let value: Value = serde_json::from_str(&line.text).map_err(|_| changed())?;
        let same = value.get("resourceType").and_then(Value::as_str) == Some(resource_type)
            && value.get("id").and_then(Value::as_str) == Some(r.id.as_str());
        if !same {
            return Err(changed());
        }
        Ok(value)
    }

    /// Line `n`, or `None` when the file no longer has a line `n`.
    fn line(&mut self, n: usize) -> Result<Option<Line>> {
        if n <= self.at {
            self.reader = None;
        }
        if self.reader.is_none() {
            self.reader = Some(NdjsonReader::open(self.path)?);
            self.at = 0;
        }
        let reader = self.reader.as_mut().expect("opened above");
        for line in reader {
            let line = line?;
            self.at = line.number;
            if line.number == n {
                return Ok(Some(line));
            }
            if line.number > n {
                break;
            }
        }
        Ok(None)
    }
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
    let (got, sent) = client.post_json_counted(base, b.to_bytes());
    match got {
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
            Err(PipesError::Environment(if sent > 1 {
                // The first attempt timed out or got a 5xx; the server may
                // have committed it, and this answer is about its own writes.
                format!(
                    "bundle {n}/{count} rejected with HTTP {status} after it was sent {sent} times. \
                     An earlier attempt timed out or failed with a server error and may have been \
                     committed, in which case the conflicts below are that attempt's own writes. {detail}\n\
                     Check these resources on the server: if they hold the edit, the bundle is \
                     loaded; if not, they changed after the snapshot was taken: run extract, \
                     re-apply the edit, and diff again."
                )
            } else {
                format!(
                    "bundle {n}/{count} rejected with HTTP {status}; nothing in it was written. {detail}\n\
                     These resources changed on the server after the snapshot was taken: \
                     run extract, re-apply the edit, and diff again."
                )
            }))
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
    let mut deps = dependencies(bundles);
    post_stream(
        client,
        base,
        bundles.len(),
        concurrency,
        progress,
        |i| Ok((&bundles[i], std::mem::take(&mut deps[i]))),
        post,
    )
}

/// `post_all` over bundles made as they are posted: a worker takes the next
/// bundle by calling `next(i)`, which returns bundle `i` and the earlier
/// bundles it references. `next` is called once per bundle, in order and
/// one call at a time, so it can read a file front to back; only the
/// bundles being posted are held. An error from `next` is a failure like
/// a failed post.
pub fn post_stream<B, F>(
    client: &FhirClient,
    base: &str,
    count: usize,
    concurrency: usize,
    progress: &Progress,
    next: F,
    post: &PostFn,
) -> Posted
where
    B: Borrow<Bundle> + Send,
    F: FnMut(usize) -> Result<(B, Vec<usize>)> + Send,
{
    // The next bundle to make, and how.
    let source = Mutex::new((0usize, next));
    // Which bundles are committed, and whether to stop.
    let state = (Mutex::new((vec![false; count], false)), Condvar::new());
    let (tx, rx) = mpsc::channel::<(usize, Result<usize>)>();
    let mut failure: Option<PipesError> = None;
    std::thread::scope(|s| {
        for _ in 0..concurrency.clamp(1, count.max(1)) {
            let tx = tx.clone();
            let (source, state) = (&source, &state);
            s.spawn(move || {
                loop {
                    let (i, made) = {
                        let mut src = source.lock().unwrap();
                        let i = src.0;
                        if i >= count || state.0.lock().unwrap().1 {
                            break;
                        }
                        src.0 += 1;
                        let made = (src.1)(i);
                        if made.is_err() {
                            // Before the next worker can make another.
                            state.0.lock().unwrap().1 = true;
                        }
                        (i, made)
                    };
                    let (bundle, deps) = match made {
                        Ok(made) => made,
                        Err(e) => {
                            state.1.notify_all();
                            let _ = tx.send((i, Err(e)));
                            break;
                        }
                    };
                    let bundle = bundle.borrow();
                    {
                        let (lock, changed) = state;
                        let mut st = lock.lock().unwrap();
                        while !st.1 && deps.iter().any(|&j| !st.0[j]) {
                            st = changed.wait(st).unwrap();
                        }
                        if st.1 {
                            break;
                        }
                    }
                    let r = post(client, base, bundle, progress.first + i + 1, progress.total);
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
                    if tx.send((i, r.map(|()| bundle.size()))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (i, r) in rx.iter() {
            match r {
                Ok(size) => eprintln!(
                    "{}bundle {}/{} committed ({size} resources)",
                    progress.label,
                    progress.first + i + 1,
                    progress.total,
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
    let resources = read_plan(&args.input)?;
    if resources.is_empty() {
        println!("Nothing to load: {} is empty", args.input.display());
        return Ok(());
    }
    // Ordering and the cycle check come before any request: a bad input
    // should never cost a network round trip.
    let creates = resources.create_types();
    let total = resources.len();
    let count = total.div_ceil(args.batch_size);

    let base = args.server.trim_end_matches('/').to_string();
    let client = FhirClient::new(
        args.auth.auth()?,
        args.retries,
        Duration::from_secs(args.timeout),
    )?;

    preflight(&client, &base, &creates)?;

    if args.dry_run {
        println!("Dry run: {total} resources in {count} bundle(s) for {base}; nothing posted");
        for (i, line) in resources.describe(args.batch_size).iter().enumerate() {
            println!("bundle {}/{count}: {line}", i + 1);
        }
        return Ok(());
    }

    // Each bundle is read from the file as a worker takes it. With one
    // worker every earlier bundle is committed before the next starts, so
    // the references between bundles matter only with more.
    let batch = args.batch_size;
    let home = (args.concurrency > 1).then(|| resources.bundle_of_each(batch));
    let chunks: Vec<&[usize]> = resources.order.chunks(batch).collect();
    let mut reread = Reread::new(&args.input);
    let next = |i: usize| -> Result<(Bundle, Vec<usize>)> {
        let values = chunks[i]
            .iter()
            .map(|&j| reread.resource(&resources, &resources.resources[j]))
            .collect::<Result<Vec<Value>>>()?;
        let bundle = put_bundle(values);
        let deps = match &home {
            Some(home) => depends_on(&bundle, i, |key| home.get(key).copied()),
            None => Vec::new(),
        };
        Ok((bundle, deps))
    };
    let progress = Progress {
        label: "",
        first: 0,
        total: count,
    };
    let posted = post_stream(
        &client,
        &base,
        count,
        args.concurrency,
        &progress,
        next,
        &post,
    );
    if let Some(e) = posted.failure {
        return Err(e);
    }
    println!("Loaded {total} resources in {count} bundle(s) to {base}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, resources: &[Value]) {
        let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    fn loc(id: &str, parent: Option<&str>) -> Value {
        let mut r = json!({"resourceType": "Location", "id": id, "meta": {"versionId": "1"}});
        if let Some(p) = parent {
            r["partOf"] = json!({"reference": format!("Location/{p}")});
        }
        r
    }

    /// Every resource of the plan, read again in order.
    fn reread_all(path: &Path, plan: &LoadPlan) -> Result<Vec<Value>> {
        let mut reread = Reread::new(path);
        plan.order
            .iter()
            .map(|&i| reread.resource(plan, &plan.resources[i]))
            .collect()
    }

    #[test]
    fn the_plan_keeps_what_ordering_and_the_bundles_need() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.ndjson");
        let resources = [
            json!({"resourceType": "Location", "id": "c", "partOf": {"reference": "Location/b"}}),
            json!({"resourceType": "Organization", "id": "b"}),
            loc("b", Some("a")),
            json!({"resourceType": "Location", "id": "a", "meta": {"versionId": "W/\"7\""}}),
        ];
        // A blank line counts in the line numbers but holds no resource.
        let text: String = resources.iter().map(|r| format!("{r}\n\n")).collect();
        std::fs::write(&path, text).unwrap();
        let plan = read_plan(&path).unwrap();
        assert_eq!(plan.len(), 4);
        assert_eq!(plan.create_types(), ["Location", "Organization"]);
        assert_eq!(
            plan.describe(3),
            [
                "Organization/b create, Location/a update@W/\"7\", Location/b update@1",
                "Location/c create"
            ]
        );
        let again = reread_all(&path, &plan).unwrap();
        assert_eq!(again, [1, 3, 2, 0].map(|i| resources[i].clone()));
    }

    #[test]
    fn a_bundle_that_cannot_be_made_stops_the_stream() {
        let client = FhirClient::new(
            crate::extract::client::Auth::None,
            1,
            Duration::from_secs(1),
        )
        .unwrap();
        let made = Mutex::new(Vec::new());
        let posted = post_stream(
            &client,
            "http://unused",
            6,
            3,
            &Progress {
                label: "",
                first: 0,
                total: 6,
            },
            |i| {
                made.lock().unwrap().push(i);
                if i == 3 {
                    return Err(PipesError::Environment("gone".into()));
                }
                let r = json!({"resourceType": "Location", "id": format!("l{i}")});
                Ok((put_bundle(vec![r]), Vec::new()))
            },
            &|_, _, _, _, _| Ok(()),
        );
        assert!(
            matches!(&posted.failure, Some(PipesError::Environment(m)) if m == "gone"),
            "{:?}",
            posted.failure.map(|e| e.to_string())
        );
        assert_eq!(
            *made.lock().unwrap(),
            [0, 1, 2, 3],
            "made in order, none after"
        );
        assert_eq!(
            posted.committed,
            [true, true, true, false, false, false],
            "those taken before it finish"
        );
    }

    #[test]
    fn a_file_changed_between_the_passes_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.ndjson");
        write(&path, &[loc("b", Some("a")), loc("a", None)]);
        let plan = read_plan(&path).unwrap();
        // Another resource on the planned line.
        write(&path, &[loc("b", Some("a")), loc("x", None)]);
        let err = reread_all(&path, &plan).unwrap_err();
        assert!(
            matches!(&err, PipesError::Environment(m)
                if m.contains("changed while it was being loaded: line 2 is no longer Location/a")),
            "{err}"
        );
        // The planned line gone.
        write(&path, &[loc("b", Some("a"))]);
        let err = reread_all(&path, &plan).unwrap_err();
        assert!(
            matches!(&err, PipesError::Environment(m) if m.contains("line 2")),
            "{err}"
        );
        // No longer JSON.
        std::fs::write(&path, "{\"x\":\n{\"y\":\n").unwrap();
        let err = reread_all(&path, &plan).unwrap_err();
        assert!(matches!(&err, PipesError::Environment(_)), "{err}");
    }
}
