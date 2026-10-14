//! Integration tests for SQL on FHIR generation.
//!
//! These exercise the full ViewDefinition → SQL flow at the level the library
//! guarantees: successful generation, column metadata, and the error cases the
//! spec requires. Row-level behaviour is covered by the Postgres-backed
//! conformance harness (`tests/conformance.rs`), which is the source of truth;
//! these tests deliberately avoid asserting on the internal SQL string shape.

use octofhir_sof::{ColumnType, SqlGenerator, ViewDefinition};
use serde_json::json;

fn generate(view_json: serde_json::Value) -> octofhir_sof::GeneratedSql {
    let view = ViewDefinition::from_json(&view_json).expect("parse ViewDefinition");
    SqlGenerator::new().generate(&view).expect("generate SQL")
}

fn col_names(g: &octofhir_sof::GeneratedSql) -> Vec<&str> {
    g.columns.iter().map(|c| c.name.as_str()).collect()
}

#[test]
fn demographics_columns_and_types() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{ "column": [
            { "name": "id", "path": "id", "type": "id" },
            { "name": "gender", "path": "gender", "type": "code" },
            { "name": "birth_date", "path": "birthDate", "type": "date" },
            { "name": "active", "path": "active", "type": "boolean" }
        ] }]
    }));
    assert_eq!(col_names(&g), vec!["id", "gender", "birth_date", "active"]);
    assert_eq!(g.columns[2].col_type, ColumnType::Date);
    assert_eq!(g.columns[3].col_type, ColumnType::Boolean);
    assert!(g.sql.contains("FROM patient base"));
}

#[test]
fn for_each_generates_lateral_join() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [
            { "column": [{ "name": "id", "path": "id", "type": "id" }] },
            { "forEach": "name", "column": [{ "name": "family", "path": "family", "type": "string" }] }
        ]
    }));
    assert_eq!(col_names(&g), vec!["id", "family"]);
    assert!(g.sql.contains("CROSS JOIN LATERAL"));
}

#[test]
fn for_each_or_null_uses_left_join() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [
            { "column": [{ "name": "id", "path": "id", "type": "id" }] },
            { "forEachOrNull": "name", "column": [{ "name": "family", "path": "family", "type": "string" }] }
        ]
    }));
    assert!(g.sql.contains("LEFT JOIN LATERAL"));
    assert!(g.sql.contains("ON true"));
}

#[test]
fn collection_column_is_json() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{ "column": [
            { "name": "given", "path": "name.given", "type": "string", "collection": true }
        ] }]
    }));
    assert_eq!(g.columns[0].col_type, ColumnType::Json);
}

#[test]
fn union_all_produces_single_query() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{
            "column": [{ "name": "id", "path": "id", "type": "id" }],
            "unionAll": [
                { "forEach": "telecom", "column": [{ "name": "v", "path": "value", "type": "string" }] },
                { "forEach": "contact.telecom", "column": [{ "name": "v", "path": "value", "type": "string" }] }
            ]
        }]
    }));
    assert!(g.sql.contains("UNION ALL"));
    assert_eq!(col_names(&g), vec!["id", "v"]);
}

#[test]
fn union_all_shape_mismatch_is_error() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": [{ "unionAll": [
            { "column": [{ "name": "a", "path": "id" }, { "name": "b", "path": "id" }] },
            { "column": [{ "name": "b", "path": "id" }, { "name": "a", "path": "id" }] }
        ] }]
    }))
    .unwrap();
    assert!(SqlGenerator::new().generate(&view).is_err());
}

#[test]
fn constant_is_substituted() {
    let g = generate(json!({
        "resource": "Patient",
        "constant": [{ "name": "use", "valueString": "official" }],
        "select": [{ "column": [
            { "name": "fam", "path": "name.where(use = %use).family", "type": "string" }
        ] }]
    }));
    assert!(g.sql.contains("official"));
}

#[test]
fn undefined_constant_is_error() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": [{ "column": [
            { "name": "fam", "path": "name.where(use = %missing).family", "type": "string" }
        ] }]
    }))
    .unwrap();
    assert!(SqlGenerator::new().generate(&view).is_err());
}

#[test]
fn nested_select_cross_joins() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{
            "column": [{ "name": "c_id", "path": "id", "type": "id" }],
            "select": [{ "column": [{ "name": "s_id", "path": "id", "type": "id" }] }]
        }]
    }));
    assert_eq!(col_names(&g), vec!["c_id", "s_id"]);
}

#[test]
fn where_clause_emitted() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{ "column": [{ "name": "id", "path": "id", "type": "id" }] }],
        "where": [{ "path": "active = true" }]
    }));
    assert!(g.sql.contains("WHERE"));
}

#[test]
fn invalid_fhirpath_is_error() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": [{ "forEach": "@@", "column": [{ "name": "x", "path": "id" }] }]
    }))
    .unwrap();
    assert!(SqlGenerator::new().generate(&view).is_err());
}

#[test]
fn empty_view_is_error() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": []
    }))
    .unwrap();
    assert!(SqlGenerator::new().generate(&view).is_err());
}

#[test]
fn reference_key_handles_contained_fragment() {
    // getReferenceKey must resolve a fragment (`#id`) reference into the
    // resource's contained[]; the SQL strips the `#` and a typed call checks
    // the contained resourceType. (Row behaviour is asserted by the
    // contained-resource conformance fixture on both paths.)
    let g = generate(json!({
        "resource": "Patient",
        "select": [{ "column": [
            { "name": "k", "path": "managingOrganization.getReferenceKey(Organization)", "type": "string" }
        ] }]
    }));
    assert!(
        g.sql.contains("LIKE '#%'"),
        "fragment branch missing: {}",
        g.sql
    );
    assert!(
        g.sql.contains("'contained'"),
        "contained lookup missing: {}",
        g.sql
    );
}

#[test]
fn generated_columns_metadata() {
    let g = generate(json!({
        "resource": "Patient",
        "select": [{ "column": [
            { "name": "id", "path": "id", "type": "id" },
            { "name": "cnt", "path": "name.count()", "type": "integer" }
        ] }]
    }));
    assert_eq!(g.columns.len(), 2);
    assert_eq!(g.columns[1].col_type, ColumnType::Integer);
    assert_eq!(g.columns[1].alias, "cnt");
}
