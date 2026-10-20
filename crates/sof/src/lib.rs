//! SQL on FHIR v2 for fhir-pipes: ViewDefinition parsing, a database-free
//! in-memory evaluator, SQL generation, and writers for the rows.
//!
//! This crate came from [octofhir/sof](https://github.com/octofhir/sof) and
//! keeps only what fhir-pipes uses.
//!
//! # Components
//!
//! - [`ViewDefinition`]: a parsed ViewDefinition resource
//!   ([`ViewDefinition::from_json`]).
//! - [`CompiledView`]: a view compiled once and run over in-memory FHIR
//!   resources (`serde_json::Value`), one resource at a time
//!   ([`CompiledView::execute_resource`]) or on several threads
//!   ([`CompiledView::execute_parallel`]). [`execute`], [`execute_blocking`]
//!   and [`execute_parallel`] compile and run in one call.
//! - [`SqlGenerator`]: generates a PostgreSQL or DuckDB query over FHIR
//!   resources stored as JSON, and [`create_table`] the matching
//!   `CREATE TABLE`.
//! - [`ViewRunner`] (the `postgres` feature): runs the generated SQL against
//!   PostgreSQL.
//! - [`ViewResult`]: the columns and rows of a run, which the writers in
//!   [`output`] write as CSV, NDJSON, JSON or (the `parquet` feature)
//!   Parquet.
//!
//! # Example
//!
//! ```no_run
//! use fhir_pipes_sof::output::{OutputWriter, get_writer};
//! use fhir_pipes_sof::{CompiledView, SqlGenerator, ViewDefinition};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let json = serde_json::json!({});
//! # let resources: Vec<serde_json::Value> = Vec::new();
//! let view = ViewDefinition::from_json(&json)?;
//!
//! // In memory, on four threads.
//! let result = CompiledView::compile_blocking(&view)?.execute_parallel(&resources, 4)?;
//! get_writer("csv")?.write(&result, &mut std::io::stdout())?;
//!
//! // Or as SQL.
//! let sql = SqlGenerator::new().generate(&view)?.sql;
//! # Ok(())
//! # }
//! ```
//!
//! # SQL on FHIR Specification
//!
//! See: <https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/>

mod choice_elements;
mod column;
mod eval;
mod fhir_model;
mod lexer;
pub mod output;
mod runner;
mod sql_generator;
mod view_definition;

pub use column::{ColumnInfo, ColumnType};
pub use eval::{CompiledView, calls_any, execute, execute_blocking, execute_parallel};
pub use runner::ViewResult;
#[cfg(feature = "postgres")]
pub use runner::ViewRunner;
pub use sql_generator::{Dialect, GeneratedColumn, GeneratedSql, SqlGenerator, create_table};
pub use view_definition::{Column, Constant, SelectColumn, ViewDefinition, WhereClause};

use thiserror::Error;

/// Errors that can occur during SQL on FHIR operations.
#[derive(Debug, Error)]
pub enum Error {
    /// The ViewDefinition JSON is invalid or missing required fields.
    #[error("Invalid ViewDefinition: {0}")]
    InvalidViewDefinition(String),

    /// A FHIRPath expression is invalid or cannot be converted to SQL.
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    /// An error occurred while executing SQL.
    #[cfg(feature = "postgres")]
    #[error("SQL execution error: {0}")]
    Sql(#[from] sqlx_core::error::Error),

    /// An error occurred during FHIRPath evaluation or compilation.
    #[error("FHIRPath error: {0}")]
    FhirPath(String),

    /// An error occurred while generating output.
    #[error("Output error: {0}")]
    Output(String),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type alias using the crate's Error type.
pub type Result<T> = std::result::Result<T, Error>;
