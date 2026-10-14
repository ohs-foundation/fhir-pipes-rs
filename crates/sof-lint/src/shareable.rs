//! FH11 — ShareableViewDefinition FHIRPath allow-list.
//!
//! The Shareable View Definition profile requires runners to implement only a
//! minimal FHIRPath subset, so a view that stays inside it runs unchanged on any
//! conformant engine. This rule walks every FHIRPath expression in the view and
//! flags anything outside that subset.
//!
//! Tiers (source: SQL-on-FHIR v2 ShareableViewDefinition notes,
//! <https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/StructureDefinition-ShareableViewDefinition.html>):
//!
//! - **Required** (allowed): literals String/Integer/Decimal (and Boolean, used
//!   by the comparison/boolean operators); functions `where`, `exists`,
//!   `empty`, `extension`, `ofType`, `first`; boolean operators `and`/`or`/`not`;
//!   math `+ - * /`; comparisons `= != > <=`; indexer `[]`.
//! - **Experimental** (warn): `join`, `lowBoundary`, `highBoundary` — slated for
//!   the required subset but not yet normative FHIRPath.
//! - Everything else (other functions/operators/literals) is outside the subset
//!   and reported as an error under strict (`--shareable`) enforcement.
//!
//! `getResourceKey`/`getReferenceKey` are SQL-on-FHIR special functions (always
//! available, defined by the spec itself) and are allowed. Engine-registered
//! custom functions can be exempted via the `allowed_custom` list so the strict
//! pass does not hard-fail them.

use octofhir_fhirpath::{BinaryOperator, ExpressionNode, LiteralValue, parse_ast};
use octofhir_sof::{SelectColumn, ViewDefinition};

use crate::finding::{Finding, Severity};

/// FHIRPath functions in the Shareable required subset.
const REQUIRED_FUNCS: &[&str] = &["where", "exists", "empty", "extension", "ofType", "first"];
/// SQL-on-FHIR special functions — always available, not part of FHIRPath.
const CORE_FUNCS: &[&str] = &["getResourceKey", "getReferenceKey"];
/// Functions intended for the subset but not yet normative FHIRPath.
const EXPERIMENTAL_FUNCS: &[&str] = &["join", "lowBoundary", "highBoundary"];

/// Lint a ViewDefinition against the ShareableViewDefinition FHIRPath subset.
///
/// `allowed_custom` lists extra function names (e.g. engine-registered custom
/// FHIRPath functions) that should not be flagged.
pub fn lint_shareable(view: &ViewDefinition, allowed_custom: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    walk_selects(&view.select, allowed_custom, &mut out);
    for w in &view.where_ {
        check_expr(&w.path, &w.path, allowed_custom, &mut out);
    }
    out
}

fn walk_selects(selects: &[SelectColumn], allowed: &[String], out: &mut Vec<Finding>) {
    for select in selects {
        if let Some(p) = &select.for_each {
            check_expr(p, p, allowed, out);
        }
        if let Some(p) = &select.for_each_or_null {
            check_expr(p, p, allowed, out);
        }
        for p in &select.repeat {
            check_expr(p, p, allowed, out);
        }
        if let Some(columns) = &select.column {
            for col in columns {
                check_expr(&col.path, &col.name, allowed, out);
            }
        }
        walk_selects(&select.select, allowed, out);
        if let Some(union) = &select.union_all {
            walk_selects(union, allowed, out);
        }
    }
}

fn check_expr(path: &str, location: &str, allowed: &[String], out: &mut Vec<Finding>) {
    // Parse errors are surfaced by other rules; skip here to avoid double-report.
    if let Ok(ast) = parse_ast(path) {
        walk(&ast, location, allowed, out);
    }
}

fn finding(severity: Severity, location: &str, msg: String) -> Finding {
    Finding::fhir("FH11", severity, msg).at(location)
}

/// Recursively classify every function, operator and literal in `expr`.
fn walk(expr: &ExpressionNode, location: &str, allowed: &[String], out: &mut Vec<Finding>) {
    match expr {
        ExpressionNode::Literal(l) => check_literal(&l.value, location, out),
        ExpressionNode::FunctionCall(f) => {
            check_function(&f.name, location, allowed, out);
            for a in &f.arguments {
                walk(a, location, allowed, out);
            }
        }
        ExpressionNode::MethodCall(m) => {
            check_function(&m.method, location, allowed, out);
            walk(&m.object, location, allowed, out);
            for a in &m.arguments {
                walk(a, location, allowed, out);
            }
        }
        ExpressionNode::BinaryOperation(b) => {
            check_binary(&b.operator, location, out);
            walk(&b.left, location, allowed, out);
            walk(&b.right, location, allowed, out);
        }
        ExpressionNode::UnaryOperation(u) => {
            // All three unary operators (not, -, +) are in the subset.
            let _ = &u.operator;
            walk(&u.operand, location, allowed, out);
        }
        ExpressionNode::Union(_) => {
            out.push(finding(
                Severity::Error,
                location,
                "collection union `|` is not in the Shareable FHIRPath subset".into(),
            ));
            if let ExpressionNode::Union(u) = expr {
                walk(&u.left, location, allowed, out);
                walk(&u.right, location, allowed, out);
            }
        }
        ExpressionNode::TypeCheck(t) => {
            out.push(finding(
                Severity::Error,
                location,
                "type operator `is` is not in the Shareable FHIRPath subset".into(),
            ));
            walk(&t.expression, location, allowed, out);
        }
        ExpressionNode::TypeCast(c) => {
            out.push(finding(
                Severity::Error,
                location,
                "type operator `as` is not in the Shareable FHIRPath subset".into(),
            ));
            walk(&c.expression, location, allowed, out);
        }
        ExpressionNode::PropertyAccess(p) => walk(&p.object, location, allowed, out),
        ExpressionNode::IndexAccess(i) => {
            walk(&i.object, location, allowed, out);
            walk(&i.index, location, allowed, out);
        }
        ExpressionNode::Filter(fl) => {
            walk(&fl.base, location, allowed, out);
            walk(&fl.condition, location, allowed, out);
        }
        ExpressionNode::Parenthesized(e) => walk(e, location, allowed, out),
        ExpressionNode::Collection(c) => {
            for e in &c.elements {
                walk(e, location, allowed, out);
            }
        }
        ExpressionNode::Lambda(l) => walk(&l.body, location, allowed, out),
        ExpressionNode::Path(p) => walk(&p.base, location, allowed, out),
        // Identifiers, variables ($this/%constants) and type-info references are
        // structural and carry no out-of-subset semantics.
        ExpressionNode::Identifier(_)
        | ExpressionNode::Variable(_)
        | ExpressionNode::TypeInfo(_) => {}
    }
}

fn check_function(name: &str, location: &str, allowed: &[String], out: &mut Vec<Finding>) {
    if REQUIRED_FUNCS.contains(&name)
        || CORE_FUNCS.contains(&name)
        || allowed.iter().any(|a| a == name)
    {
        return;
    }
    if EXPERIMENTAL_FUNCS.contains(&name) {
        out.push(finding(
            Severity::Warning,
            location,
            format!(
                "`{name}()` is experimental — slated for the Shareable subset but \
                 not yet normative FHIRPath; portability is not guaranteed"
            ),
        ));
        return;
    }
    out.push(finding(
        Severity::Error,
        location,
        format!(
            "`{name}()` is outside the ShareableViewDefinition portable FHIRPath \
             subset — a view using it may not run on other SQL-on-FHIR engines"
        ),
    ));
}

fn check_binary(op: &BinaryOperator, location: &str, out: &mut Vec<Finding>) {
    use BinaryOperator::*;
    let allowed = matches!(
        op,
        Add | Subtract
            | Multiply
            | Divide
            | Equal
            | NotEqual
            | GreaterThan
            | LessThanOrEqual
            | And
            | Or
    );
    if allowed {
        return;
    }
    let sym = match op {
        Modulo => "mod",
        IntegerDivide => "div",
        Equivalent => "~",
        NotEquivalent => "!~",
        LessThan => "<",
        GreaterThanOrEqual => ">=",
        Xor => "xor",
        Implies => "implies",
        Concatenate => "&",
        Union => "|",
        In => "in",
        Contains => "contains",
        Is => "is",
        As => "as",
        _ => "operator",
    };
    out.push(finding(
        Severity::Error,
        location,
        format!(
            "operator `{sym}` is outside the ShareableViewDefinition portable \
             FHIRPath subset — a view using it may not run on other SQL-on-FHIR engines"
        ),
    ));
}

fn check_literal(lit: &LiteralValue, location: &str, out: &mut Vec<Finding>) {
    match lit {
        LiteralValue::String(_)
        | LiteralValue::Integer(_)
        | LiteralValue::Decimal(_)
        | LiteralValue::Boolean(_) => {}
        LiteralValue::Long(_) => out.push(finding(
            Severity::Warning,
            location,
            "long (`L`-suffixed) literals are a FHIRPath 3.0 ballot feature, not in \
             the Shareable required subset"
                .into(),
        )),
        LiteralValue::Date(_) | LiteralValue::DateTime(_) | LiteralValue::Time(_) => {
            out.push(finding(
                Severity::Error,
                location,
                "date/time literals are not in the Shareable required subset (the \
                 required literal types are String, Integer and Decimal)"
                    .into(),
            ))
        }
        LiteralValue::Quantity { .. } => out.push(finding(
            Severity::Error,
            location,
            "quantity literals are not in the Shareable required subset".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(path: &str) -> ViewDefinition {
        ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "column": [{ "name": "c", "path": path }] }]
        }))
        .unwrap()
    }

    fn codes_sev(fs: &[Finding]) -> Vec<(String, Severity)> {
        fs.iter().map(|f| (f.code.clone(), f.severity)).collect()
    }

    #[test]
    fn required_subset_is_clean() {
        for p in [
            "name.where(use = 'official').family.first()",
            "name.exists()",
            "name.empty()",
            "value.ofType(Quantity).first()",
            "extension('http://x').value",
            "managingOrganization.getReferenceKey(Organization)",
            "getResourceKey()",
            "telecom[0].value",
            "(age + 1) * 2 = 4",
            "active and gender = 'male'",
            "age > 18 and age <= 65",
        ] {
            let f = lint_shareable(&view(p), &[]);
            assert!(f.is_empty(), "expected clean for `{p}`, got {f:?}");
        }
    }

    #[test]
    fn experimental_functions_warn() {
        for fn_name in ["join", "lowBoundary", "highBoundary"] {
            let f = lint_shareable(&view(&format!("name.given.{fn_name}()")), &[]);
            assert_eq!(f.len(), 1, "{fn_name}: {f:?}");
            assert_eq!(f[0].code, "FH11");
            assert_eq!(f[0].severity, Severity::Warning);
        }
    }

    #[test]
    fn out_of_subset_function_errors() {
        let f = lint_shareable(&view("name.given.substring(0, 1)"), &[]);
        assert_eq!(codes_sev(&f), vec![("FH11".into(), Severity::Error)]);
        assert!(f[0].message.contains("substring"));
    }

    #[test]
    fn out_of_subset_operator_errors() {
        let f = lint_shareable(&view("age < 18"), &[]);
        assert_eq!(codes_sev(&f), vec![("FH11".into(), Severity::Error)]);
        assert!(f[0].message.contains('<'));
    }

    #[test]
    fn allowed_custom_function_is_exempt() {
        let f = lint_shareable(&view("name.myCustomFn()"), &["myCustomFn".to_string()]);
        assert!(f.is_empty(), "{f:?}");
        // ...but flagged when not whitelisted.
        let f2 = lint_shareable(&view("name.myCustomFn()"), &[]);
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].severity, Severity::Error);
    }

    #[test]
    fn where_and_foreach_are_walked() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "forEach": "name.given.substring(0,1)",
                         "column": [{ "name": "c", "path": "value" }] }],
            "where": [{ "path": "active xor false" }]
        }))
        .unwrap();
        let f = lint_shareable(&v, &[]);
        // one for the forEach substring(), one for the `xor` in where.
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(
            f.iter()
                .all(|x| x.code == "FH11" && x.severity == Severity::Error)
        );
    }
}
