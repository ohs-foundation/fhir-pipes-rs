//! The parameters a SQLQuery Library declares (`Library.parameter`) and the
//! `:name` placeholders its SQL reads them through.

use std::borrow::Cow;
use std::collections::BTreeSet;

use anyhow::{Result, bail};

use super::Library;
use super::compose::bind_placeholders;

/// An `in` parameter of a Library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    /// Its FHIR type, `string` when the Library gives none.
    pub fhir_type: String,
}

/// The `in` parameters `lib` declares, in its order.
pub fn declared(lib: &Library) -> Vec<Parameter> {
    lib.json["parameter"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["use"].as_str().is_none_or(|u| u == "in"))
        .filter_map(|p| {
            Some(Parameter {
                name: p["name"].as_str()?.to_string(),
                fhir_type: p["type"].as_str().unwrap_or("string").to_string(),
            })
        })
        .collect()
}

/// Whether `name` can be written as a `:name` placeholder.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The duckdb type a parameter of FHIR type `fhir` is cast to, or `None`
/// for a type that has none here.
pub fn duckdb_type(fhir: &str) -> Option<&'static str> {
    Some(match fhir {
        "string" | "code" | "id" | "uri" | "url" | "canonical" | "markdown" | "oid" | "uuid" => {
            "VARCHAR"
        }
        "integer" | "positiveInt" | "unsignedInt" | "integer64" => "BIGINT",
        "decimal" => "DOUBLE",
        "boolean" => "BOOLEAN",
        "date" => "DATE",
        "dateTime" | "instant" => "TIMESTAMPTZ",
        "time" => "TIME",
        _ => return None,
    })
}

/// What a value of duckdb type `sql_type` must look like, when `text` does
/// not: the text is cast to the type by duckdb, so one that would fail
/// there is refused here, with words for the caller.
pub fn check_value(sql_type: &str, text: &str) -> Result<(), &'static str> {
    // A value travels in an environment variable, which holds no NUL.
    if text.contains('\0') {
        return Err("a value without a NUL character");
    }
    let ok = match sql_type {
        "BIGINT" => text.parse::<i64>().is_ok(),
        "DOUBLE" => {
            text.parse::<f64>().is_ok_and(f64::is_finite)
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        }
        "BOOLEAN" => matches!(text, "true" | "false"),
        "DATE" => is_date(text),
        "TIMESTAMPTZ" => is_date(text) || chrono::DateTime::parse_from_rfc3339(text).is_ok(),
        "TIME" => chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f").is_ok(),
        _ => true,
    };
    if ok {
        return Ok(());
    }
    Err(match sql_type {
        "BIGINT" => "an integer",
        "DOUBLE" => "a number",
        "BOOLEAN" => "true or false",
        "DATE" => "a date, as YYYY-MM-DD",
        "TIMESTAMPTZ" => {
            "a date (YYYY-MM-DD) or a date and time with its offset (2026-01-31T12:00:00Z)"
        }
        _ => "a time, as HH:MM:SS",
    })
}

fn is_date(text: &str) -> bool {
    text.len() == 10 && chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
}

/// `lib` with each placeholder of a parameter in `params` replaced by
/// `to(parameter)`, in every content entry.
pub fn bound(lib: &Library, params: &[Parameter], to: impl Fn(&Parameter) -> String) -> Library {
    let names: BTreeSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut copy = lib.clone();
    for (_, sql) in &mut copy.content {
        *sql = bind_placeholders(sql, &names, |n| {
            params
                .iter()
                .find(|p| p.name == n)
                .map(&to)
                .unwrap_or_else(|| format!(":{n}"))
        });
    }
    copy
}

/// `lib` with no parameter given: each declared parameter's placeholders
/// become a NULL of its type. This is how a refresh, which has no caller to
/// give values, runs a Library that declares parameters.
pub fn unset(lib: &Library) -> Result<Cow<'_, Library>> {
    let params = declared(lib);
    if params.is_empty() {
        return Ok(Cow::Borrowed(lib));
    }
    for p in &params {
        if duckdb_type(&p.fhir_type).is_none() {
            bail!(
                "Library {}: the parameter {} has type {}, which cannot be bound; use string, \
                 code, integer, decimal, boolean, date, dateTime or time",
                lib.name,
                p.name,
                p.fhir_type
            );
        }
    }
    Ok(Cow::Owned(bound(lib, &params, |p| {
        format!(
            "CAST(NULL AS {})",
            duckdb_type(&p.fhir_type).unwrap_or("VARCHAR")
        )
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    fn lib(sql: &str, parameter: serde_json::Value) -> Library {
        Library::from_json(&json!({
            "resourceType": "Library",
            "meta": {"profile": [crate::library::SQLQUERY_PROFILE]},
            "url": "https://example.org/Library/P",
            "name": "P",
            "status": "active",
            "parameter": parameter,
            "content": [{"contentType": "application/sql", "data": STANDARD.encode(sql)}],
        }))
        .unwrap()
    }

    #[test]
    fn declared_keeps_in_parameters_with_their_types() {
        let l = lib(
            "SELECT 1",
            json!([
                {"name": "year", "use": "in", "type": "integer"},
                {"name": "state"},
                {"name": "rows", "use": "out", "type": "integer"},
            ]),
        );
        let got: Vec<(String, String)> = declared(&l)
            .into_iter()
            .map(|p| (p.name, p.fhir_type))
            .collect();
        assert_eq!(
            got,
            [
                ("year".to_string(), "integer".to_string()),
                ("state".to_string(), "string".to_string())
            ]
        );
    }

    #[test]
    fn unset_makes_typed_nulls_and_leaves_other_placeholders() {
        let l = lib(
            "SELECT ':year' AS s, x::INT FROM t WHERE (:year IS NULL OR y = :year) AND s = :state AND z = :other",
            json!([
                {"name": "year", "use": "in", "type": "integer"},
                {"name": "state", "use": "in", "type": "string"},
            ]),
        );
        let u = unset(&l).unwrap();
        assert_eq!(
            u.content[0].1,
            "SELECT ':year' AS s, x::INT FROM t WHERE (CAST(NULL AS BIGINT) IS NULL OR \
             y = CAST(NULL AS BIGINT)) AND s = CAST(NULL AS VARCHAR) AND z = :other"
        );
        // A Library with no parameters is not copied.
        let plain = lib("SELECT :x", json!([]));
        assert!(matches!(unset(&plain).unwrap(), Cow::Borrowed(_)));
    }

    #[test]
    fn unset_refuses_a_type_it_cannot_bind() {
        let l = lib(
            "SELECT :q",
            json!([{"name": "q", "use": "in", "type": "Quantity"}]),
        );
        let err = unset(&l).unwrap_err().to_string();
        assert!(err.contains("parameter q has type Quantity"), "{err}");
    }

    #[test]
    fn names() {
        for ok in ["year", "_x", "a1", "A_b"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "1a", "a-b", "a b", "é", "a'"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn values_are_checked_by_type() {
        for (ty, ok, bad) in [
            (
                "BIGINT",
                vec!["0", "-12", "2024"],
                vec!["", "1.5", "abc", "1e3", "99999999999999999999"],
            ),
            (
                "DOUBLE",
                vec!["1", "-1.5", "2e3"],
                vec!["", "nan", "inf", "1,5", "abc"],
            ),
            (
                "BOOLEAN",
                vec!["true", "false"],
                vec!["", "1", "TRUE", "yes"],
            ),
            (
                "DATE",
                vec!["2024-02-29"],
                vec!["", "2023-02-29", "2024-2-9", "2024", "2024-01-01T00:00:00Z"],
            ),
            (
                "TIMESTAMPTZ",
                vec![
                    "2024-01-31",
                    "2024-01-31T12:00:00Z",
                    "2024-01-31T12:00:00.5+03:00",
                ],
                vec!["", "2024-01-31T12:00:00", "yesterday"],
            ),
            (
                "TIME",
                vec!["09:30:00", "09:30:00.250"],
                vec!["", "9:30", "25:00:00"],
            ),
            (
                "VARCHAR",
                vec!["", "O'Brien; DROP TABLE x", "é"],
                vec!["a\0b"],
            ),
        ] {
            for v in ok {
                assert!(check_value(ty, v).is_ok(), "{ty} {v:?}");
            }
            for v in bad {
                assert!(check_value(ty, v).is_err(), "{ty} {v:?}");
            }
        }
    }
}
