//! Column type definitions for SQL on FHIR views.
//!
//! This module defines the types used to represent column information
//! in the generated SQL and result sets.

use serde::{Deserialize, Serialize};

/// Information about a column in a view result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// The column name.
    pub name: String,

    /// The column's data type.
    pub col_type: ColumnType,

    /// Whether this column can contain null values.
    pub nullable: bool,

    /// Human-readable description of the column.
    pub description: Option<String>,
}

impl ColumnInfo {
    /// Create a new column info with default settings.
    pub fn new(name: impl Into<String>, col_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            col_type,
            nullable: true,
            description: None,
        }
    }

    /// Set whether this column is nullable.
    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    /// Set the column description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Data types supported by SQL on FHIR columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    /// String/text values.
    #[default]
    String,

    /// Integer values (FHIR `integer`, `positiveInt`, `unsignedInt`).
    Integer,

    /// 64-bit integer values (FHIR `integer64`).
    Integer64,

    /// Decimal/floating-point values.
    Decimal,

    /// Boolean values.
    Boolean,

    /// Date values (YYYY-MM-DD).
    Date,

    /// DateTime values (ISO 8601).
    DateTime,

    /// Instant values (precise timestamp).
    Instant,

    /// Time values (HH:MM:SS).
    Time,

    /// Base64 encoded binary data.
    Base64Binary,

    /// JSON/complex object (when collection=true or complex type).
    Json,
}

impl ColumnType {
    /// Parse a column type from a string.
    ///
    /// Returns `String` as the default type for unknown values.
    pub fn from_fhir_type(type_str: &str) -> Self {
        match type_str.to_lowercase().as_str() {
            "string" | "code" | "uri" | "url" | "canonical" | "id" | "oid" | "uuid"
            | "markdown" => Self::String,
            "integer" | "positiveint" | "unsignedint" => Self::Integer,
            "integer64" => Self::Integer64,
            "decimal" => Self::Decimal,
            "boolean" => Self::Boolean,
            "date" => Self::Date,
            "datetime" => Self::DateTime,
            "instant" => Self::Instant,
            "time" => Self::Time,
            "base64binary" => Self::Base64Binary,
            _ => Self::String, // Default to string for unknown types
        }
    }

    /// Parse a column type from an ANSI SQL type name, as supplied by an
    /// `ansi/type` column tag overriding the inferred type. Unknown names fall
    /// back to `String`.
    pub fn from_ansi_type(type_str: &str) -> Self {
        let t = type_str.trim().to_lowercase();
        match t.as_str() {
            "int" | "integer" | "smallint" => Self::Integer,
            "bigint" => Self::Integer64,
            "decimal" | "numeric" | "real" | "double precision" | "float" => Self::Decimal,
            "boolean" | "bool" => Self::Boolean,
            "timestamp with time zone" | "timestamptz" => Self::Instant,
            "binary" | "varbinary" | "bytea" => Self::Base64Binary,
            "json" | "jsonb" => Self::Json,
            _ => Self::String,
        }
    }

    /// Get the PostgreSQL type name for this column type. Used for the runtime
    /// value cast/decode; this is a Postgres dialect, not the ANSI default.
    pub fn sql_type(&self) -> &'static str {
        match self {
            Self::String => "TEXT",
            Self::Integer => "INTEGER",
            Self::Integer64 => "BIGINT",
            Self::Decimal => "NUMERIC",
            Self::Boolean => "BOOLEAN",
            Self::Date => "DATE",
            Self::DateTime | Self::Instant => "TIMESTAMPTZ",
            Self::Time => "TIME",
            Self::Base64Binary => "BYTEA",
            Self::Json => "JSONB",
        }
    }

    /// The default ANSI SQL (ISO/IEC 9075) type per the SQL-on-FHIR v2 spec's
    /// type-mapping table. Temporal and decimal types map to `CHARACTER VARYING`
    /// to preserve the FHIR string representation; `instant` keeps timezone.
    /// <https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/StructureDefinition-ViewDefinition-notes.html>
    pub fn ansi_type(&self) -> &'static str {
        match self {
            // string, code, uri, url, canonical, id, oid, uuid, markdown,
            // date, dateTime, decimal, time
            Self::String | Self::Decimal | Self::Date | Self::DateTime | Self::Time => {
                "CHARACTER VARYING"
            }
            Self::Integer => "INT",
            Self::Integer64 => "BIGINT",
            Self::Boolean => "BOOLEAN",
            Self::Instant => "TIMESTAMP WITH TIME ZONE",
            Self::Base64Binary => "BINARY",
            // JSON has no ANSI equivalent; collection columns are emitted as JSON.
            Self::Json => "JSON",
        }
    }

    /// Get the default value representation for null values.
    pub fn null_representation(&self) -> &'static str {
        "NULL"
    }
}

impl std::fmt::Display for ColumnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String => write!(f, "string"),
            Self::Integer => write!(f, "integer"),
            Self::Integer64 => write!(f, "integer64"),
            Self::Decimal => write!(f, "decimal"),
            Self::Boolean => write!(f, "boolean"),
            Self::Date => write!(f, "date"),
            Self::DateTime => write!(f, "dateTime"),
            Self::Instant => write!(f, "instant"),
            Self::Time => write!(f, "time"),
            Self::Base64Binary => write!(f, "base64Binary"),
            Self::Json => write!(f, "json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_type_from_fhir_type() {
        assert_eq!(ColumnType::from_fhir_type("string"), ColumnType::String);
        assert_eq!(ColumnType::from_fhir_type("code"), ColumnType::String);
        assert_eq!(ColumnType::from_fhir_type("integer"), ColumnType::Integer);
        assert_eq!(
            ColumnType::from_fhir_type("positiveInt"),
            ColumnType::Integer
        );
        assert_eq!(ColumnType::from_fhir_type("decimal"), ColumnType::Decimal);
        assert_eq!(ColumnType::from_fhir_type("boolean"), ColumnType::Boolean);
        assert_eq!(ColumnType::from_fhir_type("date"), ColumnType::Date);
        assert_eq!(ColumnType::from_fhir_type("dateTime"), ColumnType::DateTime);
        assert_eq!(ColumnType::from_fhir_type("instant"), ColumnType::Instant);
        assert_eq!(ColumnType::from_fhir_type("time"), ColumnType::Time);
        assert_eq!(
            ColumnType::from_fhir_type("base64Binary"),
            ColumnType::Base64Binary
        );
        // Unknown types default to string
        assert_eq!(
            ColumnType::from_fhir_type("UnknownType"),
            ColumnType::String
        );
    }

    #[test]
    fn test_column_type_sql_type() {
        assert_eq!(ColumnType::String.sql_type(), "TEXT");
        assert_eq!(ColumnType::Integer.sql_type(), "INTEGER");
        assert_eq!(ColumnType::Integer64.sql_type(), "BIGINT");
        assert_eq!(ColumnType::Decimal.sql_type(), "NUMERIC");
        assert_eq!(ColumnType::Boolean.sql_type(), "BOOLEAN");
        assert_eq!(ColumnType::Date.sql_type(), "DATE");
        assert_eq!(ColumnType::DateTime.sql_type(), "TIMESTAMPTZ");
        assert_eq!(ColumnType::Json.sql_type(), "JSONB");
    }

    #[test]
    fn test_column_type_ansi_type() {
        // Spec default ANSI mapping: temporal/decimal stay CHARACTER VARYING.
        assert_eq!(ColumnType::String.ansi_type(), "CHARACTER VARYING");
        assert_eq!(ColumnType::Decimal.ansi_type(), "CHARACTER VARYING");
        assert_eq!(ColumnType::Date.ansi_type(), "CHARACTER VARYING");
        assert_eq!(ColumnType::DateTime.ansi_type(), "CHARACTER VARYING");
        assert_eq!(ColumnType::Time.ansi_type(), "CHARACTER VARYING");
        assert_eq!(ColumnType::Integer.ansi_type(), "INT");
        assert_eq!(ColumnType::Integer64.ansi_type(), "BIGINT");
        assert_eq!(ColumnType::Boolean.ansi_type(), "BOOLEAN");
        assert_eq!(ColumnType::Instant.ansi_type(), "TIMESTAMP WITH TIME ZONE");
        assert_eq!(ColumnType::Base64Binary.ansi_type(), "BINARY");
    }

    #[test]
    fn test_integer64_maps_to_bigint() {
        assert_eq!(
            ColumnType::from_fhir_type("integer64"),
            ColumnType::Integer64
        );
        assert_eq!(ColumnType::from_fhir_type("integer"), ColumnType::Integer);
        assert_eq!(ColumnType::from_ansi_type("BIGINT"), ColumnType::Integer64);
        assert_eq!(ColumnType::from_ansi_type("INTEGER"), ColumnType::Integer);
    }

    #[test]
    fn test_column_info_builder() {
        let col = ColumnInfo::new("test_col", ColumnType::String)
            .with_nullable(false)
            .with_description("A test column");

        assert_eq!(col.name, "test_col");
        assert_eq!(col.col_type, ColumnType::String);
        assert!(!col.nullable);
        assert_eq!(col.description, Some("A test column".to_string()));
    }
}
