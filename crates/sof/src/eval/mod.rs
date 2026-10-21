//! In-memory ViewDefinition execution.
//!
//! Evaluates a [`ViewDefinition`] directly against FHIR resources held as
//! `serde_json::Value`, with no database. Per-expression FHIRPath evaluation is
//! delegated to the real `octofhir-fhirpath` [`FhirPathEngine`], so the full
//! FHIRPath function set (`substring`, `upper`, math, etc.) is available —
//! unlike the hand-lowered subset used by the SQL generator.
//!
//! What this module adds is the SQL-on-FHIR *relational orchestration* that
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
//!
//! `evaluator` compiles and evaluates the paths, `rewrite` prepares them for
//! the engine, `functions` and `boundary` implement the functions whose
//! engine versions differ from SQL-on-FHIR (or that it lacks), and
//! `parallel` runs a view for non-async callers.

mod boundary;
mod evaluator;
mod functions;
mod parallel;
mod rewrite;

pub use parallel::{execute_blocking, execute_parallel};
pub use rewrite::calls_any;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octofhir_fhirpath::core::node::FhirNode;
use octofhir_fhirpath::evaluator::plan::CompiledPlan;
use octofhir_fhirpath::{FhirPathEngine, FhirPathValue, ModelProvider, create_function_registry};
use serde_json::Value;

use crate::column::{ColumnInfo, ColumnType};
use crate::fhir_model::TableModelProvider;
use crate::runner::ViewResult;
use crate::sql_generator::build_constants;
use crate::view_definition::{Column, ViewDefinition};
use crate::{Error, Result};

use evaluator::{CompiledSelect, Evaluator, Focus};
use functions::{GetReferenceKey, GetResourceKey, JoinFn};

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

/// A column's type: JSON for a collection, else its `ansi/type` tag, else
/// the FHIR type it declares, else a string. The evaluator and the SQL
/// generator both type columns with this.
pub(crate) fn column_type(col: &Column) -> ColumnType {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn view(v: Value) -> ViewDefinition {
        ViewDefinition::from_json(&v).expect("parse ViewDefinition")
    }

    pub(super) fn patient(i: usize, names: usize) -> Value {
        let name: Vec<Value> = (0..names)
            .map(|n| json!({ "family": format!("f{i}-{n}") }))
            .collect();
        json!({ "resourceType": "Patient", "id": format!("p{i}"), "name": name })
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
