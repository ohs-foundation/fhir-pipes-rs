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
    Collection, EvaluationContext, ExpressionNode, FhirPathEngine, FhirPathError, FhirPathValue,
    ModelProvider, create_function_registry,
};
use serde_json::Value;

use crate::column::{ColumnInfo, ColumnType};
use crate::fhir_model::{self, TableModelProvider};
use crate::lexer::{Kind, Token, tokens, variable_name};
use crate::runner::ViewResult;
use crate::sql_generator::{build_constants, substitute_constants_keeping};
use crate::view_definition::{Column, SelectColumn, ViewDefinition};
use crate::{Error, Result};
use octofhir_fhirpath::core::model_provider::TypeInfo;

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
    Ok(ViewResult {
        columns: compiled.columns().to_vec(),
        data,
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

/// A current-thread Tokio runtime for one evaluation thread.
fn thread_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::FhirPath(format!("building runtime: {e}")))
}

fn thread_panicked() -> Error {
    Error::FhirPath("evaluation thread panicked".to_string())
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

        // Build the engine once with the SoF custom functions registered. The
        // model is the FHIR type table built into the crate (R4 and R5; no
        // network, no FHIR package). ofType() over a choice element is still
        // rewritten to the element's JSON name, see `rewrite_of_type`.
        let mut registry = create_function_registry();
        registry.register_lazy_function(Arc::new(GetResourceKey));
        registry.register_lazy_function(Arc::new(GetReferenceKey));
        // SoF-divergent function semantics: override the engine's defaults to
        // match the SQL-on-FHIR conformance suite (join() with no separator,
        // and the spec's precision-boundary rules for low/highBoundary).
        registry.register_pure_function(Arc::new(JoinFn));
        let model: Arc<dyn ModelProvider + Send + Sync> = Arc::new(TableModelProvider);
        let engine = FhirPathEngine::new(Arc::new(registry), model.clone())
            .await
            .map_err(|e| Error::FhirPath(format!("building FHIRPath engine: {e}")))?;

        let ev = Evaluator {
            resource_type: view.resource.clone(),
            constants,
            engine,
            model,
            uses_context: AtomicBool::new(false),
        };

        let mut mismatched = false;
        let shape: Vec<(String, ColumnType)> = view
            .output_columns(|| mismatched = true)
            .into_iter()
            .map(|col| (col.name.clone(), column_type(col)))
            .collect();
        if mismatched {
            return Err(Error::InvalidViewDefinition(
                "unionAll branches have mismatched column shape".to_string(),
            ));
        }
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
            .map(|w| ev.plan(&ev.prepare(&w.path, Some(&view.resource))?))
            .collect::<Result<Vec<_>>>()?;
        let selects = view
            .select
            .iter()
            .map(|s| ev.compile_select(s, Some(&view.resource)))
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

    /// [`CompiledView::compile`] for non-async callers. Compiles on a thread
    /// of its own, so that calling this from inside an async runtime never
    /// starts a runtime on that runtime's thread.
    ///
    /// # Errors
    ///
    /// Same as [`CompiledView::compile`].
    pub fn compile_blocking(view: &ViewDefinition) -> Result<Self> {
        std::thread::scope(|s| {
            s.spawn(|| thread_runtime()?.block_on(Self::compile(view)))
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
        let threads = threads.clamp(1, resources.len().max(1));
        let block = resources
            .len()
            .div_ceil(threads * BLOCKS_PER_THREAD)
            .clamp(1, MAX_BLOCK);
        let blocks: Vec<&[Value]> = resources.chunks(block).collect();
        let blocks = &blocks;
        let next = &AtomicUsize::new(0);
        let failed = &AtomicBool::new(false);
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads.min(blocks.len()))
                .map(|_| {
                    s.spawn(move || -> Result<Vec<(usize, Result<Rows>)>> {
                        thread_runtime()?.block_on(async {
                            let mut done = Vec::new();
                            while !failed.load(Ordering::Relaxed) {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some(part) = blocks.get(i) else { break };
                                let rows = self.execute_block(part).await;
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

    /// The output columns, in order.
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
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
        // below shares it, and every context is a child of its context.
        let root = FhirPathValue::resource_from_node(FhirNode::from_json(resource));
        let root_ctx = self.ev.root_context(&root);
        let focus = Focus::with_context(&root, root_ctx.clone());
        for plan in &self.wheres {
            if !self
                .ev
                .eval_where(plan, self.ev.ctx(&root_ctx, &focus))
                .await?
            {
                return Ok(Vec::new());
            }
        }

        let mut rows = vec![Vec::with_capacity(self.columns.len())];
        for select in &self.selects {
            let srows = self.ev.eval_select(select, &root_ctx, &focus).await?;
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

struct Evaluator {
    resource_type: String,
    constants: std::collections::HashMap<String, String>,
    engine: FhirPathEngine,
    model: Arc<dyn ModelProvider + Send + Sync>,
    /// Whether any path uses `%context`, so contexts bind [`CONTEXT_VAR`].
    /// Set while the view compiles.
    uses_context: AtomicBool,
}

/// The variable `%context` is renamed to ([`rename_context`]). Each context
/// binds it to its own focus: the engine's own `%context` is an alias of the
/// root, which child contexts share, while FHIRPath defines `%context` as the
/// node the expression started from.
const CONTEXT_VAR: &str = "_sofContext";

/// A view path compiled for the engine: either a plain expression, or a
/// `lowBoundary()`/`highBoundary()` call whose base is evaluated first and
/// each value's boundary then computed by the engine (see
/// [`Evaluator::eval_boundary`]).
enum PreparedPath {
    Plain(Arc<CompiledPlan>),
    /// The base of a boundary call: a node of the parsed path, which the
    /// engine evaluates directly.
    Node(ExpressionNode),
    /// Base, low (else high), the temporal type the path names, and the
    /// precision argument if one is given.
    Boundary(
        Box<PreparedPath>,
        bool,
        BoundaryType,
        Option<ExpressionNode>,
    ),
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
/// columns, `forEach` and nested selects on one element. Each path still runs
/// in a scope of its own ([`Evaluator::eval_plan`]).
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

    /// The resource itself as the focus, with the resource's context.
    fn with_context(value: &'a FhirPathValue, ctx: EvaluationContext) -> Self {
        Self {
            value,
            rid: 0,
            ctx: OnceLock::from(ctx),
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
    /// choice elements (the engine cannot map `value.ofType(Quantity)` →
    /// `valueQuantity` itself, see `rewrite_of_type`).
    ///
    /// `%` names that are not constants are left for the engine when they are
    /// FHIRPath environment variables or are bound by a `defineVariable()` in
    /// the same path; any other is an undefined constant.
    fn prepare(&self, path: &str, root: Option<&str>) -> Result<String> {
        let defined = defined_variables(path);
        let substituted = substitute_constants_keeping(path, &self.constants, |name| {
            is_environment_variable(name)
                || defined
                    .as_ref()
                    .is_none_or(|defined| defined.iter().any(|d| d == name))
        })?;
        let prepared = rewrite_of_type(&substituted, root);
        Ok(match rename_context(&prepared) {
            Some(renamed) => {
                self.uses_context.store(true, Ordering::Relaxed);
                renamed
            }
            None => prepared,
        })
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
    /// behaviour, so they are split off here: the base, any expression, is
    /// evaluated on its own and the SoF precision-boundary rules are applied
    /// to each of its values. The temporal flavour (`date`/`dateTime`/`time`)
    /// is taken from a leading `ofType(T)` on the base in the path as written,
    /// before the `ofType` rewrite.
    fn compile_path(&self, path: &str, root: Option<&str>) -> Result<PreparedPath> {
        let plan = self.plan(&self.prepare(path, root)?)?;
        let written = octofhir_fhirpath::parse_ast(path).ok();
        Ok(boundary_path(plan.ast(), written.as_ref()).unwrap_or(PreparedPath::Plain(plan)))
    }

    /// `root` is where the select's paths start, for the `ofType` rewrite:
    /// the view's resource type, or the type or backbone element of the
    /// `forEach` items above it (`Patient.contact`); `None` when not known,
    /// as under `repeat`.
    fn compile_select(&self, select: &SelectColumn, root: Option<&str>) -> Result<CompiledSelect> {
        let inner = if !select.repeat.is_empty() {
            None
        } else if let Some(path) = select
            .for_each
            .as_ref()
            .or(select.for_each_or_null.as_ref())
        {
            focus_owner(path, root)
        } else {
            root.map(str::to_string)
        };
        let inner = inner.as_deref();
        let columns = select
            .column
            .iter()
            .flatten()
            .map(|col| {
                Ok(CompiledColumn {
                    name: col.name.clone(),
                    collection: col.collection.unwrap_or(false),
                    path: self.compile_path(&col.path, inner)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let repeat = select
            .repeat
            .iter()
            .map(|p| self.plan(&self.prepare(p, None)?))
            .collect::<Result<Vec<_>>>()?;
        // forEach wins when both are given; forEachOrNull's presence alone
        // decides whether an empty result keeps a row.
        let for_each = match (repeat.is_empty(), select.for_each.as_deref()) {
            (false, _) => None,
            (true, Some(path)) => Some(path),
            (true, None) => select.for_each_or_null.as_deref(),
        }
        .map(|path| self.compile_path(path, root))
        .transpose()?
        .map(|path| (path, select.for_each_or_null.is_some()));
        let nested = select
            .select
            .iter()
            .map(|s| self.compile_select(s, inner))
            .collect::<Result<Vec<_>>>()?;
        let union_all = select
            .union_all
            .as_ref()
            .map(|branches| {
                branches
                    .iter()
                    .map(|b| self.compile_select(b, inner))
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

    /// The evaluation context of a resource: the resource is its input and
    /// its root, so `%resource`, `%rootResource` and `%context` are the
    /// resource, and `%rowIndex` is 0. Building one sets up the engine's
    /// shared state (caches, providers); every other context for the resource
    /// is a child of this one ([`Evaluator::context`]) and shares it.
    fn root_context(&self, root: &FhirPathValue) -> EvaluationContext {
        let ctx = EvaluationContext::new(
            Collection::single(root.clone()),
            self.model.clone(),
            None,
            None,
            None,
        );
        // The engine binds %resource to the root itself, but not
        // %rootResource.
        ctx.set_variable("rootResource".to_string(), root.clone());
        if self.uses_context.load(Ordering::Relaxed) {
            ctx.set_variable(CONTEXT_VAR.to_string(), root.clone());
        }
        // %rowIndex: the engine resolves `%rowIndex` by stripping `%` and
        // looking up `rowIndex`.
        ctx.set_variable("rowIndex".to_string(), FhirPathValue::integer(0));
        ctx
    }

    /// A context with `focus`, a node within the resource of `root`, as its
    /// input and `$this`, and `%rowIndex` bound to `rid`. Callers evaluating
    /// several paths against the same focus build it once and run each path
    /// in a scope nested under it.
    ///
    /// As a child of the resource's context, it keeps the resource as its
    /// root: `%resource` inside `forEach` is the resource, and
    /// `getReferenceKey()` resolves a `#` reference against the resource's
    /// `contained`. `%context` is the focus, the node the path starts from.
    fn context(
        &self,
        root: &EvaluationContext,
        focus: &FhirPathValue,
        rid: i64,
    ) -> EvaluationContext {
        let ctx = root.create_child_context(Collection::single(focus.clone()));
        // Without this, `$this` outside a lambda would resolve to the root.
        ctx.set_this(focus.clone());
        if self.uses_context.load(Ordering::Relaxed) {
            ctx.set_variable(CONTEXT_VAR.to_string(), focus.clone());
        }
        ctx.set_variable("rowIndex".to_string(), FhirPathValue::integer(rid));
        ctx
    }

    /// The context for `focus`, built on first use.
    fn ctx<'f>(&self, root: &EvaluationContext, focus: &'f Focus<'_>) -> &'f EvaluationContext {
        focus
            .ctx
            .get_or_init(|| self.context(root, focus.value, focus.rid))
    }

    /// Run a compiled plan in its own variable scope under `ctx`.
    ///
    /// `defineVariable()` writes into the context it is given, so each path
    /// gets a nested scope: a variable defined by one path (a `where` filter,
    /// a column) is not seen by the next, while the focus, `%rowIndex` and the
    /// engine's caches stay shared.
    async fn eval_plan(&self, plan: &CompiledPlan, ctx: &EvaluationContext) -> Result<Collection> {
        self.engine
            .evaluate_plan(plan, &ctx.nest())
            .await
            .map(|r| r.value)
            .map_err(|e| Error::FhirPath(e.to_string()))
    }

    /// Evaluate a path in a context, as JSON values.
    async fn eval_path(&self, path: &PreparedPath, ctx: &EvaluationContext) -> Result<Vec<Value>> {
        match path {
            PreparedPath::Boundary(base, low, hint, precision) => {
                self.eval_boundary(base, *low, *hint, precision.as_ref(), ctx)
                    .await
            }
            PreparedPath::Plain(plan) => Ok(collection_to_json(&self.eval_plan(plan, ctx).await?)),
            PreparedPath::Node(node) => Ok(collection_to_json(&self.eval_node(node, ctx).await?)),
        }
    }

    /// `lowBoundary()`/`highBoundary()` over the values of `base`.
    ///
    /// Values read from resource JSON reach the engine untyped (a dateTime
    /// is a string), so its own boundary functions cannot apply to them.
    /// Each value is written back as a FHIRPath literal of the type it
    /// stands for (`2010-10-10` under `ofType(dateTime)` becomes
    /// `@2010-10-10T`) and the engine evaluates the boundary of that literal.
    /// A value that is not a number, a Quantity or a well-formed date, time
    /// or dateTime has no boundary, so no data text ever reaches the
    /// expression.
    async fn eval_boundary(
        &self,
        base: &PreparedPath,
        low: bool,
        hint: BoundaryType,
        precision: Option<&ExpressionNode>,
        ctx: &EvaluationContext,
    ) -> Result<Vec<Value>> {
        let digits = match precision {
            None => None,
            Some(node) => match self.eval_node(node, ctx).await?.iter().next() {
                Some(FhirPathValue::Integer(i, ..)) => Some(*i),
                None => return Ok(Vec::new()),
                Some(other) => {
                    return Err(Error::FhirPath(format!(
                        "lowBoundary()/highBoundary() precision must be an integer, got {}",
                        other.to_json_value()
                    )));
                }
            },
        };
        let function = if low { "lowBoundary" } else { "highBoundary" };
        let mut out = Vec::new();
        for value in Box::pin(self.eval_values(base, ctx)).await? {
            let Some(input) = boundary_input(&value, hint) else {
                continue;
            };
            let kind = input.kind();
            // The engine keeps a date, time or dateTime at its own precision
            // when none is given; the spec widens it to the type's full
            // precision.
            let digits = digits.or(kind.full_precision());
            let arg = digits.map(|d| d.to_string()).unwrap_or_default();
            let result = match input {
                BoundaryInput::Typed(_) => {
                    let focus = ctx.create_child_context(Collection::single(value.clone()));
                    focus.set_this(value.clone());
                    self.engine
                        .evaluate(&format!("$this.{function}({arg})"), &focus)
                        .await
                }
                BoundaryInput::Literal(literal, _) => {
                    self.engine
                        .evaluate(&format!("{literal}.{function}({arg})"), &ctx.nest())
                        .await
                }
            }
            .map_err(|e| Error::FhirPath(e.to_string()))?;
            out.extend(
                collection_to_json(&result.value)
                    .into_iter()
                    .map(|v| match v {
                        // A time's JSON form has no leading `T`.
                        Value::String(t) if kind == BoundaryType::Time => {
                            Value::String(t.trim_start_matches('T').to_string())
                        }
                        v => v,
                    }),
            );
        }
        Ok(out)
    }

    /// The values of a path as the engine returns them, typed where it types
    /// them (literals, computed values) and as resource JSON otherwise.
    async fn eval_values(
        &self,
        path: &PreparedPath,
        ctx: &EvaluationContext,
    ) -> Result<Vec<FhirPathValue>> {
        Ok(match path {
            PreparedPath::Plain(plan) => self.eval_plan(plan, ctx).await?.iter().cloned().collect(),
            PreparedPath::Node(node) => self.eval_node(node, ctx).await?.iter().cloned().collect(),
            PreparedPath::Boundary(..) => self
                .eval_path(path, ctx)
                .await?
                .into_iter()
                .map(FhirPathValue::resource)
                .collect(),
        })
    }

    /// Evaluate a parsed node in its own variable scope under `ctx`, as
    /// [`Evaluator::eval_plan`] does a plan.
    async fn eval_node(
        &self,
        node: &ExpressionNode,
        ctx: &EvaluationContext,
    ) -> Result<Collection> {
        self.engine
            .evaluate_ast(node, &ctx.nest())
            .await
            .map(|r| r.value)
            .map_err(|e| Error::FhirPath(e.to_string()))
    }

    /// Evaluate a path in a context, as foci for nested evaluation.
    async fn eval_foci(
        &self,
        path: &PreparedPath,
        ctx: &EvaluationContext,
    ) -> Result<Vec<FhirPathValue>> {
        match path {
            PreparedPath::Plain(plan) => Ok(focus_values(&self.eval_plan(plan, ctx).await?)),
            PreparedPath::Node(node) => Ok(focus_values(&self.eval_node(node, ctx).await?)),
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
        root: &EvaluationContext,
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
        root: &EvaluationContext,
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
        root: &EvaluationContext,
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
        root: &EvaluationContext,
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

/// The items of a `forEach` or `repeat` result as foci for nested evaluation,
/// with null and empty items dropped. A node is shared with the root
/// resource, not copied, and keeps the type the engine gave it (a
/// `Patient.contact`, a `HumanName`), so paths under it are typed; a
/// contained resource is typed by its `resourceType`. Primitives keep their
/// value, and nested collections go through JSON.
fn focus_values(coll: &Collection) -> Vec<FhirPathValue> {
    coll.iter()
        .filter_map(|v| match v {
            FhirPathValue::Resource(node, _, _)
                if !node.is_null() && node.get("resourceType").is_some() =>
            {
                Some(FhirPathValue::resource_from_node(node.clone()))
            }
            FhirPathValue::Resource(node, type_info, _) if !node.is_null() => {
                let item = TypeInfo {
                    singleton: Some(true),
                    ..(**type_info).clone()
                };
                Some(FhirPathValue::Resource(node.clone(), Arc::new(item), None))
            }
            // A primitive keeps its type, so `$this.toString()` works on a
            // string or dateTime focus.
            FhirPathValue::Boolean(..)
            | FhirPathValue::Integer(..)
            | FhirPathValue::Decimal(..)
            | FhirPathValue::String(..)
            | FhirPathValue::Date(..)
            | FhirPathValue::DateTime(..)
            | FhirPathValue::Time(..)
            | FhirPathValue::Quantity { .. } => Some(v.clone()),
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
        // The decimal's own digits: the engine writes every decimal through
        // f64, so a decimal 5 (`2 + 3` over decimal elements) became `5.0`.
        FhirPathValue::Decimal(d, ..) => {
            serde_json::from_str(&d.to_string()).unwrap_or_else(|_| v.to_json_value())
        }
        // A time the engine made (`@T14:30`, `toTime()`) is written in its
        // FHIRPath form, `T14:30`; FHIR's has no `T`.
        FhirPathValue::Time(t, ..) => {
            let text = t.to_string();
            Value::String(text.strip_prefix('T').unwrap_or(&text).to_string())
        }
        other => other.to_json_value(),
    }
}

/// `path` with each `%context` (or ``%`context` ``) renamed to
/// [`CONTEXT_VAR`], or `None` if it has none. Run after constant
/// substitution, so a view constant named `context` has already been
/// replaced.
fn rename_context(path: &str) -> Option<String> {
    let toks = tokens(path);
    let is_context = |t: &Token| {
        let text = &path[t.span.clone()];
        t.kind == Kind::Variable && text.starts_with('%') && variable_name(text) == "context"
    };
    if !toks.iter().any(is_context) {
        return None;
    }
    let mut out = String::with_capacity(path.len() + 8);
    for t in &toks {
        if is_context(t) {
            out.push('%');
            out.push_str(CONTEXT_VAR);
        } else {
            out.push_str(&path[t.span.clone()]);
        }
    }
    Some(out)
}

/// FHIRPath environment variables the engine resolves itself, which constant
/// substitution leaves in place. `%factory` is FHIR's type factory.
fn is_environment_variable(name: &str) -> bool {
    matches!(
        name,
        "resource" | "rootResource" | "context" | "ucum" | "sct" | "loinc" | "factory"
    ) || name.starts_with("vs-")
        || name.starts_with("ext-")
}

/// The names a path binds with `defineVariable('name', ...)`, so that its
/// `%name` references are not taken for undefined constants. Empty if the
/// path does not parse; compiling it then reports the error. `None` if a
/// name is computed (`defineVariable(%param, ...)`), when any `%name` may be
/// bound.
fn defined_variables(path: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut computed = false;
    if let Ok(ast) = octofhir_fhirpath::parse_ast(path) {
        visit(&ast, &mut |node| {
            let (name, args) = match node {
                ExpressionNode::MethodCall(m) => (&m.method, &m.arguments),
                ExpressionNode::FunctionCall(f) => (&f.name, &f.arguments),
                _ => return,
            };
            if name != "defineVariable" {
                return;
            }
            match args.first() {
                Some(ExpressionNode::Literal(l)) => {
                    if let octofhir_fhirpath::LiteralValue::String(s) = &l.value {
                        names.push(s.clone());
                    }
                }
                _ => computed = true,
            }
        });
    }
    (!computed).then_some(names)
}

/// Whether the FHIRPath `path` calls a function named in `names`, as a
/// function (`now()`) or a method (`x.now()`), however the call is written
/// (comments, a delimited name). `None` when the path does not parse.
pub fn calls_any(path: &str, names: &[&str]) -> Option<bool> {
    let ast = octofhir_fhirpath::parse_ast(path).ok()?;
    let mut found = false;
    visit(&ast, &mut |node| {
        let name = match node {
            ExpressionNode::MethodCall(m) => &m.method,
            ExpressionNode::FunctionCall(f) => &f.name,
            _ => return,
        };
        found |= names.contains(&name.as_str());
    });
    Some(found)
}

/// Call `f` on `node` and every node below it.
fn visit(node: &ExpressionNode, f: &mut impl FnMut(&ExpressionNode)) {
    f(node);
    match node {
        ExpressionNode::Literal(_)
        | ExpressionNode::Identifier(_)
        | ExpressionNode::Variable(_)
        | ExpressionNode::TypeInfo(_) => {}
        ExpressionNode::FunctionCall(n) => n.arguments.iter().for_each(|a| visit(a, f)),
        ExpressionNode::MethodCall(n) => {
            visit(&n.object, f);
            n.arguments.iter().for_each(|a| visit(a, f));
        }
        ExpressionNode::PropertyAccess(n) => visit(&n.object, f),
        ExpressionNode::IndexAccess(n) => {
            visit(&n.object, f);
            visit(&n.index, f);
        }
        ExpressionNode::BinaryOperation(n) => {
            visit(&n.left, f);
            visit(&n.right, f);
        }
        ExpressionNode::UnaryOperation(n) => visit(&n.operand, f),
        ExpressionNode::Lambda(n) => visit(&n.body, f),
        ExpressionNode::Collection(n) => n.elements.iter().for_each(|e| visit(e, f)),
        ExpressionNode::Parenthesized(e) => visit(e, f),
        ExpressionNode::TypeCast(n) => visit(&n.expression, f),
        ExpressionNode::Filter(n) => {
            visit(&n.base, f);
            visit(&n.condition, f);
        }
        ExpressionNode::Union(n) => {
            visit(&n.left, f);
            visit(&n.right, f);
        }
        ExpressionNode::TypeCheck(n) => visit(&n.expression, f),
        ExpressionNode::Path(n) => visit(&n.base, f),
    }
}

/// Rewrite `element.ofType(T)`, where `T` is a FHIR datatype, to the JSON
/// name a choice element has for that type: `value.ofType(Quantity)` →
/// `valueQuantity`, `value.ofType(string)` → `valueString`. The type name is
/// capitalized as the JSON name requires. The engine cannot map choice
/// elements itself: the model types a choice element by its JSON name
/// (`valueQuantity`), and `value` alone has no type (see `fhir_model`).
///
/// `ofType` on an element that is not a choice element
/// (`name.ofType(HumanName)`, `gender.ofType(code)`) is left to the engine,
/// which knows the element's type from the model. Whether an element is a
/// choice element comes from the FHIR model, for the type or backbone element
/// the path reaches it from ([`step_owner`]): `contact.name` on a Patient is
/// `Patient.contact`'s `name`, not a choice element. `root` is where the
/// path starts: the view's resource type, or a `forEach` focus
/// ([`focus_owner`]). When the path does not show where the element is (it
/// follows a variable or an operator), any choice element of that name
/// counts.
///
/// The rewrite works on tokens, so string literals, backtick-quoted names and
/// comments are left alone. `ofType` with a resource type
/// (`contained.ofType(Practitioner)`), on a base that is not an element name
/// (`first().ofType(string)`), or with a qualified type is left to the engine.
fn rewrite_of_type(path: &str, root: Option<&str>) -> String {
    let toks = tokens(path);
    // The tokens that are not whitespace or comments.
    let sig: Vec<&Token> = toks
        .iter()
        .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment))
        .collect();
    let text = |t: &Token| &path[t.span.clone()];
    let mut out = String::with_capacity(path.len());
    let mut copied = 0;
    let mut k = 0;
    while let Some(&[element, dot, of_type, open, ty, close]) = sig.get(k..k + 6) {
        if element.kind == Kind::Ident
            && text(dot) == "."
            && text(of_type) == "ofType"
            && text(open) == "("
            && ty.kind == Kind::Ident
            && text(close) == ")"
            && is_fhir_datatype(text(ty))
            // On an element that is not a choice element the engine, which
            // knows the element's type from the model, answers it
            // (`name.ofType(HumanName)`, `gender.ofType(code)`).
            && is_choice(path, &sig, k, root)
        {
            let (element, ty_name) = (text(element), text(ty));
            out.push_str(&path[copied..sig[k].span.start]);
            out.push_str(element);
            out.push_str(&capitalize_first(ty_name));
            copied = close.span.end;
            k += 6;
        } else {
            k += 1;
        }
    }
    out.push_str(&path[copied..]);
    out
}

/// Whether the element at significant token `k` is a choice element (see
/// [`rewrite_of_type`]).
fn is_choice(path: &str, sig: &[&Token], k: usize, root: Option<&str>) -> bool {
    let element = &path[sig[k].span.clone()];
    match step_owner(path, sig, k, root) {
        Some(owner) => fhir_model::is_choice_element(&owner, element),
        None => fhir_model::is_choice_anywhere(element),
    }
}

/// The type or backbone element whose element is at significant token `k`:
/// followed through the chain of steps before it (`contact.name`,
/// `name.where(use = 'official').family`, `value.ofType(Range).low`) from
/// `root`, when the chain starts the path, or from a type name the chain
/// starts with (`Patient.contact`). `None` when the chain starts elsewhere
/// (`$this.value`, after an operator) or reaches an element the model has no
/// type for.
fn step_owner(path: &str, sig: &[&Token], k: usize, root: Option<&str>) -> Option<String> {
    let text = |t: &Token| &path[t.span.clone()];
    if k == 0 {
        return root.map(str::to_string);
    }
    if text(sig[k - 1]) != "." || k < 2 {
        return None;
    }
    let start = chain_start(path, sig, k - 2)?;
    walk_chain(
        path,
        &sig[start..k - 1],
        (start == 0).then_some(root).flatten(),
    )
}

/// The type or backbone element that the items `path` selects have
/// (`contact` on a Patient: `Patient.contact`), when the whole path is a
/// chain of steps; else `None`.
fn focus_owner(path: &str, root: Option<&str>) -> Option<String> {
    let toks = tokens(path);
    let sig: Vec<&Token> = toks
        .iter()
        .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment))
        .collect();
    if chain_start(path, &sig, sig.len().checked_sub(1)?)? != 0 {
        return None;
    }
    walk_chain(path, &sig, root)
}

/// Index of the first token of the chain of steps (`a.b(...).c`) that ends
/// at significant token `end`, an identifier or a call's `)`.
fn chain_start(path: &str, sig: &[&Token], mut end: usize) -> Option<usize> {
    let text = |t: &Token| &path[t.span.clone()];
    loop {
        let first = if text(sig[end]) == ")" {
            // Back to the matching `(` and the function name before it.
            let mut depth = 0;
            let open = (0..=end).rev().find(|&i| {
                match text(sig[i]) {
                    ")" => depth += 1,
                    "(" => depth -= 1,
                    _ => {}
                }
                depth == 0
            })?;
            (open > 0 && sig[open - 1].kind == Kind::Ident).then(|| open - 1)?
        } else if sig[end].kind == Kind::Ident {
            end
        } else {
            return None;
        };
        if first >= 2 && text(sig[first - 1]) == "." {
            end = first - 2;
        } else {
            return Some(first);
        }
    }
}

/// The owner reached by the chain of steps `chain`, starting from `root`, or
/// from the type name the chain starts with. Calls that keep the items' type
/// (`where`, `first`, ...) are passed over, and `ofType(T)` gives `T`.
fn walk_chain(path: &str, chain: &[&Token], root: Option<&str>) -> Option<String> {
    let text = |t: &Token| &path[t.span.clone()];
    let first = text(*chain.first()?);
    let is_call = |i: usize| chain.get(i + 1).is_some_and(|t| text(t) == "(");
    let (mut owner, mut i) = if first.starts_with(|c: char| c.is_ascii_uppercase())
        && fhir_model::is_type(first)
        && !is_call(0)
    {
        (first.to_string(), 1)
    } else {
        (root?.to_string(), 0)
    };
    while i < chain.len() {
        let name = text(chain[i]);
        if name == "." {
            i += 1;
        } else if is_call(i) {
            let mut depth = 0;
            let close = (i + 1..chain.len()).find(|&j| {
                match text(chain[j]) {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
                depth == 0
            })?;
            match name {
                "where" | "first" | "last" | "tail" | "skip" | "take" | "distinct" | "single"
                | "trace" => {}
                "ofType" if close == i + 3 && fhir_model::is_type(text(chain[i + 2])) => {
                    owner = text(chain[i + 2]).to_string();
                }
                _ => return None,
            }
            i = close + 1;
        } else {
            // A choice element by its own name (`value`) has no type until
            // an `ofType(T)` after it gives one.
            let of_type_next = chain.get(i + 1).is_some_and(|t| text(t) == ".")
                && chain.get(i + 2).is_some_and(|t| text(t) == "ofType");
            owner = match fhir_model::element_owner(&owner, name) {
                Some(next) => next.to_string(),
                None if of_type_next => String::new(),
                None => return None,
            };
            i += 1;
        }
    }
    (!owner.is_empty()).then_some(owner)
}

/// Whether `name` is a FHIR datatype a choice element can take (R4 and R5),
/// or a FHIRPath system type name, as opposed to a resource type.
fn is_fhir_datatype(name: &str) -> bool {
    matches!(
        name,
        // Primitive types.
        "base64Binary" | "boolean" | "canonical" | "code" | "date" | "dateTime" | "decimal"
            | "id" | "instant" | "integer" | "integer64" | "markdown" | "oid" | "positiveInt"
            | "string" | "time" | "unsignedInt" | "uri" | "url" | "uuid"
            // General-purpose and metadata types.
            | "Address" | "Age" | "Annotation" | "Attachment" | "Availability"
            | "CodeableConcept" | "CodeableReference" | "Coding" | "ContactDetail"
            | "ContactPoint" | "Contributor" | "Count" | "DataRequirement" | "Distance"
            | "Dosage" | "Duration" | "Expression" | "ExtendedContactDetail" | "HumanName"
            | "Identifier" | "Meta" | "MonetaryComponent" | "Money" | "ParameterDefinition"
            | "Period" | "Quantity" | "Range" | "Ratio" | "RatioRange" | "Reference"
            | "RelatedArtifact" | "SampledData" | "Signature" | "Timing"
            | "TriggerDefinition" | "UsageContext" | "VirtualServiceDetail"
            // FHIRPath system types.
            | "Boolean" | "String" | "Integer" | "Decimal" | "Date" | "DateTime" | "Time"
    )
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryType {
    Date,
    DateTime,
    Time,
    Unknown,
}

/// If `node` is a `lowBoundary()`/`highBoundary()` call, the path that
/// evaluates its base (a node, or another boundary call) and applies the
/// boundary. `written` is the same call in the path as written, before the
/// `ofType` rewrite, from which the temporal hint is read.
fn boundary_path(node: &ExpressionNode, written: Option<&ExpressionNode>) -> Option<PreparedPath> {
    let ExpressionNode::MethodCall(m) = node else {
        return None;
    };
    let low = match m.method.as_str() {
        "lowBoundary" => true,
        "highBoundary" => false,
        _ => return None,
    };
    let written = match written {
        Some(ExpressionNode::MethodCall(w)) if w.method == m.method => Some(w.object.as_ref()),
        _ => None,
    };
    let hint = written.map_or(BoundaryType::Unknown, boundary_hint);
    let base = boundary_path(&m.object, written)
        .unwrap_or_else(|| PreparedPath::Node(m.object.as_ref().clone()));
    let precision = m.arguments.first().cloned();
    Some(PreparedPath::Boundary(Box::new(base), low, hint, precision))
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

impl BoundaryType {
    /// Precision digits of the type's finest form: day, millisecond,
    /// millisecond. `None` for numbers, whose default the engine applies.
    fn full_precision(self) -> Option<i64> {
        match self {
            BoundaryType::Date => Some(8),
            BoundaryType::DateTime => Some(17),
            BoundaryType::Time => Some(9),
            BoundaryType::Unknown => None,
        }
    }
}

/// What a boundary is computed on: a value the engine has typed, or a
/// literal written for a value it has not.
enum BoundaryInput {
    Typed(BoundaryType),
    Literal(String, BoundaryType),
}

impl BoundaryInput {
    fn kind(&self) -> BoundaryType {
        match self {
            BoundaryInput::Typed(k) | BoundaryInput::Literal(_, k) => *k,
        }
    }
}

/// How to compute the boundary of `value`, or `None` if it has none.
///
/// Dates, times, dateTimes, integers and Quantities the engine has typed are
/// used as they are. A decimal is written as a literal when its JSON form
/// shows more digits after the point than the engine kept (a resource's
/// `1.0` reaches the engine as `1`, and the boundary depends on the digits
/// written). Strings, which is how the engine holds a resource's date, time
/// or dateTime, become a literal of the type `hint` names.
fn boundary_input(value: &FhirPathValue, hint: BoundaryType) -> Option<BoundaryInput> {
    let typed = |k| Some(BoundaryInput::Typed(k));
    match value {
        FhirPathValue::Integer(..) | FhirPathValue::Quantity { .. } => typed(BoundaryType::Unknown),
        FhirPathValue::Date(..) => typed(BoundaryType::Date),
        FhirPathValue::DateTime(..) => typed(BoundaryType::DateTime),
        FhirPathValue::Time(..) => typed(BoundaryType::Time),
        FhirPathValue::Decimal(d, ..) => {
            let engine = d.to_string();
            let text = decimal_text(&engine, &value.to_json_value());
            if text == engine {
                typed(BoundaryType::Unknown)
            } else {
                Some(BoundaryInput::Literal(signed(text), BoundaryType::Unknown))
            }
        }
        _ => match value.to_json_value() {
            Value::Number(n) => Some(BoundaryInput::Literal(
                signed(n.to_string()),
                BoundaryType::Unknown,
            )),
            Value::String(s) => {
                temporal_literal(&s, hint).map(|(l, k)| BoundaryInput::Literal(l, k))
            }
            _ => None,
        },
    }
}

/// The digits of a decimal: the engine's, or the JSON number's when it has
/// more after the point (a resource's `1.0` reaches the engine as `1`, and
/// the boundary depends on the digits written).
fn decimal_text(engine: &str, json: &Value) -> String {
    let fraction = |s: &str| s.split_once('.').map_or(0, |(_, f)| f.len());
    match json.as_number().map(|n| n.to_string()) {
        Some(j) if fraction(&j) > fraction(engine) => j,
        _ => engine.to_string(),
    }
}

/// A number literal, in parentheses when negative so the method binds to it.
fn signed(number: String) -> String {
    if number.starts_with('-') {
        format!("({number})")
    } else {
        number
    }
}

/// A date, dateTime or time string from a resource as a FHIRPath literal of
/// the type `hint` names, or that its form shows when `hint` is unknown.
/// `None` unless the string is well formed.
fn temporal_literal(s: &str, hint: BoundaryType) -> Option<(String, BoundaryType)> {
    let digits = |t: &str, n: usize| t.len() == n && t.bytes().all(|b| b.is_ascii_digit());
    let is_time = |t: &str| {
        let (hms, frac) = t.split_once('.').unwrap_or((t, "0"));
        let parts: Vec<&str> = hms.split(':').collect();
        !parts.is_empty()
            && parts.len() <= 3
            && parts.iter().all(|p| digits(p, 2))
            && frac.bytes().all(|b| b.is_ascii_digit())
    };
    let is_date = |t: &str| {
        let parts: Vec<&str> = t.split('-').collect();
        digits(parts[0], 4) && parts.len() <= 3 && parts[1..].iter().all(|p| digits(p, 2))
    };
    let is_datetime = |t: &str| match t.split_once('T') {
        None => is_date(t),
        Some((date, time)) => {
            let time = time.trim_end_matches('Z');
            let time = match time.rfind(['+', '-']) {
                Some(i) => {
                    let (clock, offset) = time.split_at(i);
                    let offset = &offset[1..];
                    if !(offset.len() == 5 && is_time(offset)) {
                        return false;
                    }
                    clock
                }
                None => time,
            };
            is_date(date) && date.len() == 10 && is_time(time)
        }
    };
    let ty = match hint {
        BoundaryType::Unknown if is_time(s) => BoundaryType::Time,
        BoundaryType::Unknown if s.contains('T') => BoundaryType::DateTime,
        BoundaryType::Unknown => BoundaryType::Date,
        known => known,
    };
    match ty {
        BoundaryType::Time if is_time(s) => Some((format!("@T{s}"), ty)),
        BoundaryType::Date if is_date(s) => Some((format!("@{s}"), ty)),
        BoundaryType::DateTime if is_datetime(s) => {
            let t = if s.contains('T') { "" } else { "T" };
            Some((format!("@{s}{t}"), ty))
        }
        _ => None,
    }
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
    fn boundaries_follow_the_precision_argument() {
        let obs = json!({
            "resourceType": "Observation", "id": "o1",
            "valueQuantity": { "value": 1.587 },
            "effectiveDateTime": "2010-10-10",
            "issued": "not a date"
        });
        let column = |path: &str| {
            let v = view(json!({
                "resource": "Observation",
                "select": [{ "column": [{ "name": "b", "path": path, "collection": true }] }]
            }));
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(execute(&v, std::slice::from_ref(&obs)))
                .unwrap()
                .data[0][0]
                .clone()
        };
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(2)"),
            json!([1.58])
        );
        assert_eq!(
            column("value.ofType(Quantity).value.highBoundary(2)"),
            json!([1.59])
        );
        // A precision out of range has no boundary.
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(-1)"),
            json!([])
        );
        assert_eq!(
            column("value.ofType(Quantity).value.lowBoundary(32)"),
            json!([])
        );
        // A resource's dateTime string, read as the type the path names.
        assert_eq!(
            column("effective.ofType(dateTime).lowBoundary()"),
            json!(["2010-10-10T00:00:00.000+14:00"])
        );
        assert_eq!(
            column("effective.ofType(dateTime).highBoundary(6)"),
            json!(["2010-10"])
        );
        // Text that is not a date has no boundary, and never reaches the
        // expression the engine evaluates.
        assert_eq!(column("issued.lowBoundary()"), json!([]));
        assert_eq!(column("@T10:30.lowBoundary(9)"), json!(["10:30:00.000"]));
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
    fn of_type_is_rewritten_only_for_datatypes_on_element_names() {
        for (path, want) in [
            ("value.ofType(Quantity).value", "valueQuantity.value"),
            ("a.value . ofType ( dateTime )", "a.valueDateTime"),
            (
                "where(value.ofType(integer) > 1)",
                "where(valueInteger > 1)",
            ),
            // Left to the engine.
            (
                "contained.ofType(Practitioner)",
                "contained.ofType(Practitioner)",
            ),
            ("first().ofType(string)", "first().ofType(string)"),
            ("$this.ofType(string)", "$this.ofType(string)"),
            ("value.ofType(FHIR.string)", "value.ofType(FHIR.string)"),
            ("'a.ofType(string)'", "'a.ofType(string)'"),
            ("id // value.ofType(string)", "id // value.ofType(string)"),
        ] {
            assert_eq!(rewrite_of_type(path, None), want, "{path}");
        }
    }

    #[test]
    fn of_type_is_rewritten_only_on_choice_elements() {
        for (path, root, want) in [
            // The parent is named in the path, or is where it starts.
            (
                "Observation.value.ofType(Quantity)",
                None,
                "Observation.valueQuantity",
            ),
            (
                "value.ofType(Quantity)",
                Some("Observation"),
                "valueQuantity",
            ),
            (
                "Patient.name.ofType(HumanName)",
                None,
                "Patient.name.ofType(HumanName)",
            ),
            (
                "name.ofType(HumanName)",
                Some("Patient"),
                "name.ofType(HumanName)",
            ),
            // Deeper, the model follows the path to the element's parent.
            (
                "component.value.ofType(Quantity)",
                Some("Observation"),
                "component.valueQuantity",
            ),
            (
                "contact.name.ofType(HumanName)",
                Some("Patient"),
                "contact.name.ofType(HumanName)",
            ),
            (
                "Patient.contact.name.ofType(HumanName)",
                None,
                "Patient.contact.name.ofType(HumanName)",
            ),
            (
                "name.where(use = 'official').family.ofType(string)",
                Some("Patient"),
                "name.where(use = 'official').family.ofType(string)",
            ),
            (
                "value.ofType(Range).low.ofType(Quantity)",
                Some("Observation"),
                "valueRange.low.ofType(Quantity)",
            ),
            // A forEach focus (`component` on an Observation).
            (
                "value.ofType(Quantity)",
                Some("Observation.component"),
                "valueQuantity",
            ),
            // Where the path does not show the parent, any choice element of
            // that name counts: `name` is one in
            // ImplementationGuide.definition.page.
            (
                "component.value.ofType(Quantity)",
                None,
                "component.valueQuantity",
            ),
            (
                "contact.name.ofType(HumanName)",
                None,
                "contact.nameHumanName",
            ),
            (
                "$this.value.ofType(Quantity)",
                Some("Observation"),
                "$this.valueQuantity",
            ),
            (
                "contact.telecom.ofType(ContactPoint)",
                None,
                "contact.telecom.ofType(ContactPoint)",
            ),
            // So is a primitive type: the engine answers from the model.
            (
                "Observation.code.ofType(code)",
                None,
                "Observation.code.ofType(code)",
            ),
            (
                "Patient.gender.ofType(code)",
                None,
                "Patient.gender.ofType(code)",
            ),
        ] {
            assert_eq!(rewrite_of_type(path, root), want, "{path}");
        }
    }

    #[test]
    fn a_for_each_focus_has_its_owner() {
        assert_eq!(
            focus_owner("contact", Some("Patient")).as_deref(),
            Some("Patient.contact")
        );
        assert_eq!(
            focus_owner("name", Some("Patient")).as_deref(),
            Some("HumanName")
        );
        assert_eq!(
            focus_owner("name.where(use = 'official')", Some("Patient")).as_deref(),
            Some("HumanName")
        );
        assert_eq!(
            focus_owner("value.ofType(Quantity)", Some("Observation")).as_deref(),
            Some("Quantity")
        );
        assert_eq!(
            focus_owner("item", Some("Questionnaire.item")).as_deref(),
            Some("Questionnaire.item")
        );
        // Not a chain of steps, a primitive, or nothing to start from.
        assert_eq!(focus_owner("name | contact.name", Some("Patient")), None);
        assert_eq!(focus_owner("name.given", Some("Patient")), None);
        assert_eq!(focus_owner("contact", None), None);
        assert_eq!(
            focus_owner("Patient.contact", None).as_deref(),
            Some("Patient.contact")
        );
    }

    #[test]
    fn context_is_renamed_outside_literals() {
        assert_eq!(
            rename_context("%context.a | %`context`.b | '%context' | %contextual").as_deref(),
            Some("%_sofContext.a | %_sofContext.b | '%context' | %contextual")
        );
        assert_eq!(rename_context("%resource.id // %context"), None);
    }

    #[test]
    fn repeat_keeps_the_resource_as_root() {
        // repeat foci are children of the resource's context, like forEach
        // foci: %resource is the resource and a `#` reference resolves
        // against its contained resources.
        let resource = json!({
            "resourceType": "QuestionnaireResponse",
            "id": "q1",
            "contained": [{ "resourceType": "Practitioner", "id": "c1" }],
            "item": [{
                "linkId": "1",
                "answer": [{ "valueReference": { "reference": "#c1" } }],
                "item": [{ "linkId": "1.1" }]
            }]
        });
        let v = view(json!({
            "resource": "QuestionnaireResponse",
            "select": [{ "repeat": ["item"], "column": [
                { "name": "link", "path": "linkId" },
                { "name": "res", "path": "%resource.id" },
                { "name": "who", "path": "answer.value.ofType(Reference).getReferenceKey(Practitioner)" }
            ] }]
        }));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute(&v, &[resource])).unwrap();
        assert_eq!(
            result.data,
            vec![
                vec![json!("1"), json!("q1"), json!("c1")],
                vec![json!("1.1"), json!("q1"), Value::Null],
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

    #[test]
    fn define_variable_stays_in_its_own_path() {
        // defineVariable() writes into the context it runs in; one path's
        // variable must not make the same definition in another path fail.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let run = |v: Value| rt.block_on(execute(&view(v), &[patient(1, 1)]));
        let define = json!({ "name": "b", "path": "id.defineVariable('x')" });
        let cases = [
            // Sibling top-level selects.
            json!({
                "resource": "Patient",
                "select": [
                    { "column": [{ "name": "a", "path": "id.defineVariable('x')" }] },
                    { "column": [define] }
                ]
            }),
            // A where filter and a column.
            json!({
                "resource": "Patient",
                "where": [{ "path": "true.defineVariable('x')" }],
                "select": [
                    { "column": [{ "name": "a", "path": "id" }, define] }
                ]
            }),
            // Two columns of one select, and a nested select on the same focus.
            json!({
                "resource": "Patient",
                "select": [{
                    "column": [{ "name": "a", "path": "id.defineVariable('x')" }],
                    "select": [{ "column": [define] }]
                }]
            }),
            json!({
                "resource": "Patient",
                "select": [{ "column": [
                    { "name": "a", "path": "id.defineVariable('x')" },
                    define
                ] }]
            }),
        ];
        for case in cases {
            let result = run(case.clone()).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(result.data, vec![vec![json!("p1"), json!("p1")]], "{case}");
        }
    }
}
