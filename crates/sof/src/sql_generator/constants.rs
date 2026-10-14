//! Constant (`%name`) substitution and literal rendering.

use std::collections::HashMap;

use serde_json::Value;

use crate::Error;
use crate::view_definition::{Constant, ViewDefinition};

/// Replace `%name` constant references with their FHIRPath literal text.
/// `%rowIndex` is preserved for the evaluator to resolve. Errors on a reference
/// to an undefined constant.
pub(crate) fn substitute_constants(
    path: &str,
    constants: &HashMap<String, String>,
) -> Result<String, Error> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            if nc.is_ascii_alphanumeric() || nc == '_' {
                name.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('%');
            continue;
        }
        if name == "rowIndex" {
            out.push('%');
            out.push_str(&name);
            continue;
        }
        match constants.get(&name) {
            Some(lit) => out.push_str(lit),
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
