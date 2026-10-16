//! In-memory ViewDefinition execution.
//!
//! Evaluates a [`ViewDefinition`] directly against FHIR resources held as
//! `serde_json::Value`, with no database. Per-expression FHIRPath evaluation is
//! delegated to the real `octofhir-fhirpath` [`FhirPathEngine`], so the full
//! FHIRPath function set (`substring`, `upper`, math, etc.) is available —
//! unlike the hand-lowered subset used by the SQL generator.
//!
//! What stays in this module is the SQL-on-FHIR *relational orchestration* that
//! is not plain FHIRPath: `forEach`/`forEachOrNull`, nested `select` cartesian
//! products, `unionAll`, `repeat` (preorder transitive closure), column
//! ordering, the scalar-vs-collection column rule, and the boolean-`where`
//! rule. The SoF-specific functions `getResourceKey()` / `getReferenceKey()`
//! are registered as custom functions on the engine's registry so they can be
//! used anywhere in an expression (including over `contained[]`).
//!
//! Every path in the view is compiled once, when the view is. Each resource is
//! converted to the engine's tree form once; the foci of `forEach` and
//! `repeat` are nodes of that tree, so moving into them copies no JSON.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use octofhir_fhirpath::core::error_code::FP0053;
use octofhir_fhirpath::core::node::FhirNode;
use octofhir_fhirpath::evaluator::AsyncNodeEvaluator;
use octofhir_fhirpath::evaluator::function_registry::{
    EmptyPropagation, FunctionCategory, FunctionMetadata, FunctionSignature, LazyFunctionEvaluator,
    PureFunctionEvaluator,
};
use octofhir_fhirpath::evaluator::plan::CompiledPlan;
use octofhir_fhirpath::{
    Collection, EmptyModelProvider, EvaluationContext, ExpressionNode, FhirPathEngine,
    FhirPathError, FhirPathValue, ModelProvider, create_function_registry,
};
use serde_json::Value;

use crate::column::{ColumnInfo, ColumnType};
use crate::runner::ViewResult;
use crate::sql_generator::{build_constants, substitute_constants};
use crate::view_definition::{Column, SelectColumn, ViewDefinition};
use crate::{Error, Result};

/// Execute a ViewDefinition against in-memory FHIR resources.
///
/// Resources whose `resourceType` differs from the view's `resource` are
/// skipped. Returns the tabular result with column metadata and rows.
///
/// # Errors
///
/// Returns an error if a selector cannot be parsed, a constant is undefined, the
/// column shape is inconsistent (duplicate names, mismatched `unionAll`
/// branches), or a non-collection column yields more than one value.
pub async fn execute(view: &ViewDefinition, resources: &[Value]) -> Result<ViewResult> {
    let compiled = CompiledView::compile(view).await?;
    let mut data: Vec<Vec<Value>> = Vec::new();
    for resource in resources {
        data.extend(compiled.execute_resource(resource).await?);
    }
    let row_count = data.len();
    Ok(ViewResult {
        columns: compiled.columns().to_vec(),
        data,
        row_count,
    })
}

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
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| Error::FhirPath(format!("building runtime: {e}")))?;
            rt.block_on(execute(view, resources))
        })
        .join()
        .map_err(|_| Error::FhirPath("evaluation thread panicked".to_string()))?
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
/// The view is compiled once and the resources are split into small blocks.
/// Each thread takes the next unclaimed block until none are left, so costly
/// resources stored together (all of one type, say) are spread over the
/// threads rather than landing on one. Each thread runs its own current-thread
/// Tokio runtime. Rows come back in input order, the same as [`execute`]. With
/// `threads <= 1`, or fewer resources than threads, fewer threads are used.
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
    let threads = threads.clamp(1, resources.len().max(1));
    let block = resources
        .len()
        .div_ceil(threads * BLOCKS_PER_THREAD)
        .clamp(1, MAX_BLOCK);
    let blocks: Vec<&[Value]> = resources.chunks(block).collect();
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::FhirPath(format!("building runtime: {e}")))
    };
    let panicked = || Error::FhirPath("evaluation thread panicked".to_string());
    // Compiled on a thread of its own too, so that calling this from inside an
    // async runtime never starts a runtime on that runtime's thread.
    let compiled = std::thread::scope(|s| {
        s.spawn(|| runtime()?.block_on(CompiledView::compile(view)))
            .join()
            .map_err(|_| panicked())?
    })?;
    let (compiled, blocks) = (&compiled, &blocks);
    let next = &AtomicUsize::new(0);
    let failed = &AtomicBool::new(false);
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.min(blocks.len()))
            .map(|_| {
                s.spawn(move || -> Result<Vec<(usize, Result<Rows>)>> {
                    runtime()?.block_on(async {
                        let mut done = Vec::new();
                        while !failed.load(Ordering::Relaxed) {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(part) = blocks.get(i) else { break };
                            let rows = compiled.execute_block(part).await;
                            if rows.is_err() {
                                failed.store(true, Ordering::Relaxed);
                            }
                            done.push((i, rows));
                        }
                        Ok(done)
                    })
                })
            })
            .collect();
        let mut done = Vec::with_capacity(blocks.len());
        for worker in workers {
            done.extend(worker.join().map_err(|_| panicked())??);
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
        let row_count = data.len();
        Ok(ViewResult {
            columns: compiled.columns().to_vec(),
            data,
            row_count,
        })
    })
}

/// A ViewDefinition compiled once for repeated, resource-at-a-time execution.
///
/// Validates the view (columns, name collisions, constants) and compiles every
/// path up front, building the FHIRPath engine once, then lets callers stream
/// resources through [`CompiledView::execute_resource`] without holding the
/// whole dataset in memory.
pub struct CompiledView {
    ev: Evaluator,
    selects: Vec<CompiledSelect>,
    wheres: Vec<Arc<CompiledPlan>>,
    columns: Vec<ColumnInfo>,
}

impl CompiledView {
    /// Compile and validate a ViewDefinition, building the FHIRPath engine.
    pub async fn compile(view: &ViewDefinition) -> Result<Self> {
        if view.resource.trim().is_empty() {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition is missing the required `resource`".to_string(),
            ));
        }
        let constants = build_constants(view)?;

        // Build the engine once with the SoF custom functions registered. No
        // network, no FHIR package: EmptyModelProvider keeps the library
        // self-contained (ofType choice resolution is handled by string rewrite
        // below, not by the model provider).
        let mut registry = create_function_registry();
        registry.register_lazy_function(Arc::new(GetResourceKey));
        registry.register_lazy_function(Arc::new(GetReferenceKey));
        // SoF-divergent function semantics: override the engine's defaults to
        // match the SQL-on-FHIR conformance suite (join() with no separator,
        // and the spec's precision-boundary rules for low/highBoundary).
        registry.register_pure_function(Arc::new(JoinFn));
        let model: Arc<dyn ModelProvider + Send + Sync> = Arc::new(EmptyModelProvider);
        let engine = FhirPathEngine::new(Arc::new(registry), model.clone())
            .await
            .map_err(|e| Error::FhirPath(format!("building FHIRPath engine: {e}")))?;

        let ev = Evaluator {
            resource_type: view.resource.clone(),
            constants,
            engine,
            model,
        };

        let shape = shape(&view.select)?;
        if shape.is_empty() {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition produces no columns".to_string(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (name, _) in &shape {
            if !seen.insert(name.clone()) {
                return Err(Error::InvalidViewDefinition(format!(
                    "column `{name}` is defined more than once"
                )));
            }
        }

        // Compile every where, forEach, repeat and column path now (constants,
        // ofType rewrite, parse), so per-resource evaluation runs the compiled
        // plans with no lookup or parse.
        let wheres = view
            .where_
            .iter()
            .map(|w| ev.plan(&ev.prepare(&w.path)?))
            .collect::<Result<Vec<_>>>()?;
        let selects = view
            .select
            .iter()
            .map(|s| ev.compile_select(s))
            .collect::<Result<Vec<_>>>()?;

        let columns = shape
            .iter()
            .map(|(name, ty)| ColumnInfo::new(name.clone(), *ty))
            .collect();

        Ok(Self {
            ev,
            selects,
            wheres,
            columns,
        })
    }

    /// The output columns, in order.
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }

    /// The FHIR resource type this view selects from.
    pub fn resource_type(&self) -> &str {
        &self.ev.resource_type
    }

    /// Evaluate the view against a single resource, returning its rows (column
    /// values in column order). Resources of a different type, or filtered out
    /// by `where`, yield no rows.
    pub async fn execute_resource(&self, resource: &Value) -> Result<Vec<Vec<Value>>> {
        if resource.get("resourceType").and_then(Value::as_str)
            != Some(self.ev.resource_type.as_str())
        {
            return Ok(Vec::new());
        }
        // The resource's one conversion to the engine's tree; every focus
        // below shares it.
        let root = FhirPathValue::resource_from_node(FhirNode::from_json(resource));
        let focus = Focus::new(&root, 0);
        for plan in &self.wheres {
            if !self.ev.eval_where(plan, self.ev.ctx(&root, &focus)).await? {
                return Ok(Vec::new());
            }
        }

        let mut rows = vec![Vec::with_capacity(self.columns.len())];
        for select in &self.selects {
            let srows = self.ev.eval_select(select, &root, &focus).await?;
            rows = cartesian(rows, srows);
        }
        Ok(rows)
    }

    /// The rows of a run of resources, in order.
    async fn execute_block(&self, resources: &[Value]) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        for resource in resources {
            rows.extend(self.execute_resource(resource).await?);
        }
        Ok(rows)
    }
}

/// The cartesian product of two row sets, each `left` row followed by each
/// `right` row. Row parts are column values in column order: a select's
/// columns are one contiguous run of the view's columns, so joining two parts
/// is appending one to the other.
fn cartesian(left: Vec<Vec<Value>>, right: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    if let [only] = left.as_slice()
        && only.is_empty()
    {
        return right;
    }
    if let [only] = right.as_slice() {
        // The common case: one row on the right, appended to each left row in
        // place.
        return left
            .into_iter()
            .map(|mut row| {
                row.extend_from_slice(only);
                row
            })
            .collect();
    }
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in &left {
        for r in &right {
            let mut row = Vec::with_capacity(l.len() + r.len());
            row.extend_from_slice(l);
            row.extend_from_slice(r);
            out.push(row);
        }
    }
    out
}

/// The ordered column shape of a select list, validating `unionAll` branch
/// consistency.
fn shape(selects: &[SelectColumn]) -> Result<Vec<(String, ColumnType)>> {
    let mut cols = Vec::new();
    for select in selects {
        cols.extend(shape_of(select)?);
    }
    Ok(cols)
}

fn shape_of(select: &SelectColumn) -> Result<Vec<(String, ColumnType)>> {
    let mut cols = Vec::new();
    if let Some(columns) = &select.column {
        for col in columns {
            cols.push((col.name.clone(), column_type(col)));
        }
    }
    for nested in &select.select {
        cols.extend(shape_of(nested)?);
    }
    if let Some(branches) = &select.union_all {
        let mut shapes = branches.iter().map(shape_of).collect::<Result<Vec<_>>>()?;
        if let Some(first) = shapes.first() {
            let first_names: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
            for other in &shapes[1..] {
                let names: Vec<&str> = other.iter().map(|(n, _)| n.as_str()).collect();
                if names != first_names {
                    return Err(Error::InvalidViewDefinition(
                        "unionAll branches have mismatched column shape".to_string(),
                    ));
                }
            }
            cols.extend(shapes.swap_remove(0));
        }
    }
    Ok(cols)
}

struct Evaluator {
    resource_type: String,
    constants: std::collections::HashMap<String, String>,
    engine: FhirPathEngine,
    model: Arc<dyn ModelProvider + Send + Sync>,
}

/// A view path compiled for the engine: either a plain expression, or a
/// `lowBoundary()`/`highBoundary()` call whose base is evaluated first and the
/// SoF boundary rules applied to each value.
enum PreparedPath {
    Plain(Arc<CompiledPlan>),
    Boundary(Box<PreparedPath>, bool, BoundaryType),
}

/// A `select` node with its paths compiled. Its rows are its columns' values
/// in shape order: its own columns, then each nested select's, then the
/// `unionAll` branch's.
struct CompiledSelect {
    columns: Vec<CompiledColumn>,
    /// The `forEach` or `forEachOrNull` path, and whether an empty result
    /// still yields a (null) row. Unused when `repeat` is set.
    for_each: Option<(PreparedPath, bool)>,
    repeat: Vec<Arc<CompiledPlan>>,
    select: Vec<CompiledSelect>,
    union_all: Option<Vec<CompiledSelect>>,
    /// The row for an empty `forEachOrNull`.
    null_row: Vec<Value>,
}

/// A focus value and its `%rowIndex`, with the evaluation context for them
/// built on first use and shared by every path evaluated against them: the
/// `where` filters and top-level columns on the resource, or a select's
/// columns, `forEach` and nested selects on one element.
struct Focus<'a> {
    value: &'a FhirPathValue,
    rid: i64,
    ctx: OnceLock<EvaluationContext>,
}

impl<'a> Focus<'a> {
    fn new(value: &'a FhirPathValue, rid: i64) -> Self {
        Self {
            value,
            rid,
            ctx: OnceLock::new(),
        }
    }
}

struct CompiledColumn {
    name: String,
    collection: bool,
    path: PreparedPath,
}

impl Evaluator {
    /// Normalize a FHIRPath path string for the engine: substitute view
    /// constants (`%name`) into the string, then rewrite `ofType()` over FHIR
    /// choice elements (the engine, on an `EmptyModelProvider`, cannot map
    /// `value.ofType(Quantity)` → `valueQuantity` itself).
    fn prepare(&self, path: &str) -> Result<String> {
        let substituted = substitute_constants(path, &self.constants)?;
        Ok(rewrite_of_type(&substituted))
    }

    /// Parse a prepared expression into a plan the engine runs directly.
    fn plan(&self, prepared: &str) -> Result<Arc<CompiledPlan>> {
        self.engine
            .compile_plan(prepared)
            .map_err(|e| Error::FhirPath(e.to_string()))
    }

    /// Compile a column or forEach path.
    ///
    /// `lowBoundary()`/`highBoundary()` diverge from the engine's stock
    /// behaviour, so they are split off here: the base is compiled on its own
    /// and the SoF precision-boundary rules are applied to each of its values.
    /// The temporal flavour (`date`/`dateTime`/`time`) is taken from a leading
    /// `ofType(T)` on the base before the `ofType` rewrite collapses it.
    fn compile_path(&self, path: &str) -> Result<PreparedPath> {
        Ok(match boundary_call(path) {
            Some((base, low, hint)) => {
                PreparedPath::Boundary(Box::new(self.compile_path(&base)?), low, hint)
            }
            None => PreparedPath::Plain(self.plan(&self.prepare(path)?)?),
        })
    }

    fn compile_select(&self, select: &SelectColumn) -> Result<CompiledSelect> {
        let columns = select
            .column
            .iter()
            .flatten()
            .map(|col| {
                Ok(CompiledColumn {
                    name: col.name.clone(),
                    collection: col.collection.unwrap_or(false),
                    path: self.compile_path(&col.path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let repeat = select
            .repeat
            .iter()
            .map(|p| self.plan(&self.prepare(p)?))
            .collect::<Result<Vec<_>>>()?;
        // forEach wins when both are given; forEachOrNull's presence alone
        // decides whether an empty result keeps a row.
        let for_each = match (repeat.is_empty(), select.for_each.as_deref()) {
            (false, _) => None,
            (true, Some(path)) => Some(path),
            (true, None) => select.for_each_or_null.as_deref(),
        }
        .map(|path| self.compile_path(path))
        .transpose()?
        .map(|path| (path, select.for_each_or_null.is_some()));
        let nested = select
            .select
            .iter()
            .map(|s| self.compile_select(s))
            .collect::<Result<Vec<_>>>()?;
        let union_all = select
            .union_all
            .as_ref()
            .map(|branches| {
                branches
                    .iter()
                    .map(|b| self.compile_select(b))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let mut null_row = Vec::new();
        null_fill(select, &mut null_row);
        Ok(CompiledSelect {
            columns,
            for_each,
            repeat,
            select: nested,
            union_all,
            null_row,
        })
    }

    /// An evaluation context with `focus` as its input and `%rowIndex` bound to
    /// `rid`. Building one sets up the engine's variables, so callers
    /// evaluating several paths against the same focus build it once.
    ///
    /// `root` is the resource being evaluated; `focus` is it or a node within
    /// it. The context does not bind `root` yet: nested contexts take the focus
    /// as their root, so `getReferenceKey()` on a `#` reference inside
    /// `forEach` looks in the focus for `contained` (issue #57).
    fn context(&self, _root: &FhirPathValue, focus: &FhirPathValue, rid: i64) -> EvaluationContext {
        let ctx = EvaluationContext::new(
            Collection::single(focus.clone()),
            self.model.clone(),
            None,
            None,
            None,
        );
        // %rowIndex: the engine resolves `%rowIndex` by stripping `%` and
        // looking up `rowIndex`.
        ctx.set_variable("rowIndex".to_string(), FhirPathValue::integer(rid));
        ctx
    }

    /// The context for `focus`, built on first use.
    fn ctx<'f>(&self, root: &FhirPathValue, focus: &'f Focus<'_>) -> &'f EvaluationContext {
        focus
            .ctx
            .get_or_init(|| self.context(root, focus.value, focus.rid))
    }

    /// Run a compiled plan in a context.
    async fn eval_plan(&self, plan: &CompiledPlan, ctx: &EvaluationContext) -> Result<Collection> {
        self.engine
            .evaluate_plan(plan, ctx)
            .await
            .map(|r| r.value)
            .map_err(|e| Error::FhirPath(e.to_string()))
    }

    /// Evaluate a path in a context, as JSON values.
    async fn eval_path(&self, path: &PreparedPath, ctx: &EvaluationContext) -> Result<Vec<Value>> {
        match path {
            PreparedPath::Boundary(base, low, hint) => {
                let base_vals = Box::pin(self.eval_path(base, ctx)).await?;
                Ok(base_vals
                    .iter()
                    .map(|v| boundary(v, *low, *hint))
                    .filter(|v| !v.is_null())
                    .collect())
            }
            PreparedPath::Plain(plan) => Ok(collection_to_json(&self.eval_plan(plan, ctx).await?)),
        }
    }

    /// Evaluate a path in a context, as foci for nested evaluation.
    async fn eval_foci(
        &self,
        path: &PreparedPath,
        ctx: &EvaluationContext,
    ) -> Result<Vec<FhirPathValue>> {
        match path {
            PreparedPath::Plain(plan) => Ok(focus_values(&self.eval_plan(plan, ctx).await?)),
            PreparedPath::Boundary(..) => Ok(self
                .eval_path(path, ctx)
                .await?
                .into_iter()
                .map(FhirPathValue::resource)
                .collect()),
        }
    }

    /// Evaluate a select node against a focus value, yielding partial rows.
    async fn eval_select(
        &self,
        select: &CompiledSelect,
        root: &FhirPathValue,
        focus: &Focus<'_>,
    ) -> Result<Vec<Vec<Value>>> {
        // `repeat` recursively traverses the focus, collecting every node reached
        // by transitively re-applying the repeat path(s) (preorder, the focus
        // node itself excluded). %rowIndex tracks position in the flattened list.
        if !select.repeat.is_empty() {
            let mut foci = Vec::new();
            self.repeat_collect(&select.repeat, root, focus.value, &mut foci)
                .await?;
            let mut rows = Vec::new();
            for (idx, node) in foci.iter().enumerate() {
                let node = Focus::new(node, idx as i64);
                rows.extend(Box::pin(self.eval_level(select, root, &node)).await?);
            }
            return Ok(rows);
        }

        if let Some((path, or_null)) = &select.for_each {
            let elements = self.eval_foci(path, self.ctx(root, focus)).await?;
            if elements.is_empty() {
                if *or_null {
                    return Ok(vec![select.null_row.clone()]);
                }
                return Ok(Vec::new());
            }
            let mut rows = Vec::new();
            for (idx, elem) in elements.iter().enumerate() {
                let elem = Focus::new(elem, idx as i64);
                rows.extend(Box::pin(self.eval_level(select, root, &elem)).await?);
            }
            Ok(rows)
        } else {
            self.eval_level(select, root, focus).await
        }
    }

    /// Evaluate this select's columns, nested selects and unionAll against an
    /// already-focused value (after any forEach has selected the element).
    async fn eval_level(
        &self,
        select: &CompiledSelect,
        root: &FhirPathValue,
        focus: &Focus<'_>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut own = Vec::with_capacity(select.null_row.len());
        for col in &select.columns {
            own.push(self.column_value(col, self.ctx(root, focus)).await?);
        }
        let mut combos = vec![own];

        for nested in &select.select {
            let nrows = Box::pin(self.eval_select(nested, root, focus)).await?;
            combos = cartesian(combos, nrows);
        }

        if let Some(branches) = &select.union_all {
            let mut branch_rows = Vec::new();
            for branch in branches {
                branch_rows.extend(Box::pin(self.eval_select(branch, root, focus)).await?);
            }
            combos = cartesian(combos, branch_rows);
        }

        Ok(combos)
    }

    /// Preorder transitive closure of the `repeat` path(s): apply every path to
    /// `node`, push each result, then recurse into it. The starting node itself
    /// is not emitted.
    async fn repeat_collect(
        &self,
        plans: &[Arc<CompiledPlan>],
        root: &FhirPathValue,
        node: &FhirPathValue,
        out: &mut Vec<FhirPathValue>,
    ) -> Result<()> {
        // Iterative worklist (preorder) to avoid recursive async lifetimes.
        // Pushed in reverse so that, popped LIFO, children come out in source
        // order.
        let mut stack: Vec<FhirPathValue> = Vec::new();
        let seed = self.repeat_children(plans, root, node).await?;
        stack.extend(seed.into_iter().rev());
        while let Some(current) = stack.pop() {
            let children = self.repeat_children(plans, root, &current).await?;
            out.push(current);
            stack.extend(children.into_iter().rev());
        }
        Ok(())
    }

    /// The results of every `repeat` path applied to `node`, in path order.
    async fn repeat_children(
        &self,
        plans: &[Arc<CompiledPlan>],
        root: &FhirPathValue,
        node: &FhirPathValue,
    ) -> Result<Vec<FhirPathValue>> {
        let ctx = self.context(root, node, 0);
        let mut children = Vec::new();
        for plan in plans {
            children.extend(focus_values(&self.eval_plan(plan, &ctx).await?));
        }
        Ok(children)
    }

    async fn column_value(&self, col: &CompiledColumn, ctx: &EvaluationContext) -> Result<Value> {
        let values = self.eval_path(&col.path, ctx).await?;
        if col.collection {
            return Ok(Value::Array(values));
        }
        match values.len() {
            0 => Ok(Value::Null),
            1 => Ok(values.into_iter().next().unwrap()),
            _ => Err(Error::InvalidPath(format!(
                "column `{}` yields multiple values but is not a collection",
                col.name
            ))),
        }
    }

    /// Evaluate a top-level `where` filter. The expression must yield a boolean
    /// (or empty); a non-boolean result is an error per the spec. Empty → row
    /// excluded.
    async fn eval_where(&self, plan: &CompiledPlan, ctx: &EvaluationContext) -> Result<bool> {
        let coll = collection_to_json(&self.eval_plan(plan, ctx).await?);
        if coll.is_empty() {
            return Ok(false);
        }
        if coll.iter().any(|v| !v.is_boolean()) {
            return Err(Error::InvalidViewDefinition(
                "where path does not evaluate to a boolean".to_string(),
            ));
        }
        Ok(coll.iter().any(|v| v == &Value::Bool(true)))
    }
}

/// The all-null row for an empty `forEachOrNull`, in shape order. Per spec,
/// columns whose path is `%rowIndex` are bound to 0 rather than null.
fn null_fill(select: &SelectColumn, row: &mut Vec<Value>) {
    if let Some(columns) = &select.column {
        for col in columns {
            row.push(if col.path.trim() == "%rowIndex" {
                Value::Number(0.into())
            } else {
                Value::Null
            });
        }
    }
    for nested in &select.select {
        null_fill(nested, row);
    }
    if let Some(first) = select.union_all.as_ref().and_then(|b| b.first()) {
        null_fill(first, row);
    }
}

/// The items of a `forEach` or `repeat` result as foci for nested evaluation:
/// each a resource value over its node, as if built from its JSON, with
/// null and empty items dropped. A node is shared with the root resource,
/// not copied; other values (primitives, nested collections) go through JSON.
fn focus_values(coll: &Collection) -> Vec<FhirPathValue> {
    coll.iter()
        .filter_map(|v| match v {
            FhirPathValue::Resource(node, _, _) if !node.is_null() => {
                Some(FhirPathValue::resource_from_node(node.clone()))
            }
            other => {
                let json = value_to_json(other);
                (!json.is_null()).then(|| FhirPathValue::resource(json))
            }
        })
        .collect()
}

/// Convert a FHIRPath [`Collection`] into a flat `Vec` of JSON values, dropping
/// `null`/empty entries so navigation results round-trip the way the conformance
/// `expect` multisets assume.
fn collection_to_json(coll: &Collection) -> Vec<Value> {
    coll.iter()
        .map(value_to_json)
        .filter(|v| !v.is_null())
        .collect()
}

/// Convert a single [`FhirPathValue`] to JSON. A nested collection flattens
/// (collection columns are arrays at the row level, not nested here).
fn value_to_json(v: &FhirPathValue) -> Value {
    match v {
        FhirPathValue::Collection(c) => {
            let items: Vec<Value> = c
                .iter()
                .map(value_to_json)
                .filter(|x| !x.is_null())
                .collect();
            if items.len() == 1 {
                items.into_iter().next().unwrap()
            } else {
                Value::Array(items)
            }
        }
        FhirPathValue::Empty => Value::Null,
        other => other.to_json_value(),
    }
}

/// Rewrite `<base>.ofType(Type)` so a FHIR choice element collapses onto the
/// concrete property (`value.ofType(Quantity)` → `valueQuantity`,
/// `value.ofType(string)` → `valueString`), matching how the SQL generator and
/// the FHIR JSON representation name choice elements. The type name is
/// capitalized exactly as the choice-element suffix requires. The engine, on an
/// `EmptyModelProvider`, cannot do this mapping itself.
fn rewrite_of_type(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for the literal `.ofType(` at position i.
        if path[i..].starts_with(".ofType(") {
            // The base identifier is the trailing run of [A-Za-z0-9_] already in
            // `out`. Pop it so we can fuse the capitalized type onto it.
            let base_start = out
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map(|p| p + 1)
                .unwrap_or(0);
            let base: String = out[base_start..].to_string();
            // Find the matching close paren for the argument.
            let arg_start = i + ".ofType(".len();
            if let Some(rel_close) = path[arg_start..].find(')') {
                let arg = path[arg_start..arg_start + rel_close].trim();
                // Only a bare type identifier is a choice rewrite; anything else
                // (shouldn't occur) is left to the engine.
                if !base.is_empty() && is_simple_ident(arg) {
                    out.truncate(base_start);
                    out.push_str(&base);
                    out.push_str(&capitalize_first(arg));
                    i = arg_start + rel_close + 1;
                    continue;
                }
            }
        }
        let ch = path[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_simple_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn column_type(col: &Column) -> ColumnType {
    if col.collection.unwrap_or(false) {
        return ColumnType::Json;
    }
    // An `ansi/type` tag explicitly overrides the inferred column type.
    if let Some(ansi) = ansi_type_tag(col) {
        return ColumnType::from_ansi_type(ansi);
    }
    col.col_type
        .as_deref()
        .map(ColumnType::from_fhir_type)
        .unwrap_or(ColumnType::String)
}

/// The value of a column's `ansi/type` tag, if present.
pub(crate) fn ansi_type_tag(col: &Column) -> Option<&str> {
    col.tag
        .iter()
        .find(|t| t.name == "ansi/type")
        .and_then(|t| t.value.as_deref())
}

// --- SoF custom FHIRPath functions ---------------------------------------
//
// `getResourceKey()` and `getReferenceKey([type])` are SQL-on-FHIR special
// functions, not standard FHIRPath. They are registered as lazy functions so
// they can read the evaluation context's root resource (for fragment `#id`
// references resolving into `contained[]`). The first argument (if any) is a
// bare type identifier read directly off the AST, never evaluated.

/// A value's tree: the node itself for a resource or element value, else one
/// built from its JSON form (a primitive has no fields, so lookups find none).
fn value_node(v: &FhirPathValue) -> Cow<'_, FhirNode> {
    match v {
        FhirPathValue::Resource(node, _, _) => Cow::Borrowed(node),
        other => Cow::Owned(FhirNode::from_json(&other.to_json_value())),
    }
}

/// The resource type carried by the contained resource with the given local id,
/// if the root resource lists it under `contained[]`.
fn contained_type(root: &FhirNode, local_id: &str) -> Option<String> {
    root.get("contained")
        .and_then(FhirNode::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("id").and_then(FhirNode::as_str) == Some(local_id))
        .and_then(|c| c.get("resourceType").and_then(FhirNode::as_str))
        .map(str::to_string)
}

/// `getReferenceKey()` on a single Reference value. Relative/absolute `Type/id`
/// references key on the id (filtered by the optional expected type). Fragment
/// `#id` references resolve into the root resource's `contained[]` and key on
/// the local id, so the value matches `getResourceKey()` on that contained
/// resource. Absolute URL and `urn:` references are not keyed. `root` is
/// called only for a fragment reference.
fn reference_key<'a>(
    item: &FhirNode,
    root: impl FnOnce() -> Option<&'a FhirNode>,
    want_type: Option<&str>,
) -> Option<String> {
    let reference = item.get("reference")?.as_str()?;
    if let Some(local) = reference.strip_prefix('#') {
        if local.is_empty() {
            return None;
        }
        let actual = root()
            .and_then(|root| contained_type(root, local))
            .or_else(|| {
                item.get("type")
                    .and_then(FhirNode::as_str)
                    .map(str::to_string)
            });
        return match (want_type, actual.as_deref()) {
            (Some(t), Some(a)) if t != a => None,
            (Some(_), None) => None,
            _ => Some(local.to_string()),
        };
    }
    if reference.starts_with("urn:") || reference.contains("://") {
        return None;
    }
    let trimmed = reference.trim_start_matches('/');
    let mut parts = trimmed.split('/');
    let rtype = parts.next()?;
    let id = parts.next()?;
    if id.is_empty() {
        return None;
    }
    match want_type {
        Some(t) if t != rtype => None,
        _ => Some(id.to_string()),
    }
}

/// The root resource from the evaluation context, falling back to the first
/// function input if no root is set.
fn root_node(context: &EvaluationContext, input: &Collection) -> Option<FhirNode> {
    let root = context
        .root_resource_value()
        .or_else(|| input.first().cloned())?;
    Some(value_node(&root).into_owned())
}

struct GetResourceKey;

#[async_trait]
impl LazyFunctionEvaluator for GetResourceKey {
    async fn evaluate(
        &self,
        input: Collection,
        _context: &EvaluationContext,
        _args: Vec<ExpressionNode>,
        _evaluator: AsyncNodeEvaluator<'_>,
    ) -> octofhir_fhirpath::Result<octofhir_fhirpath::EvaluationResult> {
        // getResourceKey() keys a resource on its `id`.
        let values: Vec<FhirPathValue> = input
            .iter()
            .filter_map(|v| {
                value_node(v)
                    .get("id")
                    .and_then(FhirNode::as_str)
                    .map(FhirPathValue::string)
            })
            .collect();
        Ok(octofhir_fhirpath::EvaluationResult::from_values(values))
    }

    fn metadata(&self) -> &FunctionMetadata {
        static META: std::sync::LazyLock<FunctionMetadata> =
            std::sync::LazyLock::new(|| sof_fn_metadata("getResourceKey", 0, Some(0)));
        &META
    }
}

struct GetReferenceKey;

#[async_trait]
impl LazyFunctionEvaluator for GetReferenceKey {
    async fn evaluate(
        &self,
        input: Collection,
        context: &EvaluationContext,
        args: Vec<ExpressionNode>,
        _evaluator: AsyncNodeEvaluator<'_>,
    ) -> octofhir_fhirpath::Result<octofhir_fhirpath::EvaluationResult> {
        // Optional bare type identifier, read directly off the AST (not
        // evaluated): getReferenceKey(Patient).
        let want_type = match args.first() {
            Some(node) => Some(ast_type_name(node)?),
            None => None,
        };
        // Looked up only for a `#` reference, and then once.
        let root = OnceCell::new();
        let values: Vec<FhirPathValue> = input
            .iter()
            .filter_map(|v| {
                reference_key(
                    &value_node(v),
                    || root.get_or_init(|| root_node(context, &input)).as_ref(),
                    want_type.as_deref(),
                )
            })
            .map(FhirPathValue::string)
            .collect();
        Ok(octofhir_fhirpath::EvaluationResult::from_values(values))
    }

    fn metadata(&self) -> &FunctionMetadata {
        static META: std::sync::LazyLock<FunctionMetadata> =
            std::sync::LazyLock::new(|| sof_fn_metadata("getReferenceKey", 0, Some(1)));
        &META
    }
}

/// Extract a bare type name from an argument node (`Patient`, `Organization`,
/// or a `System.String`-style qualified type). Errors on anything else.
fn ast_type_name(node: &ExpressionNode) -> octofhir_fhirpath::Result<String> {
    match node {
        ExpressionNode::Identifier(n) => Ok(n.name.clone()),
        ExpressionNode::TypeInfo(t) => Ok(t.name.clone()),
        ExpressionNode::PropertyAccess(p) => match p.object.as_ref() {
            // Qualified type name like FHIR.Patient — take the trailing segment.
            ExpressionNode::Identifier(_) => Ok(p.property.clone()),
            _ => Err(FhirPathError::evaluation_error(
                FP0053,
                "getReferenceKey() type argument must be a type name",
            )),
        },
        _ => Err(FhirPathError::evaluation_error(
            FP0053,
            "getReferenceKey() type argument must be a type name",
        )),
    }
}

/// Minimal metadata for a SoF custom function (polymorphic input, no model or
/// terminology dependency).
fn sof_fn_metadata(name: &str, min: usize, max: Option<usize>) -> FunctionMetadata {
    FunctionMetadata {
        name: name.to_string(),
        description: format!("SQL-on-FHIR {name}()"),
        signature: FunctionSignature {
            input_type: "Any".to_string(),
            parameters: Vec::new(),
            return_type: "String".to_string(),
            polymorphic: true,
            min_params: min,
            max_params: max,
        },
        argument_evaluation: Default::default(),
        null_propagation: Default::default(),
        empty_propagation: EmptyPropagation::NoPropagation,
        deterministic: true,
        category: FunctionCategory::Utility,
        requires_terminology: false,
        requires_model: false,
    }
}

// --- SoF-divergent overrides: join / lowBoundary / highBoundary -----------
//
// These three functions are overridden so the in-memory path matches the
// SQL-on-FHIR conformance suite, where the engine's stock behaviour differs:
//   * join() — a zero-argument call defaults to the empty separator (and an
//     empty input collection yields empty, not "").
//   * low/highBoundary() — the spec's precision-boundary rules over decimals
//     and partial date/dateTime/time strings.

/// The FHIR primitive string representation of a JSON value.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

struct JoinFn;

#[async_trait]
impl PureFunctionEvaluator for JoinFn {
    async fn evaluate(
        &self,
        input: Collection,
        args: Vec<Collection>,
    ) -> octofhir_fhirpath::Result<octofhir_fhirpath::EvaluationResult> {
        // join() over an empty collection yields the empty collection (i.e.
        // null), not an empty string. (FHIR/sql-on-fhir.js fn_join)
        if input.is_empty() {
            return Ok(octofhir_fhirpath::EvaluationResult::from_values(Vec::new()));
        }
        let sep = match args.first().and_then(|c| c.first()) {
            Some(v) => value_to_string(&v.to_json_value()),
            None => String::new(),
        };
        let parts: Vec<String> = input
            .iter()
            .map(|v| value_to_string(&v.to_json_value()))
            .collect();
        Ok(octofhir_fhirpath::EvaluationResult::from_values(vec![
            FhirPathValue::string(parts.join(&sep)),
        ]))
    }

    fn metadata(&self) -> &FunctionMetadata {
        static META: std::sync::LazyLock<FunctionMetadata> =
            std::sync::LazyLock::new(|| sof_fn_metadata("join", 0, Some(1)));
        &META
    }
}

/// The FHIR temporal type a boundary operates on, inferred from a leading
/// `ofType(T)` on the base.
#[derive(Debug, Clone, Copy)]
enum BoundaryType {
    Date,
    DateTime,
    Time,
    Unknown,
}

/// If `path` is a `…lowBoundary()`/`…highBoundary()` call, return the base path
/// string, the low flag, and the temporal hint from a leading `ofType(T)`.
fn boundary_call(path: &str) -> Option<(String, bool, BoundaryType)> {
    let ast = octofhir_fhirpath::parse_ast(path).ok()?;
    let (object, method) = match &ast {
        ExpressionNode::MethodCall(m)
            if m.method == "lowBoundary" || m.method == "highBoundary" =>
        {
            (m.object.as_ref(), m.method.as_str())
        }
        _ => return None,
    };
    let low = method == "lowBoundary";
    let hint = boundary_hint(object);
    Some((unparse(object), low, hint))
}

/// Recover a FHIRPath source string for a navigation/`ofType` sub-expression,
/// enough to re-evaluate the boundary's base through the engine.
fn unparse(node: &ExpressionNode) -> String {
    match node {
        ExpressionNode::Identifier(n) => n.name.clone(),
        ExpressionNode::PropertyAccess(p) => format!("{}.{}", unparse(&p.object), p.property),
        ExpressionNode::MethodCall(m) => {
            let args: Vec<String> = m.arguments.iter().map(unparse).collect();
            format!("{}.{}({})", unparse(&m.object), m.method, args.join(", "))
        }
        ExpressionNode::FunctionCall(f) => {
            let args: Vec<String> = f.arguments.iter().map(unparse).collect();
            format!("{}({})", f.name, args.join(", "))
        }
        ExpressionNode::Parenthesized(e) => format!("({})", unparse(e)),
        ExpressionNode::TypeInfo(t) => t.name.clone(),
        ExpressionNode::Literal(l) => match &l.value {
            octofhir_fhirpath::LiteralValue::String(s) => format!("'{s}'"),
            other => other.to_string(),
        },
        _ => String::new(),
    }
}

fn boundary_hint(object: &ExpressionNode) -> BoundaryType {
    match object {
        ExpressionNode::MethodCall(m) if m.method == "ofType" => {
            let name = match m.arguments.first() {
                Some(ExpressionNode::Identifier(n)) => n.name.to_lowercase(),
                Some(ExpressionNode::TypeInfo(t)) => t.name.to_lowercase(),
                _ => return BoundaryType::Unknown,
            };
            match name.as_str() {
                "datetime" | "instant" => BoundaryType::DateTime,
                "date" => BoundaryType::Date,
                "time" => BoundaryType::Time,
                _ => BoundaryType::Unknown,
            }
        }
        ExpressionNode::PropertyAccess(p) => boundary_hint(&p.object),
        ExpressionNode::Parenthesized(e) => boundary_hint(e),
        _ => BoundaryType::Unknown,
    }
}

/// FHIR precision boundary; numbers use the decimal half-ulp, temporal strings
/// are widened to their precision-filled bound.
fn boundary(v: &Value, low: bool, hint: BoundaryType) -> Value {
    match v {
        Value::Number(n) => {
            let text = n.to_string();
            let scale = text
                .split_once('.')
                .map(|(_, frac)| frac.len())
                .unwrap_or(0);
            let half = 0.5 / 10f64.powi(scale as i32);
            let base = n.as_f64().unwrap_or(0.0);
            number_value(if low { base - half } else { base + half })
        }
        Value::String(s) => Value::String(boundary_string(s, low, hint)),
        other => other.clone(),
    }
}

fn number_value(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Widen a partial date/dateTime/time string to its low/high precision bound,
/// using the temporal `hint` where present (else inferred from the string).
fn boundary_string(s: &str, low: bool, hint: BoundaryType) -> String {
    let is_time = matches!(hint, BoundaryType::Time)
        || (matches!(hint, BoundaryType::Unknown) && s.contains(':') && !s.contains('-'));
    if is_time {
        return match (s.len(), low) {
            (2, true) => format!("{s}:00:00.000"),
            (2, false) => format!("{s}:59:59.999"),
            (5, true) => format!("{s}:00.000"),
            (5, false) => format!("{s}:59.999"),
            (8, true) => format!("{s}.000"),
            (8, false) => format!("{s}.999"),
            _ => s.to_string(),
        };
    }
    let is_datetime = matches!(hint, BoundaryType::DateTime) || s.contains('T');
    if is_datetime {
        let time = if low {
            "T00:00:00.000+14:00"
        } else {
            "T23:59:59.999-12:00"
        };
        return match (s.len(), low) {
            (4, true) => format!("{s}-01-01{time}"),
            (4, false) => format!("{s}-12-31{time}"),
            (7, true) => format!("{s}-01{time}"),
            (7, false) => format!("{s}-{}{time}", last_day_of_month(s)),
            (10, _) => format!("{s}{time}"),
            _ => s.to_string(),
        };
    }
    match (s.len(), low) {
        (4, true) => format!("{s}-01-01"),
        (4, false) => format!("{s}-12-31"),
        (7, true) => format!("{s}-01"),
        (7, false) => format!("{s}-{}", last_day_of_month(s)),
        _ => s.to_string(),
    }
}

fn last_day_of_month(year_month: &str) -> String {
    let mut it = year_month.split('-');
    let year: i32 = it.next().and_then(|y| y.parse().ok()).unwrap_or(2000);
    let month: u32 = it.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    };
    format!("{last:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(v: Value) -> ViewDefinition {
        ViewDefinition::from_json(&v).expect("parse ViewDefinition")
    }

    fn patient(i: usize, names: usize) -> Value {
        let name: Vec<Value> = (0..names)
            .map(|n| json!({ "family": format!("f{i}-{n}") }))
            .collect();
        json!({ "resourceType": "Patient", "id": format!("p{i}"), "name": name })
    }

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
        assert_eq!(sequential.row_count, expected);
        for threads in [1, 3, 8, 64, 5000] {
            let parallel = execute_parallel(&v, &resources, threads).unwrap();
            assert_eq!(parallel.data, sequential.data, "{threads} threads");
        }
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

    #[test]
    fn paths_are_checked_when_the_view_is_compiled() {
        // Column paths are compiled with the view, so a bad one fails even
        // when no resource reaches it.
        for path in ["%missing", "name.where(", "repeat"] {
            let v = view(json!({
                "resource": "Patient",
                "select": [{ "forEach": "contact", "column": [{ "name": "c", "path": path }] }]
            }));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(execute(&v, &[patient(1, 1)]));
            if path == "repeat" {
                // A plain identifier, not a call: it compiles.
                assert!(result.is_ok(), "{path}: {result:?}");
            } else {
                assert!(result.is_err(), "{path}: {result:?}");
            }
        }
    }

    #[test]
    fn nested_foci_keep_contained_keys() {
        // forEach foci are nodes of the resource; keys read off them match
        // the ones read off the resource.
        let resource = json!({
            "resourceType": "Patient",
            "id": "p1",
            "contained": [
                { "resourceType": "Practitioner", "id": "c1" },
                { "resourceType": "Organization", "id": "c2" }
            ],
            "generalPractitioner": [
                { "reference": "#c1" },
                { "reference": "Practitioner/x" }
            ]
        });
        let v = view(json!({
            "resource": "Patient",
            "select": [
                { "column": [
                    { "name": "gp", "path": "generalPractitioner.getReferenceKey(Practitioner)", "collection": true }
                ] },
                { "forEach": "contained", "column": [
                    { "name": "key", "path": "getResourceKey()" },
                    { "name": "type", "path": "resourceType" }
                ] }
            ]
        }));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute(&v, &[resource])).unwrap();
        assert_eq!(
            result.data,
            vec![
                vec![json!(["c1", "x"]), json!("c1"), json!("Practitioner")],
                vec![json!(["c1", "x"]), json!("c2"), json!("Organization")],
            ]
        );
    }

    #[test]
    fn execute_resource_future_is_send() {
        // Callers may spawn per-resource evaluation on a multi-threaded runtime.
        fn assert_send<T: Send>(_: T) {}
        let rt = tokio::runtime::Runtime::new().unwrap();
        let compiled = rt
            .block_on(CompiledView::compile(&view(json!({
                "resource": "Patient",
                "select": [{ "column": [{ "name": "id", "path": "id" }] }]
            }))))
            .unwrap();
        let resource = patient(1, 1);
        assert_send(compiled.execute_resource(&resource));
    }
}
