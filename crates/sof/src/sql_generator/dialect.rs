//! JSON-primitive SQL fragments, parametrized by [`Dialect`].
//!
//! The collection-aware lowering in [`super::lower`] and [`super::boundary`]
//! never writes a Postgres-specific JSON expression directly; instead it calls
//! these methods so the same structural SQL renders for either the PostgreSQL
//! JSONB backend or the DuckDB JSON backend.
//!
//! [`Dialect`] itself is declared in [`super::ddl`] (it also drives DDL type
//! names). The DDL-only `Ansi` variant is treated as `Postgres` here, since
//! SELECT generation is only ever requested for `Postgres` or `DuckDb`.

use super::ddl::Dialect;

impl Dialect {
    /// True for the DuckDB JSON backend.
    fn is_duckdb(self) -> bool {
        matches!(self, Dialect::DuckDb)
    }

    /// Build a one-element JSON array from a JSON-typed scalar expression,
    /// preserving the element's JSON type.
    pub(super) fn build_array1(self, x: &str) -> String {
        if self.is_duckdb() {
            format!("json_array({x})")
        } else {
            format!("jsonb_build_array({x})")
        }
    }

    /// The empty JSON array literal.
    pub(super) fn empty_array(self) -> &'static str {
        if self.is_duckdb() {
            "'[]'::JSON"
        } else {
            "'[]'::jsonb"
        }
    }

    /// Aggregate a column of JSON values into a JSON array, defaulting an empty
    /// group to the empty array.
    pub(super) fn agg(self, x: &str) -> String {
        if self.is_duckdb() {
            format!("coalesce(json_group_array({x}),'[]'::JSON)")
        } else {
            format!("coalesce(jsonb_agg({x}),'[]'::jsonb)")
        }
    }

    /// Extract a JSON scalar as text (Postgres `#>> '{}'`, DuckDB `->> '$'`).
    pub(super) fn scalar_text(self, j: &str) -> String {
        if self.is_duckdb() {
            format!("({j} ->> '$')")
        } else {
            format!("({j} #>> '{{}}')")
        }
    }

    /// Boolean SQL: the JSON value is an array.
    pub(super) fn is_array(self, j: &str) -> String {
        if self.is_duckdb() {
            format!("json_type({j}) = 'ARRAY'")
        } else {
            format!("jsonb_typeof({j}) = 'array'")
        }
    }

    /// Boolean SQL: the JSON value is a string.
    pub(super) fn is_string(self, j: &str) -> String {
        if self.is_duckdb() {
            format!("json_type({j}) = 'VARCHAR'")
        } else {
            format!("jsonb_typeof({j}) = 'string'")
        }
    }

    /// Boolean SQL: the JSON value is a number.
    pub(super) fn is_number(self, j: &str) -> String {
        if self.is_duckdb() {
            format!(
                "json_type({j}) IN ('UBIGINT','BIGINT','DOUBLE','DECIMAL','UINTEGER','INTEGER')"
            )
        } else {
            format!("jsonb_typeof({j}) = 'number'")
        }
    }

    /// Boolean SQL: the JSON value is JSON null.
    pub(super) fn is_json_null(self, j: &str) -> String {
        if self.is_duckdb() {
            format!("json_type({j}) = 'NULL'")
        } else {
            format!("jsonb_typeof({j}) = 'null'")
        }
    }

    /// Length of a JSON array.
    pub(super) fn array_length(self, j: &str) -> String {
        if self.is_duckdb() {
            format!("json_array_length({j})")
        } else {
            format!("jsonb_array_length({j})")
        }
    }

    /// Wrap a SQL scalar as a JSON value.
    pub(super) fn to_json_scalar(self, x: &str) -> String {
        if self.is_duckdb() {
            format!("to_json({x})")
        } else {
            format!("to_jsonb({x})")
        }
    }

    /// Cast keyword for numeric (decimal) values.
    pub(super) fn num_cast(self) -> &'static str {
        if self.is_duckdb() {
            "DECIMAL(38,9)"
        } else {
            "numeric"
        }
    }

    /// Cast keyword for 64-bit integers.
    pub(super) fn int_cast(self) -> &'static str {
        if self.is_duckdb() { "BIGINT" } else { "bigint" }
    }

    /// Cast keyword for booleans.
    pub(super) fn bool_cast(self) -> &'static str {
        if self.is_duckdb() {
            "BOOLEAN"
        } else {
            "boolean"
        }
    }

    /// Cast keyword for text.
    pub(super) fn text_cast(self) -> &'static str {
        if self.is_duckdb() { "VARCHAR" } else { "text" }
    }

    /// A FROM fragment that expands a JSON array `arr` into rows of column
    /// `col`, bound to table alias `alias` (referenced as `alias.col`). An
    /// empty array yields zero rows.
    pub(super) fn elements_table(self, arr: &str, alias: &str, col: &str) -> String {
        if self.is_duckdb() {
            format!("(SELECT unnest(json_extract({arr}, '$[*]')) AS {col}) AS {alias}")
        } else {
            format!("jsonb_array_elements({arr}) AS {alias}({col})")
        }
    }

    /// A FROM fragment expanding `coll` into rows of `(value, ord)` (ord is
    /// 1-based), bound to alias `alias`.
    pub(super) fn elements_ord_table(self, coll: &str, alias: &str) -> String {
        if self.is_duckdb() {
            format!(
                "(SELECT unnest(json_extract({coll},'$[*]')) AS value, \
                 unnest(range(1, ({len}+1)::BIGINT)) AS ord) AS {alias}",
                len = self.array_length(coll)
            )
        } else {
            format!("jsonb_array_elements({coll}) WITH ORDINALITY AS {alias}(value, ord)")
        }
    }
}
