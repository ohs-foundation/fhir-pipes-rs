//! A view's paths compiled for the engine, and their evaluation against one
//! resource's tree: `where`, columns, `forEach`/`forEachOrNull`, `repeat`,
//! nested `select` and `unionAll`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use octofhir_fhirpath::core::model_provider::TypeInfo;
use octofhir_fhirpath::evaluator::plan::CompiledPlan;
use octofhir_fhirpath::{
    Collection, EvaluationContext, ExpressionNode, FhirPathEngine, FhirPathValue, ModelProvider,
};
use serde_json::Value;

use crate::boundary_type::BoundaryType;
use crate::sql_generator::substitute_constants_keeping;
use crate::view_definition::SelectColumn;
use crate::{Error, Result};

use super::boundary::boundary_path;
use super::cartesian;
use super::rewrite::{
    defined_variables, focus_owner, is_environment_variable, rename_context, rewrite_of_type,
};

/// Deeper than any resource's JSON can be (serde_json stops at 128).
const MAX_REPEAT_DEPTH: usize = 128;

pub(super) struct Evaluator {
    pub(super) resource_type: String,
    pub(super) constants: std::collections::HashMap<String, String>,
    pub(super) engine: FhirPathEngine,
    pub(super) model: Arc<dyn ModelProvider + Send + Sync>,
    /// Whether any path uses `%context`, so contexts bind [`CONTEXT_VAR`].
    /// Set while the view compiles.
    pub(super) uses_context: AtomicBool,
}

/// The variable `%context` is renamed to ([`rename_context`]). Each context
/// binds it to its own focus: the engine's own `%context` is an alias of the
/// root, which child contexts share, while FHIRPath defines `%context` as the
/// node the expression started from.
pub(super) const CONTEXT_VAR: &str = "_sofContext";

/// A view path compiled for the engine: either a plain expression, or a
/// `lowBoundary()`/`highBoundary()` call whose base is evaluated first and
/// each value's boundary then computed by the engine (see
/// [`Evaluator::eval_boundary`]).
pub(super) enum PreparedPath {
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
pub(super) struct CompiledSelect {
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
pub(super) struct Focus<'a> {
    pub(super) value: &'a FhirPathValue,
    pub(super) rid: i64,
    pub(super) ctx: OnceLock<EvaluationContext>,
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
    pub(super) fn with_context(value: &'a FhirPathValue, ctx: EvaluationContext) -> Self {
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
    pub(super) fn prepare(&self, path: &str, root: Option<&str>) -> Result<String> {
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
    pub(super) fn plan(&self, prepared: &str) -> Result<Arc<CompiledPlan>> {
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
    pub(super) fn compile_select(
        &self,
        select: &SelectColumn,
        root: Option<&str>,
    ) -> Result<CompiledSelect> {
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
            .map(|p| {
                let ast =
                    octofhir_fhirpath::parse_ast(p).map_err(|e| Error::FhirPath(e.to_string()))?;
                crate::repeat_path::check(p, &ast, &self.resource_type)?;
                self.plan(&self.prepare(p, None)?)
            })
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
    pub(super) fn root_context(&self, root: &FhirPathValue) -> EvaluationContext {
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
    pub(super) fn ctx<'f>(
        &self,
        root: &EvaluationContext,
        focus: &'f Focus<'_>,
    ) -> &'f EvaluationContext {
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

    /// The values of a path as the engine returns them, typed where it types
    /// them (literals, computed values) and as resource JSON otherwise.
    pub(super) async fn eval_values(
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
    pub(super) async fn eval_node(
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
    pub(super) async fn eval_select(
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
        // Each node is paired with its depth. The paths only go down the
        // resource (`repeat_path::check`), so the depth is the JSON's at most;
        // the limit is there in case a path still finds its way back.
        let mut stack: Vec<(FhirPathValue, usize)> = Vec::new();
        let seed = self.repeat_children(plans, root, node).await?;
        stack.extend(seed.into_iter().rev().map(|v| (v, 1)));
        while let Some((current, depth)) = stack.pop() {
            if depth > MAX_REPEAT_DEPTH {
                return Err(Error::InvalidPath(format!(
                    "repeat went more than {MAX_REPEAT_DEPTH} levels deep; its path keeps \
                     finding nodes it has already been through"
                )));
            }
            let children = self.repeat_children(plans, root, &current).await?;
            out.push(current);
            stack.extend(children.into_iter().rev().map(|v| (v, depth + 1)));
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
    pub(super) async fn eval_where(
        &self,
        plan: &CompiledPlan,
        ctx: &EvaluationContext,
    ) -> Result<bool> {
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
    for col in select.output_columns(|| {}) {
        row.push(if col.path.trim() == "%rowIndex" {
            Value::Number(0.into())
        } else {
            Value::Null
        });
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
pub(super) fn collection_to_json(coll: &Collection) -> Vec<Value> {
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
