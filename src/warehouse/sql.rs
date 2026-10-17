//! Quoting for the SQL the warehouse writes: DuckDB catalog views, the tiles
//! step's DuckDB queries and the PostgreSQL load.

use std::path::Path;

/// A quoted SQL identifier, so a name that is a keyword (`group`) works.
/// Queries can still name it unquoted where the database allows.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A SQL string literal.
pub fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A SQL string literal for a path.
pub fn sql_path(p: &Path) -> String {
    sql_str(&p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_strings_escape_quotes() {
        assert_eq!(sql_path(Path::new("/a/o'b")), "'/a/o''b'");
    }
}
