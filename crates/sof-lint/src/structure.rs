//! Structural validation of a ViewDefinition against the SQL-on-FHIR v2 spec,
//! independent of any FHIR schema or SQL backend.
//!
//! These checks mirror the spec's StructureDefinition invariants and the
//! `ValidateColumns` algorithm: SQL-safe names, at most one iteration construct
//! per `select`, unique column names across the view, and consistent `unionAll`
//! branch shapes. They need no FHIR package, so they back the offline `validate`
//! command and also run as part of `lint`.

use fhir_pipes_sof::{Column, SelectColumn, ViewDefinition};

use crate::finding::{Finding, Severity};

/// Validate a ViewDefinition's structure. Returns spec-violation findings
/// (errors); an empty vector means the view is structurally well-formed.
pub fn validate_structure(view: &ViewDefinition) -> Vec<Finding> {
    let mut out = Vec::new();

    if view.resource.trim().is_empty() {
        out.push(err(
            "FH10",
            "ViewDefinition is missing the required `resource`",
        ));
    }
    // SQL-on-FHIR's sql-name rule is the requirement. FHIR's cnl-0 (a capital
    // first letter) is a warning in FHIR itself, and views such as
    // fhir-data-pipes' `patient_flat` are valid without it.
    if let Some(name) = &view.name {
        if !is_sql_name(name) {
            out.push(err(
                "FH06",
                format!("view name `{name}` is not a valid SQL name"),
            ));
        } else if !is_resource_name(name) {
            out.push(Finding::fhir(
                "FH06",
                Severity::Warning,
                format!(
                    "view name `{name}` should match ^[A-Z][A-Za-z0-9_]{{1,254}}$ (FHIR cnl-0)"
                ),
            ));
        }
    }
    for c in &view.constant {
        if !is_sql_name(&c.name) {
            out.push(
                err(
                    "FH06",
                    format!("constant name `{}` is not a valid SQL name", c.name),
                )
                .at(&c.name),
            );
        }
    }

    walk(&view.select, &mut out);

    // The ordered output column names, with `unionAll` branches contributing
    // once (and flagged when their shapes disagree).
    let names: Vec<String> = view
        .output_columns(|| {
            out.push(err(
                "FH09",
                "unionAll branches have mismatched column shape (same names in the same order required)",
            ))
        })
        .into_iter()
        .map(|c| c.name.clone())
        .collect();
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        if !seen.insert(name) {
            out.push(err("FH08", format!("column `{name}` is defined more than once")).at(name));
        }
    }
    if names.is_empty() {
        out.push(err("FH10", "ViewDefinition produces no columns"));
    }

    out
}

/// Per-select invariants: SQL-safe column names and the `sql-expressions`
/// invariant (at most one of forEach / forEachOrNull / repeat).
fn walk(selects: &[SelectColumn], out: &mut Vec<Finding>) {
    for select in selects {
        let iterations = [
            select.for_each.is_some(),
            select.for_each_or_null.is_some(),
            !select.repeat.is_empty(),
        ]
        .into_iter()
        .filter(|x| *x)
        .count();
        if iterations > 1 {
            out.push(err(
                "FH07",
                "a select may use at most one of forEach, forEachOrNull or repeat",
            ));
        }
        if let Some(columns) = &select.column {
            for col in columns {
                check_name(col, out);
            }
        }
        walk(&select.select, out);
        if let Some(branches) = &select.union_all {
            walk(branches, out);
        }
    }
}

fn check_name(col: &Column, out: &mut Vec<Finding>) {
    if !is_sql_name(&col.name) {
        out.push(
            err(
                "FH06",
                format!("column name `{}` is not a valid SQL name", col.name),
            )
            .at(&col.name),
        );
    }
}

/// `sql-name` invariant: `^[A-Za-z][A-Za-z0-9_]*$`.
fn is_sql_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `cnl-0` invariant for the resource-level name: `^[A-Z][A-Za-z0-9_]{1,254}$`.
fn is_resource_name(name: &str) -> bool {
    let len = name.chars().count();
    if !(2..=255).contains(&len) {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn err(code: &str, message: impl Into<String>) -> Finding {
    Finding::fhir(code, Severity::Error, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(v: serde_json::Value) -> ViewDefinition {
        ViewDefinition::from_json(&v).unwrap()
    }

    fn codes(fs: &[Finding]) -> Vec<&str> {
        fs.iter().map(|f| f.code.as_str()).collect()
    }

    #[test]
    fn well_formed_view_is_clean() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }]
        })));
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn invalid_column_name() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [{ "column": [{ "name": "1bad", "path": "id" }] }]
        })));
        assert_eq!(codes(&f), vec!["FH06"]);
    }

    #[test]
    fn lowercase_view_name_is_a_warning() {
        let f = validate_structure(&view(json!({
            "name": "patient_flat",
            "resource": "Patient",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }]
        })));
        assert_eq!(codes(&f), vec!["FH06"]);
        assert_eq!(f[0].severity, Severity::Warning);
        let f = validate_structure(&view(json!({
            "name": "patient-flat",
            "resource": "Patient",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }]
        })));
        assert_eq!(f[0].severity, Severity::Error);
    }

    #[test]
    fn duplicate_column_name() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [
                { "column": [{ "name": "id", "path": "id" }] },
                { "column": [{ "name": "id", "path": "id" }] }
            ]
        })));
        assert_eq!(codes(&f), vec!["FH08"]);
    }

    #[test]
    fn too_many_iterations() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [{
                "forEach": "name",
                "repeat": ["link"],
                "column": [{ "name": "id", "path": "id" }]
            }]
        })));
        assert!(codes(&f).contains(&"FH07"));
    }

    #[test]
    fn union_shape_mismatch() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [{ "unionAll": [
                { "column": [{ "name": "a", "path": "id" }] },
                { "column": [{ "name": "b", "path": "id" }] }
            ] }]
        })));
        assert!(codes(&f).contains(&"FH09"));
    }

    #[test]
    fn union_same_shape_is_clean() {
        let f = validate_structure(&view(json!({
            "resource": "Patient",
            "select": [{ "unionAll": [
                { "column": [{ "name": "a", "path": "id" }] },
                { "column": [{ "name": "a", "path": "name.given" }] }
            ] }]
        })));
        assert!(f.is_empty(), "{f:?}");
    }
}
