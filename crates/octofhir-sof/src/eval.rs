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

use std::sync::Arc;

use async_trait::async_trait;
use octofhir_fhirpath::core::error_code::FP0053;
use octofhir_fhirpath::evaluator::AsyncNodeEvaluator;
use octofhir_fhirpath::evaluator::function_registry::{
    EmptyPropagation, FunctionCategory, FunctionMetadata, FunctionSignature, LazyFunctionEvaluator,
    PureFunctionEvaluator,
};
use octofhir_fhirpath::{
    Collection, EmptyModelProvider, EvaluationContext, ExpressionNode, FhirPathEngine,
    FhirPathError, FhirPathValue, ModelProvider, create_function_registry,
};
use serde_json::{Map, Value};

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

/// A ViewDefinition compiled once for repeated, resource-at-a-time execution.
///
/// Validates the view (columns, name collisions, constants) up front and builds
/// the FHIRPath engine once, then lets callers stream resources through
/// [`CompiledView::execute_resource`] without holding the whole dataset in
/// memory.
pub struct CompiledView {
    ev: Evaluator,
    selects: Vec<SelectColumn>,
    where_paths: Vec<String>,
    shape: Vec<(String, ColumnType)>,
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
            prepared: Default::default(),
        };

        let shape = ev.shape(&view.select)?;
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

        // Validate/normalize every where path up front (constants + ofType
        // rewrite), so per-resource evaluation just hands strings to the engine.
        let where_paths = view
            .where_
            .iter()
            .map(|w| ev.prepare(&w.path))
            .collect::<Result<Vec<_>>>()?;

        let columns = shape
            .iter()
            .map(|(name, ty)| ColumnInfo::new(name.clone(), *ty))
            .collect();

        Ok(Self {
            ev,
            selects: view.select.clone(),
            where_paths,
            shape,
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
        for path in &self.where_paths {
            if !self.ev.eval_where(path, resource).await? {
                return Ok(Vec::new());
            }
        }

        let mut combos = vec![Map::new()];
        for select in &self.selects {
            let srows = self.ev.eval_select(select, resource, 0).await?;
            combos = cartesian(&combos, &srows);
        }

        Ok(combos
            .iter()
            .map(|row| {
                self.shape
                    .iter()
                    .map(|(name, _)| row.get(name).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect())
    }
}

/// The cartesian product of two row sets, merging each pair of maps.
fn cartesian(a: &[Map<String, Value>], b: &[Map<String, Value>]) -> Vec<Map<String, Value>> {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for x in a {
        for y in b {
            let mut merged = x.clone();
            for (k, v) in y {
                merged.insert(k.clone(), v.clone());
            }
            out.push(merged);
        }
    }
    out
}

struct Evaluator {
    resource_type: String,
    constants: std::collections::HashMap<String, String>,
    engine: FhirPathEngine,
    model: Arc<dyn ModelProvider + Send + Sync>,
    /// Column and forEach paths as the engine sees them, keyed on the path in
    /// the view. A view has a fixed set of paths but each is evaluated once per
    /// resource, so preparing (and parsing for a boundary call) once saves a
    /// parse per path per resource.
    prepared: std::sync::RwLock<std::collections::HashMap<String, Arc<PreparedPath>>>,
}

/// A view path ready for evaluation: either a plain prepared expression, or a
/// `lowBoundary()`/`highBoundary()` call whose base (still unprepared) is
/// evaluated first and the SoF boundary rules applied to each value.
enum PreparedPath {
    Plain(String),
    Boundary(String, bool, BoundaryType),
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

    /// Evaluate a (prepared) FHIRPath expression against `focus`, with
    /// `%rowIndex` bound to `rid`, returning the result as a FHIRPath collection
    /// of JSON values.
    async fn eval_prepared(&self, path: &str, focus: &Value, rid: i64) -> Result<Vec<Value>> {
        self.eval_in(path, &self.context(focus, rid)).await
    }

    /// An evaluation context with `focus` as its input and `%rowIndex` bound to
    /// `rid`. Building one copies the focus and sets up the engine's variables,
    /// so callers evaluating several paths against the same focus build it once.
    fn context(&self, focus: &Value, rid: i64) -> EvaluationContext {
        let ctx = EvaluationContext::new(
            Collection::from(vec![FhirPathValue::resource(focus.clone())]),
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

    /// Evaluate a (prepared) FHIRPath expression in an existing context.
    async fn eval_in(&self, path: &str, ctx: &EvaluationContext) -> Result<Vec<Value>> {
        let result = self
            .engine
            .evaluate(path, ctx)
            .await
            .map_err(|e| Error::FhirPath(e.to_string()))?;

        Ok(collection_to_json(&result.value))
    }

    /// Prepare and evaluate a path in one step.
    ///
    /// `lowBoundary()`/`highBoundary()` diverge from the engine's stock
    /// behaviour, so they are intercepted here: the base is evaluated by the
    /// engine and the SoF precision-boundary rules are applied to each value.
    /// The temporal flavour (`date`/`dateTime`/`time`) is taken from a leading
    /// `ofType(T)` on the base before the `ofType` rewrite collapses it.
    async fn eval_path(&self, path: &str, ctx: &EvaluationContext) -> Result<Vec<Value>> {
        match &*self.prepared_path(path)? {
            PreparedPath::Boundary(base, low, hint) => {
                let base_vals = Box::pin(self.eval_path(base, ctx)).await?;
                Ok(base_vals
                    .iter()
                    .map(|v| boundary(v, *low, *hint))
                    .filter(|v| !v.is_null())
                    .collect())
            }
            PreparedPath::Plain(prepared) => self.eval_in(prepared, ctx).await,
        }
    }

    /// The prepared form of a view path, computed on first use and cached.
    fn prepared_path(&self, path: &str) -> Result<Arc<PreparedPath>> {
        if let Some(p) = self.prepared.read().expect("prepared cache").get(path) {
            return Ok(p.clone());
        }
        let p = Arc::new(match boundary_call(path) {
            Some((base, low, hint)) => PreparedPath::Boundary(base, low, hint),
            None => PreparedPath::Plain(self.prepare(path)?),
        });
        self.prepared
            .write()
            .expect("prepared cache")
            .insert(path.to_string(), p.clone());
        Ok(p)
    }

    /// The ordered column shape of a select list, validating `unionAll` branch
    /// consistency.
    fn shape(&self, selects: &[SelectColumn]) -> Result<Vec<(String, ColumnType)>> {
        let mut cols = Vec::new();
        for select in selects {
            cols.extend(self.shape_of(select)?);
        }
        Ok(cols)
    }

    fn shape_of(&self, select: &SelectColumn) -> Result<Vec<(String, ColumnType)>> {
        let mut cols = Vec::new();
        if let Some(columns) = &select.column {
            for col in columns {
                cols.push((col.name.clone(), column_type(col)));
            }
        }
        for nested in &select.select {
            cols.extend(self.shape_of(nested)?);
        }
        if let Some(branches) = &select.union_all {
            let mut shapes = branches
                .iter()
                .map(|b| self.shape_of(b))
                .collect::<Result<Vec<_>>>()?;
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

    /// Evaluate a select node against a focus value, yielding partial rows.
    async fn eval_select(
        &self,
        select: &SelectColumn,
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Map<String, Value>>> {
        let for_each = select.for_each.as_deref();
        let for_each_or_null = select.for_each_or_null.as_deref();

        // `repeat` recursively traverses the focus, collecting every node reached
        // by transitively re-applying the repeat path(s) (preorder, the focus
        // node itself excluded). %rowIndex tracks position in the flattened list.
        if !select.repeat.is_empty() {
            let paths = select
                .repeat
                .iter()
                .map(|p| self.prepare(p))
                .collect::<Result<Vec<_>>>()?;
            let mut foci = Vec::new();
            self.repeat_collect(&paths, ctx, &mut foci).await?;
            let mut rows = Vec::new();
            for (idx, focus) in foci.iter().enumerate() {
                rows.extend(Box::pin(self.eval_level(select, focus, idx as i64)).await?);
            }
            return Ok(rows);
        }

        if let Some(path) = for_each.or(for_each_or_null) {
            let elements = self.eval_path(path, &self.context(ctx, rid)).await?;
            if elements.is_empty() {
                if for_each_or_null.is_some() {
                    return Ok(vec![self.null_row(select)]);
                }
                return Ok(Vec::new());
            }
            let mut rows = Vec::new();
            for (idx, elem) in elements.iter().enumerate() {
                rows.extend(Box::pin(self.eval_level(select, elem, idx as i64)).await?);
            }
            Ok(rows)
        } else {
            self.eval_level(select, ctx, rid).await
        }
    }

    /// Evaluate this select's columns, nested selects and unionAll against an
    /// already-focused value (after any forEach has selected the element).
    async fn eval_level(
        &self,
        select: &SelectColumn,
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Map<String, Value>>> {
        let mut own = Map::new();
        if let Some(columns) = &select.column {
            let ectx = self.context(ctx, rid);
            for col in columns {
                own.insert(col.name.clone(), self.column_value(col, &ectx).await?);
            }
        }
        let mut combos = vec![own];

        for nested in &select.select {
            let nrows = Box::pin(self.eval_select(nested, ctx, rid)).await?;
            combos = cartesian(&combos, &nrows);
        }

        if let Some(branches) = &select.union_all {
            let mut branch_rows = Vec::new();
            for branch in branches {
                branch_rows.extend(Box::pin(self.eval_select(branch, ctx, rid)).await?);
            }
            combos = cartesian(&combos, &branch_rows);
        }

        Ok(combos)
    }

    /// Preorder transitive closure of the `repeat` path(s): apply every path to
    /// `node`, push each result, then recurse into it. The starting node itself
    /// is not emitted.
    async fn repeat_collect(
        &self,
        paths: &[String],
        node: &Value,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        // Iterative worklist (preorder) to avoid recursive async lifetimes.
        let mut stack: Vec<Value> = Vec::new();
        // Seed in reverse so that, popped LIFO, children come out in source order.
        let mut seed = Vec::new();
        for path in paths {
            seed.extend(self.eval_prepared(path, node, 0).await?);
        }
        for child in seed.into_iter().rev() {
            stack.push(child);
        }
        while let Some(current) = stack.pop() {
            out.push(current.clone());
            let mut children = Vec::new();
            for path in paths {
                children.extend(self.eval_prepared(path, &current, 0).await?);
            }
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
        Ok(())
    }

    /// An all-null row for an empty `forEachOrNull`. Per spec, columns whose path
    /// is `%rowIndex` are bound to 0 rather than null.
    fn null_row(&self, select: &SelectColumn) -> Map<String, Value> {
        let mut row = Map::new();
        self.null_fill(select, &mut row);
        row
    }

    fn null_fill(&self, select: &SelectColumn, row: &mut Map<String, Value>) {
        if let Some(columns) = &select.column {
            for col in columns {
                let v = if col.path.trim() == "%rowIndex" {
                    Value::Number(0.into())
                } else {
                    Value::Null
                };
                row.insert(col.name.clone(), v);
            }
        }
        for nested in &select.select {
            self.null_fill(nested, row);
        }
        if let Some(first) = select.union_all.as_ref().and_then(|b| b.first()) {
            self.null_fill(first, row);
        }
    }

    async fn column_value(&self, col: &Column, ctx: &EvaluationContext) -> Result<Value> {
        let values = self.eval_path(&col.path, ctx).await?;
        if col.collection.unwrap_or(false) {
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
    async fn eval_where(&self, path: &str, resource: &Value) -> Result<bool> {
        let coll = self.eval_prepared(path, resource, 0).await?;
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

/// The resource type carried by the contained resource with the given local id,
/// if the root resource lists it under `contained[]`.
fn contained_type(root: &Value, local_id: &str) -> Option<String> {
    root.get("contained")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(local_id))
        .and_then(|c| c.get("resourceType").and_then(Value::as_str))
        .map(str::to_string)
}

/// `getReferenceKey()` on a single Reference value. Relative/absolute `Type/id`
/// references key on the id (filtered by the optional expected type). Fragment
/// `#id` references resolve into the root resource's `contained[]` and key on
/// the local id, so the value matches `getResourceKey()` on that contained
/// resource. Absolute URL and `urn:` references are not keyed.
fn reference_key(item: &Value, root: &Value, want_type: Option<&str>) -> Option<String> {
    let reference = item.get("reference")?.as_str()?;
    if let Some(local) = reference.strip_prefix('#') {
        if local.is_empty() {
            return None;
        }
        let actual = contained_type(root, local)
            .or_else(|| item.get("type").and_then(Value::as_str).map(str::to_string));
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

/// The root resource JSON from the evaluation context, falling back to the
/// function input if no root is set.
fn root_json(context: &EvaluationContext, input: &Collection) -> Value {
    if let Some(root) = context.root_resource_value() {
        root.to_json_value()
    } else if let Some(first) = input.first() {
        first.to_json_value()
    } else {
        Value::Null
    }
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
                v.to_json_value()
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| FhirPathValue::string(id.to_string()))
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
        let root = root_json(context, &input);
        let values: Vec<FhirPathValue> = input
            .iter()
            .filter_map(|v| reference_key(&v.to_json_value(), &root, want_type.as_deref()))
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
