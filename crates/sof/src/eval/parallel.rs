//! Running a view for non-async callers: on a thread of its own, or over
//! blocks of resources on several OS threads, each thread with its own
//! current-thread Tokio runtime. The runtimes are kept and reused, so a run
//! over many chunks of resources builds them once rather than per chunk.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::runtime::Runtime;

use serde_json::Value;

use crate::runner::ViewResult;
use crate::view_definition::ViewDefinition;
use crate::{Error, Result};

use super::{CompiledView, execute};

/// Blocking convenience wrapper around [`execute`] for non-async callers.
///
/// Spins up a fresh current-thread Tokio runtime on a dedicated thread, so it is
/// safe to call from synchronous code even when an outer runtime exists. Async
/// callers should use [`execute`] directly.
///
/// # Errors
///
/// Same as [`execute`].
pub fn execute_blocking(view: &ViewDefinition, resources: &[Value]) -> Result<ViewResult> {
    std::thread::scope(|s| {
        s.spawn(|| block_on(execute(view, resources))?)
            .join()
            .map_err(|_| thread_panicked())?
    })
}

/// Rows, each its column values in column order.
type Rows = Vec<Vec<Value>>;

/// Blocks per thread that [`execute_parallel`] aims for, so that a thread that
/// finishes early has blocks left to take.
const BLOCKS_PER_THREAD: usize = 16;

/// The most resources in one [`execute_parallel`] block.
const MAX_BLOCK: usize = 64;

/// Execute a ViewDefinition against in-memory FHIR resources on up to `threads`
/// OS threads, for non-async callers.
///
/// Compiles the view ([`CompiledView::compile_blocking`]) and runs it with
/// [`CompiledView::execute_parallel`]; callers running one view over several
/// batches of resources can compile it once and call that for each batch.
///
/// # Errors
///
/// Same as [`execute`]. If several blocks fail, the error of the first of them
/// in input order is returned; the other threads stop at their next block.
pub fn execute_parallel(
    view: &ViewDefinition,
    resources: &[Value],
    threads: usize,
) -> Result<ViewResult> {
    CompiledView::compile_blocking(view)?.execute_parallel(resources, threads)
}

/// Current-thread runtimes not in use. An evaluation thread takes one for
/// as long as it runs and puts it back, so there are as many as threads
/// have run at once. The FHIRPath engine spawns no tasks, so a runtime
/// carries nothing from one use to the next.
static RUNTIMES: Mutex<Vec<Runtime>> = Mutex::new(Vec::new());

/// Run `future` to completion on this thread, on a current-thread runtime
/// from [`RUNTIMES`] or a new one. Not for use inside an async runtime.
fn block_on<F: Future>(future: F) -> Result<F::Output> {
    let idle = RUNTIMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop();
    let rt = match idle {
        Some(rt) => rt,
        None => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::FhirPath(format!("building runtime: {e}")))?,
    };
    let out = rt.block_on(future);
    RUNTIMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(rt);
    Ok(out)
}

fn thread_panicked() -> Error {
    Error::FhirPath("evaluation thread panicked".to_string())
}

impl CompiledView {
    /// [`CompiledView::compile`] for non-async callers. Compiles on a thread
    /// of its own, so that calling this from inside an async runtime never
    /// starts a runtime on that runtime's thread.
    ///
    /// # Errors
    ///
    /// Same as [`CompiledView::compile`].
    pub fn compile_blocking(view: &ViewDefinition) -> Result<Self> {
        std::thread::scope(|s| {
            s.spawn(|| block_on(Self::compile(view))?)
                .join()
                .map_err(|_| thread_panicked())?
        })
    }

    /// [`CompiledView::compile_with_position`] for non-async callers.
    ///
    /// # Errors
    ///
    /// Same as [`CompiledView::compile_with_position`].
    pub fn compile_with_position_blocking(view: &ViewDefinition, name: &str) -> Result<Self> {
        std::thread::scope(|s| {
            s.spawn(|| block_on(Self::compile_with_position(view, name))?)
                .join()
                .map_err(|_| thread_panicked())?
        })
    }

    /// Execute the view against in-memory FHIR resources on up to `threads`
    /// OS threads, for non-async callers.
    ///
    /// The resources are split into small blocks. Each thread takes the next
    /// unclaimed block until none are left, so costly resources stored
    /// together (all of one type, say) are spread over the threads rather
    /// than landing on one. Each thread runs its own current-thread Tokio
    /// runtime. Rows come back in input order, the same as [`execute`]. With
    /// `threads <= 1`, or fewer resources than threads, fewer threads are
    /// used.
    ///
    /// # Errors
    ///
    /// Same as [`execute`]. If several blocks fail, the error of the first of
    /// them in input order is returned; the other threads stop at their next
    /// block.
    pub fn execute_parallel(&self, resources: &[Value], threads: usize) -> Result<ViewResult> {
        self.run_parallel(resources, None, threads)
    }

    /// [`CompiledView::execute_parallel`], with the position variable of a
    /// view compiled by [`CompiledView::compile_with_position`] bound to
    /// `positions[i]` for `resources[i]`.
    ///
    /// # Errors
    ///
    /// Same as [`CompiledView::execute_parallel`], and an error when
    /// `positions` and `resources` differ in length.
    pub fn execute_parallel_at(
        &self,
        resources: &[Value],
        positions: &[i64],
        threads: usize,
    ) -> Result<ViewResult> {
        if positions.len() != resources.len() {
            return Err(Error::InvalidViewDefinition(format!(
                "{} positions for {} resources",
                positions.len(),
                resources.len()
            )));
        }
        self.run_parallel(resources, Some(positions), threads)
    }

    fn run_parallel(
        &self,
        resources: &[Value],
        positions: Option<&[i64]>,
        threads: usize,
    ) -> Result<ViewResult> {
        let threads = threads.clamp(1, resources.len().max(1));
        let block = resources
            .len()
            .div_ceil(threads * BLOCKS_PER_THREAD)
            .clamp(1, MAX_BLOCK);
        // Each block with its positions, or none.
        let blocks: Vec<(&[Value], Option<&[i64]>)> = resources
            .chunks(block)
            .enumerate()
            .map(|(i, part)| {
                (
                    part,
                    positions.map(|p| &p[i * block..i * block + part.len()]),
                )
            })
            .collect();
        let blocks = &blocks;
        let next = &AtomicUsize::new(0);
        let failed = &AtomicBool::new(false);
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads.min(blocks.len()))
                .map(|_| {
                    s.spawn(move || -> Result<Vec<(usize, Result<Rows>)>> {
                        block_on(async {
                            let mut done = Vec::new();
                            while !failed.load(Ordering::Relaxed) {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some((part, at)) = blocks.get(i) else {
                                    break;
                                };
                                let rows = self.execute_block(part, *at).await;
                                if rows.is_err() {
                                    failed.store(true, Ordering::Relaxed);
                                }
                                done.push((i, rows));
                            }
                            done
                        })
                    })
                })
                .collect();
            let mut done = Vec::with_capacity(blocks.len());
            for worker in workers {
                done.extend(worker.join().map_err(|_| thread_panicked())??);
            }
            done.sort_unstable_by_key(|(i, _)| *i);
            let parts = done
                .into_iter()
                .map(|(_, rows)| rows)
                .collect::<Result<Vec<_>>>()?;
            let mut data = Vec::with_capacity(parts.iter().map(Vec::len).sum());
            for part in parts {
                data.extend(part);
            }
            Ok(ViewResult {
                columns: self.columns.clone(),
                data,
            })
        })
    }

    /// The rows of a run of resources, in order, each at its position if
    /// `positions` are given.
    async fn execute_block(
        &self,
        resources: &[Value],
        positions: Option<&[i64]>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        for (i, resource) in resources.iter().enumerate() {
            let at = positions.map_or(0, |p| p[i]);
            rows.extend(self.execute_resource_at(resource, at).await?);
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::eval::tests::{patient, view};
    use serde_json::json;

    #[test]
    fn parallel_rows_match_sequential_with_uneven_cost() {
        // The costly resources (many names) all come first, as when one
        // resource type is stored together; Observations are skipped.
        let resources: Vec<Value> = (0..1000)
            .map(|i| match i {
                0..100 => patient(i, 40),
                _ if i % 3 == 0 => json!({ "resourceType": "Observation", "id": format!("o{i}") }),
                _ => patient(i, i % 4),
            })
            .collect();
        let v = view(json!({
            "resource": "Patient",
            "select": [
                { "column": [{ "name": "id", "path": "getResourceKey()" }] },
                { "forEachOrNull": "name", "column": [
                    { "name": "family", "path": "family" },
                    { "name": "i", "path": "%rowIndex" }
                ] }
            ]
        }));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sequential = rt.block_on(execute(&v, &resources)).unwrap();
        // forEachOrNull keeps one row for a Patient with no names.
        let expected: usize = (100..1000)
            .filter(|i| i % 3 != 0)
            .map(|i| (i % 4).max(1))
            .sum::<usize>()
            + 100 * 40;
        assert_eq!(sequential.data.len(), expected);
        for threads in [1, 3, 8, 64, 5000] {
            let parallel = execute_parallel(&v, &resources, threads).unwrap();
            assert_eq!(parallel.data, sequential.data, "{threads} threads");
        }
        // One compiled view run over consecutive batches gives the same rows.
        let compiled = CompiledView::compile_blocking(&v).unwrap();
        let mut batched = Vec::new();
        for batch in resources.chunks(300) {
            batched.extend(compiled.execute_parallel(batch, 4).unwrap().data);
        }
        assert_eq!(batched, sequential.data);
    }

    #[test]
    fn a_position_variable_holds_each_resources_own_number() {
        let resources: Vec<Value> = (0..300).map(|i| patient(i, i % 3)).collect();
        let positions: Vec<i64> = (0..300).map(|i| 1000 + i).collect();
        let v = view(json!({
            "resource": "Patient",
            "select": [
                { "column": [{ "name": "line", "path": "%line" }] },
                { "forEach": "name", "column": [
                    { "name": "inner", "path": "%line + %rowIndex" }
                ] }
            ]
        }));
        let compiled = CompiledView::compile_with_position_blocking(&v, "line").unwrap();
        let want: Vec<Vec<Value>> = (0..300i64)
            .flat_map(|i| (0..i % 3).map(move |n| vec![json!(1000 + i), json!(1000 + i + n)]))
            .collect();
        for threads in [1, 4, 16] {
            let got = compiled
                .execute_parallel_at(&resources, &positions, threads)
                .unwrap();
            assert_eq!(got.data, want, "{threads} threads");
        }
        assert!(
            compiled
                .execute_parallel_at(&resources, &positions[1..], 2)
                .is_err()
        );
        // Without the position, the variable is an undefined constant.
        let err = CompiledView::compile_blocking(&v).err().unwrap();
        assert!(err.to_string().contains("line"), "{err}");
        // It may not shadow a constant of the view.
        let mut with_constant = v.clone();
        with_constant
            .constant
            .push(serde_json::from_value(json!({ "name": "line", "valueInteger": 1 })).unwrap());
        assert!(CompiledView::compile_with_position_blocking(&with_constant, "line").is_err());
    }

    #[test]
    fn parallel_returns_an_error_from_any_block() {
        let mut resources: Vec<Value> = (0..500).map(|i| patient(i, 1)).collect();
        resources[300] = patient(300, 2);
        let v = view(json!({
            "resource": "Patient",
            "select": [{ "column": [{ "name": "family", "path": "name.family" }] }]
        }));
        let err = execute_parallel(&v, &resources, 4).unwrap_err();
        assert!(err.to_string().contains("multiple values"), "{err}");
    }
}
