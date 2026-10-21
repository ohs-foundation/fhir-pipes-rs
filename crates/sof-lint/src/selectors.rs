//! Static validation of a ViewDefinition's FHIRPath selectors against FHIR
//! element information ([`ElementModel`]): the column, `where` and `forEach`
//! selectors are parsed with the FHIRPath parser and walked against the
//! model, so unknown elements and un-narrowed collections are reported
//! before any data is read (FH01–FH05).

use fhir_pipes_sof::{Column, SelectColumn, ViewDefinition};
use octofhir_fhirpath::{ExpressionNode, parse_ast};

use crate::finding::{Finding, Severity};
use crate::model::{CoreTypes, ElementKind, ElementModel};

/// One navigation step extracted from a FHIRPath selector.
#[derive(Clone)]
enum Step {
    /// A property navigation (`.name`).
    Field(String),
    /// A function that reduces a collection to a single item (`first()`, `[0]`).
    Collapse,
    /// `ofType(T)` — narrows a choice; deeper navigation is not resolved.
    OfType,
    /// Any other function. `where()` keeps the collection; everything else ends
    /// resolvable navigation.
    Func(String),
}

/// Lint a ViewDefinition against the FHIR R4 and R5 core types built into
/// fhir-pipes-sof: structural validation plus [`lint_selectors`] with
/// [`CoreTypes`]. Needs no FHIR package.
pub fn lint_core(view: &ViewDefinition) -> Vec<Finding> {
    let mut findings = crate::structure::validate_structure(view);
    findings.extend(lint_selectors(view, &CoreTypes));
    findings
}

/// Validate the ViewDefinition's FHIRPath selectors against `model`.
pub fn lint_selectors(view: &ViewDefinition, model: &dyn ElementModel) -> Vec<Finding> {
    let mut findings = Vec::new();
    let resource = view.resource.as_str();
    for select in &view.select {
        walk_select(select, resource, Some(&[]), model, &mut findings);
    }
    // A where path is a filter: it may go through arrays.
    for w in &view.where_ {
        check(
            &w.path,
            &w.path,
            resource,
            &[],
            true,
            false,
            model,
            &mut findings,
        );
    }
    findings
}

/// `prefix` is the steps of the `forEach` paths above `select`, each item
/// one value; `None` when one of them cannot be followed (or under `repeat`),
/// and the selectors below are not checked.
fn walk_select(
    select: &SelectColumn,
    resource: &str,
    prefix: Option<&[Step]>,
    model: &dyn ElementModel,
    out: &mut Vec<Finding>,
) {
    let for_each = select
        .for_each
        .as_ref()
        .or(select.for_each_or_null.as_ref());
    let mut inner = prefix.map(<[Step]>::to_vec);
    if let (Some(path), Some(pre)) = (for_each, prefix) {
        check(path, path, resource, pre, true, false, model, out);
        inner = steps(path).map(|own| {
            let mut all = pre.to_vec();
            all.extend(own);
            // Each forEach item is one value.
            all.push(Step::Collapse);
            all
        });
    }
    if !select.repeat.is_empty() {
        inner = None;
    }
    if let Some(pre) = inner.as_deref() {
        for col in select.column.iter().flatten() {
            check_column(col, resource, pre, model, out);
        }
    }
    for nested in select.nested() {
        walk_select(nested, resource, inner.as_deref(), model, out);
    }
}

fn check_column(
    col: &Column,
    resource: &str,
    prefix: &[Step],
    model: &dyn ElementModel,
    out: &mut Vec<Finding>,
) {
    let collection = col.collection.unwrap_or(false);
    check(
        &col.path, &col.name, resource, prefix, collection, true, model, out,
    );
}

/// The steps of a plain navigation selector; `None` for anything else
/// (operators, unions, literals) or a parse error, which are not checked.
fn steps(path: &str) -> Option<Vec<Step>> {
    let ast = parse_ast(path).ok()?;
    let mut steps = Vec::new();
    linearize(&ast, &mut steps).then_some(steps)
}

#[allow(clippy::too_many_arguments)]
fn check(
    path: &str,
    location: &str,
    resource: &str,
    prefix: &[Step],
    collection_ok: bool,
    is_column: bool,
    model: &dyn ElementModel,
    out: &mut Vec<Finding>,
) {
    let Some(own) = steps(path) else {
        return;
    };
    let mut all = prefix.to_vec();
    all.extend(own);
    validate_steps(
        &all,
        prefix.len(),
        location,
        resource,
        collection_ok,
        is_column,
        model,
        out,
    );
}

/// `own` is where the selector's own steps start in `steps` (after its
/// `forEach` prefix): findings are reported only for those.
#[allow(clippy::too_many_arguments)]
fn validate_steps(
    steps: &[Step],
    own: usize,
    location: &str,
    resource: &str,
    collection_ok: bool,
    is_column: bool,
    model: &dyn ElementModel,
    out: &mut Vec<Finding>,
) {
    let mut path: Vec<String> = Vec::new();
    let mut pending_array = false;
    let mut resolvable = true;
    let mut fh04_emitted = false;
    // How many elements the forEach prefix reached, so a column that adds
    // none (`getResourceKey()`) is not checked as selecting the focus.
    let mut prefix_len = 0;

    for (i, step) in steps.iter().enumerate() {
        let reporting = i >= own;
        if i == own {
            prefix_len = path.len();
        }
        match step {
            Step::Field(f) => {
                if path.is_empty() && f == resource {
                    continue; // leading resource type
                }
                if !resolvable {
                    continue;
                }
                let parent: Vec<&str> = path.iter().map(String::as_str).collect();
                match model.element_names(resource, &parent) {
                    Some(fields) => {
                        if !fields.iter().any(|x| x == f) {
                            if reporting {
                                out.push(unknown_field(f, &fields, location));
                            }
                            resolvable = false;
                            continue;
                        }
                        // Going through an array is what a forEach, a where
                        // path or a collection column asks for.
                        if pending_array && !fh04_emitted && reporting && !collection_ok {
                            out.push(array_navigation(location));
                            fh04_emitted = true;
                        }
                        path.push(f.clone());
                        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
                        pending_array = model.is_array(resource, &refs) == Some(true);
                        if model.is_choice(resource, &refs)
                            && !matches!(steps.get(i + 1), Some(Step::OfType))
                        {
                            if reporting {
                                out.push(choice_not_narrowed(f, location));
                            }
                            resolvable = false;
                        }
                    }
                    None => resolvable = false,
                }
            }
            Step::Collapse => pending_array = false,
            Step::OfType => resolvable = false,
            Step::Func(name) if name == "where" => {}
            Step::Func(_) => resolvable = false,
        }
    }

    if own >= steps.len() {
        prefix_len = path.len();
    }
    if resolvable && pending_array && !collection_ok && !fh04_emitted {
        out.push(array_terminal(location));
    }

    // Type-shape checks for scalar column selectors: the spec expects a column
    // to yield a primitive value. A non-array path that lands on a Reference or
    // any other complex element is flagged (FH05/FH03). Array terminals are
    // already covered by FH04; collection columns opt into structured output.
    if is_column && resolvable && !collection_ok && !pending_array && path.len() > prefix_len {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        match model.kind(resource, &refs) {
            Some(ElementKind::Reference) => out.push(reference_not_resolved(location)),
            Some(ElementKind::Complex) => out.push(complex_into_scalar(location)),
            _ => {}
        }
    }
}

fn unknown_field(field: &str, valid: &[String], location: &str) -> Finding {
    let mut msg = format!("unknown FHIR element `{field}`");
    if let Some(s) = suggest_similar(field, valid) {
        msg.push_str(&format!(" — did you mean `{s}`?"));
    }
    Finding::fhir("FH01", Severity::Error, msg).at(location)
}

/// The known name closest to `needle`: one it starts, or one within two
/// edits, case ignored.
fn suggest_similar<'a>(needle: &str, haystack: &'a [String]) -> Option<&'a str> {
    let needle = needle.to_lowercase();
    haystack
        .iter()
        .filter_map(|s| {
            let lower = s.to_lowercase();
            let prefix = lower.starts_with(&needle);
            let distance = levenshtein(&needle, &lower);
            (prefix || distance <= 2).then_some((!prefix, distance, s.as_str()))
        })
        .min()
        .map(|(.., s)| s)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

fn choice_not_narrowed(field: &str, location: &str) -> Finding {
    Finding::fhir(
        "FH02",
        Severity::Warning,
        format!(
            "selects choice element `{field}` without narrowing; add \
             ofType(<Type>) to pick a concrete type before extracting a value"
        ),
    )
    .at(location)
}

fn reference_not_resolved(location: &str) -> Finding {
    Finding::fhir(
        "FH05",
        Severity::Warning,
        "selects a Reference element directly; use getReferenceKey() to extract \
         a stable key, or navigate to a primitive such as .reference",
    )
    .at(location)
}

fn complex_into_scalar(location: &str) -> Finding {
    Finding::fhir(
        "FH03",
        Severity::Warning,
        "selects a complex element into a scalar column; navigate to a primitive \
         leaf, or mark the column collection to emit structured JSON",
    )
    .at(location)
}

fn array_navigation(location: &str) -> Finding {
    Finding::fhir(
        "FH04",
        Severity::Warning,
        "navigates into an array element without narrowing; the result is a \
         collection (use first()/where()/an index, or mark the column collection)",
    )
    .at(location)
}

fn array_terminal(location: &str) -> Finding {
    Finding::fhir(
        "FH04",
        Severity::Warning,
        "selects an array-valued element into a scalar column; narrow it \
         (first()/where()/index) or mark the column collection",
    )
    .at(location)
}

/// Flatten a navigation expression into ordered steps. Returns `false` for
/// expressions that are not plain navigation (binary ops, unions, literals).
fn linearize(expr: &ExpressionNode, out: &mut Vec<Step>) -> bool {
    match expr {
        ExpressionNode::Identifier(n) => {
            out.push(Step::Field(n.name.clone()));
            true
        }
        ExpressionNode::PropertyAccess(p) => {
            if !linearize(&p.object, out) {
                return false;
            }
            out.push(Step::Field(p.property.clone()));
            true
        }
        ExpressionNode::MethodCall(m) => {
            if !linearize(&m.object, out) {
                return false;
            }
            push_func(&m.method, out);
            true
        }
        ExpressionNode::FunctionCall(f) => {
            push_func(&f.name, out);
            true
        }
        ExpressionNode::IndexAccess(i) => {
            if !linearize(&i.object, out) {
                return false;
            }
            out.push(Step::Collapse);
            true
        }
        ExpressionNode::Parenthesized(e) => linearize(e, out),
        _ => false,
    }
}

fn push_func(name: &str, out: &mut Vec<Step>) {
    match name {
        "first" | "last" | "single" | "tail" => out.push(Step::Collapse),
        "ofType" => out.push(Step::OfType),
        other => out.push(Step::Func(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(resource: &str, select: serde_json::Value) -> ViewDefinition {
        ViewDefinition::from_json(&json!({ "resource": resource, "select": select })).unwrap()
    }

    fn column(resource: &str, path: &str, collection: bool) -> Vec<Finding> {
        let v = view(
            resource,
            json!([{ "column": [{ "name": "c", "path": path, "collection": collection }] }]),
        );
        lint_selectors(&v, &CoreTypes)
    }

    fn codes(fs: &[Finding]) -> Vec<&str> {
        fs.iter().map(|f| f.code.as_str()).collect()
    }

    #[test]
    fn core_types_find_unknown_elements() {
        let f = column("Patient", "nmae.family", false);
        assert_eq!(codes(&f), ["FH01"]);
        assert!(
            f[0].message.contains("did you mean `name`"),
            "{}",
            f[0].message
        );
        assert_eq!(codes(&column("Patient", "name.familly", true)), ["FH01"]);
        assert!(column("Patient", "gender", false).is_empty());
        assert!(column("Patient", "Patient.gender", false).is_empty());
    }

    #[test]
    fn core_types_check_shape() {
        assert_eq!(codes(&column("Patient", "name.family", false)), ["FH04"]);
        assert!(column("Patient", "name.first().family", false).is_empty());
        assert!(
            column(
                "Patient",
                "name.where(use = 'official').first().family",
                false
            )
            .is_empty()
        );
        assert_eq!(
            codes(&column("Patient", "managingOrganization", false)),
            ["FH05"]
        );
        assert!(column("Patient", "managingOrganization.getReferenceKey()", false).is_empty());
        assert_eq!(codes(&column("Patient", "name.first()", false)), ["FH03"]);
        assert_eq!(codes(&column("Observation", "value", false)), ["FH02"]);
        assert!(column("Observation", "value.ofType(Quantity).value", false).is_empty());
        assert!(column("Observation", "valueQuantity.value", false).is_empty());
        // An element the versions type differently is known, its kind is not.
        assert!(column("Account", "description", false).is_empty());
    }

    #[test]
    fn columns_under_for_each_start_at_its_items() {
        let v = view(
            "Patient",
            json!([{ "forEach": "name", "column": [
                { "name": "family", "path": "family" },
                { "name": "given", "path": "given" },
                { "name": "bad", "path": "famly" }
            ] }]),
        );
        let f = lint_selectors(&v, &CoreTypes);
        assert_eq!(codes(&f), ["FH04", "FH01"], "{f:?}");
        assert_eq!(f[0].location.as_deref(), Some("given"));
        assert_eq!(f[1].location.as_deref(), Some("bad"));

        let v = view(
            "Patient",
            json!([{ "forEach": "contact", "select": [{ "forEach": "telecom", "column": [
                { "name": "v", "path": "value" }
            ] }] }]),
        );
        assert!(lint_selectors(&v, &CoreTypes).is_empty());

        // A forEach that cannot be followed leaves its columns unchecked.
        let v = view(
            "Patient",
            json!([{ "forEach": "name | contact.name", "column": [{ "name": "x", "path": "famly" }] }]),
        );
        assert!(lint_selectors(&v, &CoreTypes).is_empty());
    }

    #[test]
    fn collections_may_go_through_arrays() {
        assert!(column("Patient", "name.family", true).is_empty());
        let v = view(
            "Condition",
            json!([{ "forEach": "category.coding", "column": [{ "name": "c", "path": "code" }] }]),
        );
        assert!(lint_selectors(&v, &CoreTypes).is_empty());
        let v = ViewDefinition::from_json(&json!({
            "resource": "Observation",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }],
            "where": [{ "path": "category.coding.where(code = 'laboratory').exists()" }]
        }))
        .unwrap();
        assert!(lint_selectors(&v, &CoreTypes).is_empty());
    }

    #[test]
    fn where_clauses_are_checked() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }],
            "where": [{ "path": "activ" }]
        }))
        .unwrap();
        assert_eq!(codes(&lint_selectors(&v, &CoreTypes)), ["FH01"]);
    }
}
