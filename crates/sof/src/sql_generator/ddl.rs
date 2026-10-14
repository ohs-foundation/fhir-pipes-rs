//! `CREATE TABLE` (DDL) generation for the columns a ViewDefinition produces.

use std::fmt;
use std::str::FromStr;

use crate::column::ColumnType;

use super::GeneratedColumn;

/// SQL dialect for DDL type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// ISO/IEC 9075 ANSI SQL — the SQL-on-FHIR v2 default type mapping.
    #[default]
    Ansi,
    /// PostgreSQL types (TEXT, JSONB, TIMESTAMPTZ, …).
    Postgres,
    /// DuckDB types (VARCHAR, JSON, TIMESTAMP, …).
    DuckDb,
}

impl FromStr for Dialect {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "ansi" | "sql" => Ok(Self::Ansi),
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            "duckdb" | "duck" => Ok(Self::DuckDb),
            other => Err(format!(
                "unknown dialect `{other}` (expected ansi, postgres or duckdb)"
            )),
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ansi => "ansi",
            Self::Postgres => "postgres",
            Self::DuckDb => "duckdb",
        })
    }
}

impl Dialect {
    /// The dialect's type name for a column type.
    fn type_name(&self, ty: ColumnType) -> &'static str {
        match self {
            Self::Ansi => ty.ansi_type(),
            Self::Postgres => ty.sql_type(),
            Self::DuckDb => match ty {
                ColumnType::String => "VARCHAR",
                ColumnType::Integer => "INTEGER",
                ColumnType::Integer64 => "BIGINT",
                ColumnType::Decimal => "DOUBLE",
                ColumnType::Boolean => "BOOLEAN",
                ColumnType::Date => "DATE",
                ColumnType::DateTime | ColumnType::Instant => "TIMESTAMP",
                ColumnType::Time => "TIME",
                ColumnType::Base64Binary => "BLOB",
                ColumnType::Json => "JSON",
            },
        }
    }
}

/// Render a `CREATE TABLE` statement for the given columns.
pub fn create_table(table: &str, columns: &[GeneratedColumn], dialect: Dialect) -> String {
    let ident = quote_ident(table);
    if columns.is_empty() {
        return format!("CREATE TABLE {ident} ();\n");
    }
    let mut out = format!("CREATE TABLE {ident} (\n");
    for (i, col) in columns.iter().enumerate() {
        let comma = if i + 1 < columns.len() { "," } else { "" };
        out.push_str(&format!(
            "  {} {}{}\n",
            quote_ident(&col.name),
            dialect.type_name(col.col_type),
            comma
        ));
    }
    out.push_str(");\n");
    out
}

/// Double-quote a SQL identifier, escaping embedded quotes.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<GeneratedColumn> {
        vec![
            GeneratedColumn {
                name: "id".into(),
                expression: String::new(),
                alias: "id".into(),
                col_type: ColumnType::String,
            },
            GeneratedColumn {
                name: "age".into(),
                expression: String::new(),
                alias: "age".into(),
                col_type: ColumnType::Integer,
            },
            GeneratedColumn {
                name: "born".into(),
                expression: String::new(),
                alias: "born".into(),
                col_type: ColumnType::Date,
            },
        ]
    }

    #[test]
    fn ansi_ddl_uses_character_varying() {
        let sql = create_table("patient_view", &cols(), Dialect::Ansi);
        assert!(sql.contains("CREATE TABLE \"patient_view\" ("));
        assert!(sql.contains("\"id\" CHARACTER VARYING,"));
        assert!(sql.contains("\"age\" INT,"));
        assert!(sql.contains("\"born\" CHARACTER VARYING\n"));
        assert!(sql.trim_end().ends_with(");"));
    }

    #[test]
    fn postgres_ddl_uses_native_types() {
        let sql = create_table("v", &cols(), Dialect::Postgres);
        assert!(sql.contains("\"id\" TEXT,"));
        assert!(sql.contains("\"age\" INTEGER,"));
        assert!(sql.contains("\"born\" DATE\n"));
    }

    #[test]
    fn dialect_parses() {
        assert_eq!("pg".parse::<Dialect>().unwrap(), Dialect::Postgres);
        assert_eq!("DuckDB".parse::<Dialect>().unwrap(), Dialect::DuckDb);
        assert!("oracle".parse::<Dialect>().is_err());
    }
}
