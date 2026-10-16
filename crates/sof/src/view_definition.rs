//! ViewDefinition parsing and types.
//!
//! This module defines the data structures for parsing FHIR ViewDefinition resources
//! as specified in the SQL on FHIR Implementation Guide.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

/// A ViewDefinition resource that defines a tabular view over FHIR data.
///
/// ViewDefinitions specify how to transform FHIR resources into flat,
/// tabular data suitable for SQL queries and analytics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewDefinition {
    /// The FHIR resource type (always "ViewDefinition" when present).
    ///
    /// Optional so that partial ViewDefinition objects (as used by the
    /// SQL-on-FHIR conformance test suite) parse without a wrapper.
    pub resource_type: Option<String>,

    /// Canonical URL identifying this ViewDefinition.
    pub url: Option<String>,

    /// Human-readable name for the view.
    pub name: Option<String>,

    /// Publication status: draft | active | retired | unknown.
    pub status: Option<String>,

    /// The FHIR resource type this view is based on (e.g., "Patient", "Observation").
    pub resource: String,

    /// Description of the view's purpose.
    pub description: Option<String>,

    /// Target FHIR profiles this view executes against.
    #[serde(default)]
    pub profile: Vec<String>,

    /// Applicable FHIR versions for this view.
    #[serde(default)]
    pub fhir_version: Vec<String>,

    /// The columns and nested selects to include in the view.
    #[serde(default)]
    pub select: Vec<SelectColumn>,

    /// Filter conditions to apply to the view.
    /// Note: Named `where_` because `where` is a Rust reserved keyword.
    #[serde(default, rename = "where")]
    pub where_: Vec<WhereClause>,

    /// Constants that can be referenced in FHIRPath expressions.
    #[serde(default)]
    pub constant: Vec<Constant>,
}

/// A select clause that defines columns or nested structures.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectColumn {
    /// FHIRPath expression to evaluate for this select.
    pub path: Option<String>,

    /// Alias for this select (used as table prefix for nested selects).
    pub alias: Option<String>,

    /// Whether this select represents a collection.
    #[serde(default)]
    pub collection: bool,

    /// Nested select clauses.
    #[serde(default)]
    pub select: Vec<SelectColumn>,

    /// Column definitions at this level.
    pub column: Option<Vec<Column>>,

    /// FHIRPath expression for array expansion (creates one row per element).
    pub for_each: Option<String>,

    /// Like forEach, but includes a row with nulls if the array is empty.
    pub for_each_or_null: Option<String>,

    /// Recursive FHIRPath expressions for hierarchical data traversal.
    #[serde(default)]
    pub repeat: Vec<String>,

    /// Union of multiple select clauses.
    pub union_all: Option<Vec<SelectColumn>>,
}

/// A column definition in a ViewDefinition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    /// The column name in the output.
    pub name: String,

    /// FHIRPath expression to extract the column value.
    pub path: String,

    /// Expected data type of the column.
    #[serde(rename = "type")]
    pub col_type: Option<String>,

    /// Whether this column can contain multiple values.
    pub collection: Option<bool>,

    /// Human-readable description of the column.
    pub description: Option<String>,

    /// Metadata key-value pairs for implementation directives.
    #[serde(default)]
    pub tag: Vec<Tag>,
}

/// A metadata tag for columns with key-value pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    /// Tag name/key.
    pub name: String,

    /// Tag value.
    pub value: Option<String>,
}

/// A where clause for filtering rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhereClause {
    /// FHIRPath expression that must evaluate to true for the row to be included.
    pub path: String,

    /// Human-readable explanation of the constraint.
    pub description: Option<String>,
}

/// A constant value that can be referenced in FHIRPath expressions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Constant {
    /// Name of the constant (referenced as %name in FHIRPath).
    pub name: String,

    /// String value of the constant.
    pub value_string: Option<String>,

    /// Integer value of the constant.
    pub value_integer: Option<i64>,

    /// Boolean value of the constant.
    pub value_boolean: Option<bool>,

    /// Decimal value of the constant.
    pub value_decimal: Option<f64>,

    /// Any other polymorphic `value[x]` field (`valueCode`, `valueUri`,
    /// `valueDateTime`, `valuePositiveInt`, …). The SQL-on-FHIR spec allows the
    /// full FHIR primitive `value[x]` set on a constant; the explicit fields
    /// above cover the common cases, and this captures the remainder.
    #[serde(flatten)]
    pub values: serde_json::Map<String, Value>,
}

impl ViewDefinition {
    /// Parse a ViewDefinition from a JSON Value.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is not a valid ViewDefinition.
    pub fn from_json(value: &Value) -> Result<Self, Error> {
        serde_json::from_value(value.clone())
            .map_err(|e| Error::InvalidViewDefinition(e.to_string()))
    }

    /// The view's output columns, in order: each select's own columns, then
    /// its nested selects', then its `unionAll`'s, which are the first
    /// branch's. `mismatch` is called once for each `unionAll` whose branches
    /// do not all have the same column names in the same order, innermost
    /// first.
    pub fn output_columns(&self, mut mismatch: impl FnMut()) -> Vec<&Column> {
        let mut cols = Vec::new();
        for select in &self.select {
            output_columns_of(select, &mut mismatch, &mut cols);
        }
        cols
    }
}

fn output_columns_of<'a>(
    select: &'a SelectColumn,
    mismatch: &mut dyn FnMut(),
    cols: &mut Vec<&'a Column>,
) {
    if let Some(columns) = &select.column {
        cols.extend(columns);
    }
    for nested in &select.select {
        output_columns_of(nested, mismatch, cols);
    }
    if let Some(branches) = &select.union_all {
        let shapes: Vec<Vec<&Column>> = branches
            .iter()
            .map(|branch| {
                let mut shape = Vec::new();
                output_columns_of(branch, mismatch, &mut shape);
                shape
            })
            .collect();
        if let Some(first) = shapes.first() {
            let names = |shape: &[&Column]| -> Vec<String> {
                shape.iter().map(|c| c.name.clone()).collect()
            };
            let first_names = names(first);
            if shapes[1..].iter().any(|s| names(s) != first_names) {
                mismatch();
            }
            cols.extend(first);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_simple_view_definition() {
        let json = json!({
            "resourceType": "ViewDefinition",
            "name": "patient_demographics",
            "status": "active",
            "resource": "Patient",
            "select": [{
                "column": [{
                    "name": "id",
                    "path": "id"
                }, {
                    "name": "gender",
                    "path": "gender"
                }]
            }]
        });

        let view = ViewDefinition::from_json(&json).unwrap();
        assert_eq!(view.name.as_deref(), Some("patient_demographics"));
        assert_eq!(view.resource, "Patient");
        assert_eq!(view.select.len(), 1);

        let columns = view.select[0].column.as_ref().unwrap();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[1].name, "gender");
    }

    #[test]
    fn test_parse_view_with_foreach() {
        let json = json!({
            "resourceType": "ViewDefinition",
            "name": "patient_names",
            "status": "active",
            "resource": "Patient",
            "select": [{
                "forEach": "name",
                "column": [{
                    "name": "family",
                    "path": "family"
                }, {
                    "name": "given",
                    "path": "given.first()"
                }]
            }]
        });

        let view = ViewDefinition::from_json(&json).unwrap();
        assert_eq!(view.select[0].for_each, Some("name".to_string()));
    }

    #[test]
    fn test_parse_view_with_where() {
        let json = json!({
            "resourceType": "ViewDefinition",
            "name": "active_patients",
            "status": "active",
            "resource": "Patient",
            "select": [{
                "column": [{
                    "name": "id",
                    "path": "id"
                }]
            }],
            "where": [{
                "path": "active = true"
            }]
        });

        let view = ViewDefinition::from_json(&json).unwrap();
        assert_eq!(view.where_.len(), 1);
        assert_eq!(view.where_[0].path, "active = true");
    }

    #[test]
    fn test_parse_view_with_constants() {
        let json = json!({
            "resourceType": "ViewDefinition",
            "name": "test_view",
            "status": "active",
            "resource": "Patient",
            "constant": [{
                "name": "statusFilter",
                "valueString": "active"
            }, {
                "name": "maxAge",
                "valueInteger": 65
            }],
            "select": [{
                "column": [{
                    "name": "id",
                    "path": "id"
                }]
            }]
        });

        let view = ViewDefinition::from_json(&json).unwrap();
        assert_eq!(view.constant.len(), 2);
        assert_eq!(view.constant[0].name, "statusFilter");
        assert_eq!(view.constant[0].value_string, Some("active".to_string()));
        assert_eq!(view.constant[1].value_integer, Some(65));
    }

    #[test]
    fn output_columns_take_the_first_union_branch_and_report_mismatches() {
        let view = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [
                { "column": [{ "name": "id", "path": "id" }] },
                { "unionAll": [
                    { "column": [{ "name": "a", "path": "id" }],
                      "unionAll": [
                        { "column": [{ "name": "x", "path": "id" }] },
                        { "column": [{ "name": "y", "path": "id" }] }
                      ] },
                    { "column": [{ "name": "b", "path": "id" }] }
                ] }
            ]
        }))
        .unwrap();
        let mut mismatches = 0;
        let names: Vec<&str> = view
            .output_columns(|| mismatches += 1)
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["id", "a", "x"]);
        // The inner unionAll (x, y) and the outer one ([a, x], [b]).
        assert_eq!(mismatches, 2);
    }
}
