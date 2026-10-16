//! View results, and view execution against PostgreSQL.
//!
//! [`ViewResult`] holds the rows of a view, from the in-memory evaluator or
//! from [`ViewRunner`], which runs the generated SQL against a PostgreSQL
//! database (the `postgres` feature).

use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx_core::row::Row;
#[cfg(feature = "postgres")]
use sqlx_core::sql_str::AssertSqlSafe;
#[cfg(feature = "postgres")]
use sqlx_postgres::{PgPool, PgRow};

use crate::column::ColumnInfo;
#[cfg(any(feature = "postgres", test))]
use crate::column::ColumnType;
#[cfg(feature = "postgres")]
use crate::sql_generator::{GeneratedColumn, SqlGenerator};
#[cfg(feature = "postgres")]
use crate::view_definition::ViewDefinition;
#[cfg(feature = "postgres")]
use crate::{Error, Result};

/// Executes ViewDefinitions against a PostgreSQL database.
#[cfg(feature = "postgres")]
pub struct ViewRunner {
    pool: PgPool,
}

#[cfg(feature = "postgres")]
impl ViewRunner {
    /// Create a new ViewRunner with the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Execute a ViewDefinition and return the results.
    ///
    /// # Errors
    ///
    /// Returns an error if SQL generation fails or the query fails to execute.
    pub async fn run(&self, view: &ViewDefinition) -> Result<ViewResult> {
        let generated = SqlGenerator::new().generate(view)?;

        tracing::debug!(sql = %generated.sql, "Executing view");

        let rows = sqlx_core::query::query(AssertSqlSafe(generated.sql.clone()))
            .fetch_all(&self.pool)
            .await
            .map_err(Error::Sql)?;

        let columns: Vec<ColumnInfo> = generated
            .columns
            .iter()
            .map(|c| ColumnInfo::new(c.alias.clone(), c.col_type))
            .collect();

        let data: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| extract_row_values(row, &generated.columns))
            .collect();

        Ok(ViewResult { columns, data })
    }
}

/// Extract values from a row based on column definitions.
#[cfg(feature = "postgres")]
fn extract_row_values(row: &PgRow, columns: &[GeneratedColumn]) -> Vec<Value> {
    columns
        .iter()
        .enumerate()
        .map(|(i, col)| extract_column_value(row, i, &col.col_type))
        .collect()
}

/// Extract a single column value from a row.
#[cfg(feature = "postgres")]
fn extract_column_value(row: &PgRow, index: usize, col_type: &ColumnType) -> Value {
    match col_type {
        ColumnType::Integer | ColumnType::Integer64 => row
            .try_get::<Option<i64>, _>(index)
            .ok()
            .flatten()
            .map(|v| Value::Number(v.into()))
            .unwrap_or(Value::Null),

        ColumnType::Decimal => row
            .try_get::<Option<f64>, _>(index)
            .ok()
            .flatten()
            .and_then(|v| serde_json::Number::from_f64(v).map(Value::Number))
            .unwrap_or(Value::Null),

        ColumnType::Boolean => row
            .try_get::<Option<bool>, _>(index)
            .ok()
            .flatten()
            .map(Value::Bool)
            .unwrap_or(Value::Null),

        ColumnType::Json => row
            .try_get::<Option<Value>, _>(index)
            .ok()
            .flatten()
            .unwrap_or(Value::Null),

        // String-like types
        ColumnType::String
        | ColumnType::Date
        | ColumnType::DateTime
        | ColumnType::Instant
        | ColumnType::Time
        | ColumnType::Base64Binary => row
            .try_get::<Option<String>, _>(index)
            .ok()
            .flatten()
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

/// Results from executing a ViewDefinition.
#[derive(Debug, Clone)]
pub struct ViewResult {
    /// Column metadata for the result set.
    pub columns: Vec<ColumnInfo>,

    /// Row data as JSON values, each row's values in column order.
    pub data: Vec<Vec<Value>>,
}

impl ViewResult {
    /// Check if the result set is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Convert the result to a JSON array of objects.
    pub fn to_json_array(&self) -> Vec<Value> {
        self.data
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (i, col) in self.columns.iter().enumerate() {
                    if let Some(value) = row.get(i) {
                        obj.insert(col.name.clone(), value.clone());
                    }
                }
                Value::Object(obj)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_view_result_to_json_array() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("name", ColumnType::String),
            ],
            data: vec![
                vec![json!("1"), json!("Alice")],
                vec![json!("2"), json!("Bob")],
            ],
        };

        let json_array = result.to_json_array();
        assert_eq!(json_array.len(), 2);
        assert_eq!(json_array[0]["id"], "1");
        assert_eq!(json_array[0]["name"], "Alice");
        assert_eq!(json_array[1]["id"], "2");
        assert_eq!(json_array[1]["name"], "Bob");
    }

    #[test]
    fn test_view_result_is_empty() {
        let empty_result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![],
        };
        assert!(empty_result.is_empty());

        let non_empty = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![vec![json!("1")]],
        };
        assert!(!non_empty.is_empty());
    }
}
