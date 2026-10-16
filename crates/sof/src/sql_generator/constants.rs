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
        return Ok(d.to_string());
    }
    // Polymorphic value[x] captured via flatten.
    for (k, v) in &c.values {
        if !k.starts_with("value") {
            continue;
        }
        return match v {
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
