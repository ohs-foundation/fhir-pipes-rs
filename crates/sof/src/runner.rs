//! View results, and view execution against PostgreSQL.
//!
//! [`ViewResult`] holds the rows of a view, from the in-memory evaluator or
//! from [`ViewRunner`], which runs the generated SQL against a PostgreSQL
//! database (the `postgres` feature).

use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx_core::executor::Executor;
#[cfg(feature = "postgres")]
use sqlx_core::sql_str::AssertSqlSafe;
#[cfg(feature = "postgres")]
use sqlx_postgres::{PgPool, Postgres};

use crate::column::ColumnInfo;
#[cfg(test)]
use crate::column::ColumnType;
#[cfg(feature = "postgres")]
use crate::sql_generator::SqlGenerator;
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
        run_view(&self.pool, view).await
    }
}

/// Execute a ViewDefinition on `executor`: a pool, or one connection.
///
/// Each row comes back as one JSON object (`to_jsonb`), so every column
/// keeps the value PostgreSQL computed: a `numeric` decimal, a `bigint`, a
/// JSON collection. Decoding each column into a Rust type instead would
/// need a type for each SQL type the generator can produce.
///
/// # Errors
///
/// Returns an error if SQL generation fails or the query fails to execute.
#[cfg(feature = "postgres")]
pub async fn run_view<'e, E>(executor: E, view: &ViewDefinition) -> Result<ViewResult>
where
    E: Executor<'e, Database = Postgres>,
{
    let generated = SqlGenerator::new().generate(view)?;

    tracing::debug!(sql = %generated.sql, "Executing view");

    let wrapped = format!("SELECT to_jsonb(t) FROM ({}) t", generated.sql);
    let rows: Vec<(Value,)> = sqlx_core::query_as::query_as(AssertSqlSafe(wrapped))
        .fetch_all(executor)
        .await
        .map_err(Error::Sql)?;

    let columns: Vec<ColumnInfo> = generated
        .columns
        .iter()
        .map(|c| ColumnInfo::new(c.alias.clone(), c.col_type))
        .collect();

    let data: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|(mut row,)| {
            columns
                .iter()
                .map(|c| row.get_mut(&c.name).map(Value::take).unwrap_or(Value::Null))
                .collect()
        })
        .collect();

    Ok(ViewResult { columns, data })
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
}
