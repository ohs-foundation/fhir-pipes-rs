//! SQL generation from ViewDefinitions.
//!
//! Converts a [`ViewDefinition`] into a PostgreSQL query over FHIR resources
//! stored as JSONB. FHIRPath selectors, `where` filters and `constant`
//! references are parsed with the real `octofhir-fhirpath` parser and the AST is
//! lowered to SQL with a collection-aware model: every FHIRPath sub-expression
//! evaluates to a JSONB *array* (a FHIRPath collection), so a singleton and a
//! one-element collection behave identically and array navigation flattens the
//! way the spec requires.

use octofhir_fhirpath::parse_ast;

use crate::Error;
use crate::column::ColumnType;
use crate::view_definition::ViewDefinition;

mod boundary;
mod constants;
mod ddl;
mod dialect;
mod lower;

pub use ddl::{Dialect, create_table};

use lower::Lower;
// Re-exported for the in-memory evaluator (crate::eval), which shares the
// constant-handling logic.
pub(crate) use constants::{build_constants, substitute_constants};

/// Generates SQL queries from ViewDefinitions.
pub struct SqlGenerator {
    /// Alias used for the base table (e.g. `base` in `FROM patient base`).
    table_pattern: String,
    /// Optional row-status predicate template; `{base}` is replaced with the
    /// base alias. `None` disables row filtering entirely.
    row_filter: Option<String>,
    /// SQL dialect for the generated SELECT query.
    dialect: Dialect,
}

impl Default for SqlGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlGenerator {
    /// Create a new SQL generator with default settings.
    pub fn new() -> Self {
        Self {
            table_pattern: "base".to_string(),
            row_filter: Some("{base}.status <> 'deleted'".to_string()),
            dialect: Dialect::Postgres,
        }
    }

    /// Create a new SQL generator with a custom base-table alias.
    pub fn with_table_pattern(table_pattern: impl Into<String>) -> Self {
        Self {
            table_pattern: table_pattern.into(),
            ..Self::new()
        }
    }

    /// Select the SQL dialect for the generated SELECT query. Defaults to
    /// [`Dialect::Postgres`]. `Dialect::Ansi` is treated as `Postgres`.
    pub fn with_dialect(mut self, dialect: Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Set the row-status predicate. `{base}` is replaced with the base alias.
    /// Pass `None` to emit no row filter (useful for plain tables that have no
    /// `status` column).
    pub fn with_row_filter(mut self, filter: Option<String>) -> Self {
        self.row_filter = filter;
        self
    }

    /// Generate SQL from a ViewDefinition.
    ///
    /// # Errors
    ///
    /// Returns an error if a FHIRPath selector cannot be parsed or lowered, if a
    /// referenced constant is undefined, or if the view's column shape is
    /// inconsistent across `unionAll` branches.
    pub fn generate(&self, view: &ViewDefinition) -> Result<GeneratedSql, Error> {
        if view.resource.trim().is_empty() {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition is missing the required `resource`".to_string(),
            ));
        }

        let constants = build_constants(view)?;
        let table = view.resource.to_lowercase();
        let ctx0 = format!("{}.resource", self.table_pattern);
        let lower = Lower::new(view.resource.clone(), constants, ctx0.clone(), self.dialect);

        // Top-level selects cross-join, exactly like nested selects.
        let mut plans = vec![Plan::empty()];
        for select in &view.select {
            let mut next = Vec::new();
            for p in &plans {
                for child in lower.build_select(select, &p.joins, &ctx0)? {
                    next.push(p.cross(&child));
                }
            }
            plans = next;
        }

        if plans.is_empty() || plans.iter().all(|p| p.columns.is_empty()) {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition produces no columns".to_string(),
            ));
        }

        // Every UNION ALL branch must expose the same ordered column shape.
        let shape: Vec<&str> = plans[0].columns.iter().map(|c| c.name.as_str()).collect();
        for p in &plans[1..] {
            let other: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
            if other != shape {
                return Err(Error::InvalidViewDefinition(
                    "unionAll branches have mismatched column shape".to_string(),
                ));
            }
        }

        // Lower top-level `where` filters to booleans over the base resource.
        let mut where_sql = Vec::new();
        for clause in &view.where_ {
            let path = lower.substitute(&clause.path)?;
            let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
            where_sql.push(lower.bool(&ast, &ctx0)?);
        }

        let select_sqls: Vec<String> = plans
            .iter()
            .map(|p| self.render_plan(&table, p, &where_sql))
            .collect();
        let sql = select_sqls.join(" UNION ALL ");

        let columns = plans[0]
            .columns
            .iter()
            .map(|c| GeneratedColumn {
                name: c.name.clone(),
                expression: c.expr.clone(),
                alias: c.name.clone(),
                col_type: c.col_type,
            })
            .collect();

        Ok(GeneratedSql {
            sql,
            columns,
            ctes: Vec::new(),
        })
    }

    fn render_plan(&self, table: &str, plan: &Plan, where_sql: &[String]) -> String {
        let cols: Vec<String> = plan
            .columns
            .iter()
            .map(|c| format!("{} AS \"{}\"", c.expr, c.name.replace('"', "\"\"")))
            .collect();
        let mut sql = format!(
            "SELECT {} FROM {} {}",
            cols.join(", "),
            table,
            self.table_pattern
        );
        for j in &plan.joins {
            sql.push(' ');
            sql.push_str(j);
        }
        let mut conds = Vec::new();
        if let Some(f) = &self.row_filter {
            conds.push(f.replace("{base}", &self.table_pattern));
        }
        for w in where_sql {
            conds.push(format!("({w})"));
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql
    }
}

/// A column produced by a plan, with the SQL expression that yields it.
#[derive(Debug, Clone)]
struct PlanColumn {
    name: String,
    expr: String,
    col_type: ColumnType,
}

/// One UNION ALL branch: a chain of lateral joins plus its output columns.
#[derive(Debug, Clone)]
struct Plan {
    joins: Vec<String>,
    columns: Vec<PlanColumn>,
}

impl Plan {
    fn empty() -> Self {
        Self {
            joins: Vec::new(),
            columns: Vec::new(),
        }
    }

    /// Cross join two plans: the child's joins already include this plan's joins
    /// as a prefix, so its join list wins; columns concatenate.
    fn cross(&self, child: &Plan) -> Plan {
        let mut columns = self.columns.clone();
        columns.extend(child.columns.iter().cloned());
        Plan {
            joins: child.joins.clone(),
            columns,
        }
    }
}

/// Generated SQL with column metadata.
#[derive(Debug, Clone)]
pub struct GeneratedSql {
    /// The generated SQL query.
    pub sql: String,

    /// Column information for the result set.
    pub columns: Vec<GeneratedColumn>,

    /// Common Table Expressions (CTEs) to prepend to the query.
    pub ctes: Vec<String>,
}

/// A generated column with its SQL expression and metadata.
#[derive(Debug, Clone)]
pub struct GeneratedColumn {
    /// Original column name from the ViewDefinition.
    pub name: String,

    /// SQL expression that produces this column's value.
    pub expression: String,

    /// Alias used in the SQL SELECT clause.
    pub alias: String,

    /// Data type of the column.
    pub col_type: ColumnType,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_sql(view: serde_json::Value) -> GeneratedSql {
        let v = ViewDefinition::from_json(&view).unwrap();
        SqlGenerator::new().generate(&v).unwrap()
    }

    #[test]
    fn simple_columns() {
        let g = build_sql(json!({
            "resource": "Patient",
            "select": [{ "column": [
                { "name": "id", "path": "id", "type": "id" },
                { "name": "gender", "path": "gender", "type": "code" }
            ] }]
        }));
        assert!(g.sql.contains("FROM patient base"));
        assert_eq!(g.columns.len(), 2);
        assert_eq!(g.columns[0].name, "id");
    }

    #[test]
    fn collection_column_is_json() {
        let g = build_sql(json!({
            "resource": "Patient",
            "select": [{ "column": [
                { "name": "fam", "path": "name.family", "type": "string", "collection": true }
            ] }]
        }));
        assert_eq!(g.columns[0].col_type, ColumnType::Json);
    }

    #[test]
    fn union_shape_mismatch_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "unionAll": [
                { "column": [{ "name": "a", "path": "id" }, { "name": "b", "path": "id" }] },
                { "column": [{ "name": "a", "path": "id" }, { "name": "c", "path": "id" }] }
            ] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }

    #[test]
    fn undefined_constant_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "forEach": "name.where(use = %missing)",
                "column": [{ "name": "f", "path": "family" }] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }

    #[test]
    fn missing_resource_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }
}
