//! The functions registered on the engine: the SQL-on-FHIR
//! `getResourceKey()` and `getReferenceKey()`, and `join()` as the
//! conformance suite expects it.

use std::borrow::Cow;
use std::cell::OnceCell;

use async_trait::async_trait;
use octofhir_fhirpath::core::error_code::FP0053;
use octofhir_fhirpath::core::node::FhirNode;
use octofhir_fhirpath::evaluator::AsyncNodeEvaluator;
use octofhir_fhirpath::evaluator::function_registry::{
    EmptyPropagation, FunctionCategory, FunctionMetadata, FunctionSignature, LazyFunctionEvaluator,
    PureFunctionEvaluator,
};
use octofhir_fhirpath::{
    Collection, EvaluationContext, ExpressionNode, FhirPathError, FhirPathValue,
};
use serde_json::Value;

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

pub(super) struct GetResourceKey;

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

pub(super) struct GetReferenceKey;

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

// --- SoF-divergent override: join ----------------------------------------
//
// join() is overridden so the in-memory path matches the SQL-on-FHIR
// conformance suite, where the engine's stock behaviour differs: a
// zero-argument call defaults to the empty separator (and an empty input
// collection yields empty, not ""). lowBoundary() and highBoundary() follow
// the spec's precision-boundary rules too, in `boundary`.

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

pub(super) struct JoinFn;

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
