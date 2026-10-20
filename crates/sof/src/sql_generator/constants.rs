//! Constant (`%name`) substitution and literal rendering.

use std::collections::HashMap;

use serde_json::Value;

use crate::Error;
use crate::lexer::{Kind, tokens, variable_name};
use crate::view_definition::{Constant, ViewDefinition};

/// Replace `%name` constant references with their FHIRPath literal text.
/// `%rowIndex` is preserved for the evaluator to resolve. Errors on a reference
/// to an undefined constant.
pub(crate) fn substitute_constants(
    path: &str,
    constants: &HashMap<String, String>,
) -> Result<String, Error> {
    substitute_constants_keeping(path, constants, |_| false)
}

/// [`substitute_constants`], also leaving in place each `%name` that is not a
/// constant but for which `keep(name)` holds, for the FHIRPath engine to
/// resolve (environment variables such as `%resource`, or names bound by
/// `defineVariable()`). A constant takes precedence over a kept name.
///
/// The path is tokenized first, so a `%` inside a string literal, a
/// backtick-quoted name or a comment is left alone.
pub(crate) fn substitute_constants_keeping(
    path: &str,
    constants: &HashMap<String, String>,
    keep: impl Fn(&str) -> bool,
) -> Result<String, Error> {
    let mut out = String::with_capacity(path.len());
    for token in tokens(path) {
        let text = &path[token.span];
        if token.kind != Kind::Variable || !text.starts_with('%') {
            out.push_str(text);
            continue;
        }
        let name = variable_name(text);
        match constants.get(name) {
            Some(lit) => out.push_str(lit),
            None if name == "rowIndex" || keep(name) => out.push_str(text),
            None => {
                return Err(Error::InvalidViewDefinition(format!(
                    "undefined constant %{name}"
                )));
            }
        }
    }
    Ok(out)
}

/// Render each constant as a FHIRPath literal for substitution into selectors.
pub(crate) fn build_constants(view: &ViewDefinition) -> Result<HashMap<String, String>, Error> {
    let mut map = HashMap::new();
    for c in &view.constant {
        map.insert(c.name.clone(), constant_literal(c)?);
    }
    Ok(map)
}

fn constant_literal(c: &Constant) -> Result<String, Error> {
    if let Some(s) = &c.value_string {
        return Ok(fhirpath_string(s));
    }
    if let Some(i) = c.value_integer {
        return Ok(i.to_string());
    }
    if let Some(b) = c.value_boolean {
        return Ok(b.to_string());
    }
    if let Some(d) = c.value_decimal {
        // `1` would be an integer, and f64 prints 1.0 as `1`.
        return Ok(if d.is_finite() && d.fract() == 0.0 {
            format!("{d:.1}")
        } else {
            d.to_string()
        });
    }
    // Polymorphic value[x] captured via flatten.
    for (k, v) in &c.values {
        if !k.starts_with("value") {
            continue;
        }
        return match v {
            // A date, dateTime, instant or time is a FHIRPath temporal
            // literal, so it compares as one, not as text.
            Value::String(s) if TEMPORAL.contains(&k.as_str()) => temporal_literal(k, s, &c.name),
            Value::String(s) => Ok(fhirpath_string(s)),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err(Error::InvalidViewDefinition(format!(
                "unsupported constant value type for {}",
                c.name
            ))),
        };
    }
    Err(Error::InvalidViewDefinition(format!(
        "constant {} has no value",
        c.name
    )))
}

const TEMPORAL: [&str; 4] = ["valueDate", "valueDateTime", "valueInstant", "valueTime"];

/// `@2020-01-01`, `@2020-01-01T10:00:00Z`, `@T10:00`. The literal is not
/// quoted, so only the characters these forms use are accepted.
fn temporal_literal(key: &str, s: &str, name: &str) -> Result<String, Error> {
    let shaped = s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b"-:.+TZ".contains(&b));
    if !shaped {
        return Err(Error::InvalidViewDefinition(format!(
            "constant {name}: {key} {s:?} is not a FHIR {}",
            &key["value".len()..]
        )));
    }
    Ok(if key == "valueTime" {
        format!("@T{s}")
    } else {
        format!("@{s}")
    })
}

/// A FHIRPath single-quoted string literal (backslash-escaped).
fn fhirpath_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constants() -> HashMap<String, String> {
        HashMap::from([("off".to_string(), "'x'".to_string())])
    }

    #[test]
    fn substitution_skips_literals_quoted_names_and_comments() {
        let path = "name.where(family = '50%off' or `a%off` = %off) // %off";
        assert_eq!(
            substitute_constants(path, &constants()).unwrap(),
            "name.where(family = '50%off' or `a%off` = 'x') // %off"
        );
        assert_eq!(
            substitute_constants("\"%off\" | %`off` | %rowIndex", &constants()).unwrap(),
            "\"%off\" | 'x' | %rowIndex"
        );
    }

    #[test]
    fn constants_keep_their_type() {
        let view = ViewDefinition::from_json(&serde_json::json!({
            "resource": "Patient",
            "constant": [
                {"name": "d", "valueDecimal": 1.0},
                {"name": "half", "valueDecimal": 0.5},
                {"name": "day", "valueDate": "2020-01-01"},
                {"name": "at", "valueDateTime": "2020-01-01T10:00:00Z"},
                {"name": "t", "valueTime": "10:00"},
                {"name": "code", "valueCode": "male"}
            ],
            "select": [{"column": [{"name": "id", "path": "id"}]}]
        }))
        .unwrap();
        let c = build_constants(&view).unwrap();
        assert_eq!(c["d"], "1.0");
        assert_eq!(c["half"], "0.5");
        assert_eq!(c["day"], "@2020-01-01");
        assert_eq!(c["at"], "@2020-01-01T10:00:00Z");
        assert_eq!(c["t"], "@T10:00");
        assert_eq!(c["code"], "'male'");
        let bad = ViewDefinition::from_json(&serde_json::json!({
            "resource": "Patient",
            "constant": [{"name": "day", "valueDate": "2020') or true or ('"}],
            "select": [{"column": [{"name": "id", "path": "id"}]}]
        }))
        .unwrap();
        assert!(build_constants(&bad).is_err());
    }

    #[test]
    fn undefined_names_fail_unless_kept() {
        let err = substitute_constants("%resource.id", &constants()).unwrap_err();
        assert!(
            err.to_string().contains("undefined constant %resource"),
            "{err}"
        );
        let kept =
            substitute_constants_keeping("%resource.id + %off", &constants(), |n| n == "resource")
                .unwrap();
        assert_eq!(kept, "%resource.id + 'x'");
        // A constant wins over a kept name.
        let kept = substitute_constants_keeping("%off", &constants(), |_| true).unwrap();
        assert_eq!(kept, "'x'");
    }
}
