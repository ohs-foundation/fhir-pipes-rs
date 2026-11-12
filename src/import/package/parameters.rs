//! Package parameters: `[parameters]` in `import.toml`, and the values a run
//! takes for them (the defaults, then `--param NAME=VALUE`, then for a test
//! case its `parameters.toml`).
//!
//! A value reaches each part of the package in the way that part already
//! takes values from outside:
//!
//! - maps: a FHIRPath environment variable `%NAME`, in every expression the
//!   map evaluates (`where`, `check`, `evaluate`, `let`);
//! - views: a ViewDefinition constant `%NAME`, added to every view the run
//!   runs;
//! - SQLQuery Libraries: a parameter the Library declares under
//!   `[[parameter]]`, written `:NAME` in its SQL, as the SQL on FHIR SQLQuery
//!   profile writes parameters. duckdb gets the value from the environment
//!   (`SET VARIABLE` from `getenv`), so the value is never part of the SQL text.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::error::{PipesError, Result};

/// The names a parameter may not take: FHIRPath environment variables the
/// engines resolve themselves, and the variables of record views and views.
pub const RESERVED: [&str; 11] = [
    "context",
    "resource",
    "rootResource",
    "ucum",
    "sct",
    "loinc",
    "factory",
    "terminologies",
    "server",
    "recordLine",
    "rowIndex",
];

/// The environment variable that carries a parameter's value to duckdb.
pub const ENV_PREFIX: &str = "FHIR_PIPES_PARAM_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    String,
    Integer,
    Decimal,
    Boolean,
}

impl Type {
    pub fn name(self) -> &'static str {
        match self {
            Type::String => "string",
            Type::Integer => "integer",
            Type::Decimal => "decimal",
            Type::Boolean => "boolean",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "string" => Type::String,
            "integer" => Type::Integer,
            "decimal" => Type::Decimal,
            "boolean" => Type::Boolean,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Integer(i64),
    Decimal(f64),
    Boolean(bool),
}

impl ParamValue {
    pub fn kind(&self) -> Type {
        match self {
            ParamValue::String(_) => Type::String,
            ParamValue::Integer(_) => Type::Integer,
            ParamValue::Decimal(_) => Type::Decimal,
            ParamValue::Boolean(_) => Type::Boolean,
        }
    }

    /// The value as JSON: for the run report and the map engine.
    pub fn json(&self) -> Value {
        match self {
            ParamValue::String(s) => Value::String(s.clone()),
            ParamValue::Integer(n) => Value::from(*n),
            ParamValue::Decimal(d) => serde_json::Number::from_f64(*d)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            ParamValue::Boolean(b) => Value::Bool(*b),
        }
    }

    /// The value as text, as duckdb casts it from the environment.
    pub fn text(&self) -> String {
        match self {
            ParamValue::String(s) => s.clone(),
            ParamValue::Integer(n) => n.to_string(),
            ParamValue::Decimal(d) => format!("{d:?}"),
            ParamValue::Boolean(b) => b.to_string(),
        }
    }

    /// The ViewDefinition constant that gives a view this value as `%name`.
    pub fn constant(&self, name: &str) -> fhir_pipes_sof::Constant {
        let mut c = fhir_pipes_sof::Constant {
            name: name.to_string(),
            value_string: None,
            value_integer: None,
            value_boolean: None,
            value_decimal: None,
            values: serde_json::Map::new(),
        };
        match self {
            ParamValue::String(s) => c.value_string = Some(s.clone()),
            ParamValue::Integer(n) => c.value_integer = Some(*n),
            ParamValue::Decimal(d) => c.value_decimal = Some(*d),
            ParamValue::Boolean(b) => c.value_boolean = Some(*b),
        }
        c
    }

    /// A value of type `t` from command-line text.
    fn from_text(t: Type, text: &str) -> std::result::Result<Self, String> {
        match t {
            Type::String => Ok(ParamValue::String(text.to_string())),
            Type::Integer => text
                .trim()
                .parse()
                .map(ParamValue::Integer)
                .map_err(|_| format!("{text:?} is not a whole number")),
            Type::Decimal => text
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|d| d.is_finite())
                .map(ParamValue::Decimal)
                .ok_or_else(|| format!("{text:?} is not a decimal number")),
            Type::Boolean => match text.trim() {
                "true" => Ok(ParamValue::Boolean(true)),
                "false" => Ok(ParamValue::Boolean(false)),
                _ => Err(format!("{text:?} is not true or false")),
            },
        }
    }

    /// A value from TOML, of type `t` when given: an integer is taken as a
    /// decimal, nothing else is converted.
    fn from_toml(v: &toml::Value, t: Option<Type>) -> std::result::Result<Self, String> {
        let value = match v {
            toml::Value::String(s) => ParamValue::String(s.clone()),
            toml::Value::Integer(n) if t == Some(Type::Decimal) => ParamValue::Decimal(*n as f64),
            toml::Value::Integer(n) => ParamValue::Integer(*n),
            toml::Value::Float(d) if d.is_finite() => ParamValue::Decimal(*d),
            toml::Value::Boolean(b) => ParamValue::Boolean(*b),
            other => {
                return Err(format!(
                    "a parameter value is a string, an integer, a decimal or a boolean; this \
                     one is of type {}",
                    other.type_str()
                ));
            }
        };
        match t {
            Some(t) if t != value.kind() => Err(format!(
                "the value is of type {}, and the parameter's type is {}",
                value.kind().name(),
                t.name()
            )),
            _ => Ok(value),
        }
    }
}

/// A parameter the package declares.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub kind: Type,
    /// None: every run must give a value.
    pub default: Option<ParamValue>,
    pub description: Option<String>,
}

/// A name a parameter may take: `[A-Za-z_][A-Za-z0-9_]*`, so it is a
/// FHIRPath name, a SQL name and part of an environment variable's name.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[parameters]`: each key a parameter, its value the default, or a table
/// with `default`, `type` and `description`. Without `default` the type is
/// needed and a run must give a value.
pub fn parse(table: &toml::Table, origin: &str) -> Result<Vec<Parameter>> {
    let mut out = Vec::new();
    for (name, v) in table {
        let at = format!("{origin}: parameters.{name}");
        let bad = |m: String| PipesError::Usage(format!("{at}: {m}"));
        if !valid_name(name) {
            return Err(bad(
                "a parameter name is letters, digits and `_`, and does not start with a digit"
                    .into(),
            ));
        }
        if RESERVED.contains(&name.as_str()) {
            return Err(bad(format!(
                "`{name}` is a variable the FHIRPath or view engine already has; choose another \
                 name"
            )));
        }
        let parameter = match v {
            toml::Value::Table(t) => {
                if let Some(key) = t
                    .keys()
                    .find(|k| !["default", "type", "description"].contains(&k.as_str()))
                {
                    return Err(bad(format!(
                        "unknown key `{key}`; a parameter takes default, type and description"
                    )));
                }
                let kind = match t.get("type") {
                    None => None,
                    Some(toml::Value::String(s)) => Some(Type::parse(s).ok_or_else(|| {
                        bad(format!(
                            "type {s:?}; give \"string\", \"integer\", \"decimal\" or \"boolean\""
                        ))
                    })?),
                    Some(_) => return Err(bad("type must be a string".into())),
                };
                let description = match t.get("description") {
                    None => None,
                    Some(toml::Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(bad("description must be a string".into())),
                };
                let default = t
                    .get("default")
                    .map(|d| ParamValue::from_toml(d, kind))
                    .transpose()
                    .map_err(|m| bad(format!("default: {m}")))?;
                let kind = match (kind, &default) {
                    (Some(k), _) => k,
                    (None, Some(d)) => d.kind(),
                    (None, None) => {
                        return Err(bad(
                            "with no default, give its type, so a value given to a run can be \
                             checked"
                                .into(),
                        ));
                    }
                };
                Parameter {
                    name: name.clone(),
                    kind,
                    default,
                    description,
                }
            }
            v => {
                let default = ParamValue::from_toml(v, None).map_err(bad)?;
                Parameter {
                    name: name.clone(),
                    kind: default.kind(),
                    default: Some(default),
                    description: None,
                }
            }
        };
        out.push(parameter);
    }
    Ok(out)
}

/// `NAME=VALUE`, as `--param` takes it.
pub fn parse_assignment(text: &str) -> std::result::Result<(String, String), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.trim().is_empty() => {
            Ok((name.trim().to_string(), value.to_string()))
        }
        _ => Err(format!("{text:?}: give NAME=VALUE")),
    }
}

/// The values of a run, by name.
pub type Values = BTreeMap<String, ParamValue>;

/// Where a value comes from, for messages.
pub enum Given<'a> {
    /// `--param NAME=VALUE`, as text.
    Text(&'a [(String, String)]),
    /// A test case's `parameters.toml`, typed.
    Toml(&'a toml::Table, &'a str),
}

/// The values of a run: each default, replaced by each of `given` in turn.
/// A name the package does not declare, a value of the wrong type or a name
/// given twice in one source is an error. With `require`, so is a parameter
/// with no default and no value; without it (`import check`), it is left out.
pub fn values(declared: &[Parameter], given: &[Given], require: bool) -> Result<Values> {
    let mut out: Values = declared
        .iter()
        .filter_map(|p| Some((p.name.clone(), p.default.clone()?)))
        .collect();
    let find = |name: &str, from: &str| -> Result<&Parameter> {
        declared.iter().find(|p| p.name == name).ok_or_else(|| {
            let names: Vec<&str> = declared.iter().map(|p| p.name.as_str()).collect();
            PipesError::Usage(format!(
                "{from}: the package has no parameter {name}; its parameters are: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            ))
        })
    };
    for g in given {
        let mut seen = BTreeSet::new();
        match g {
            Given::Text(pairs) => {
                for (name, text) in *pairs {
                    let from = format!("--param {name}");
                    let p = find(name, &from)?;
                    if !seen.insert(name.clone()) {
                        return Err(PipesError::Usage(format!("{from}: given twice")));
                    }
                    let v = ParamValue::from_text(p.kind, text).map_err(|m| {
                        PipesError::Usage(format!(
                            "{from}: {m}; the parameter's type is {}",
                            p.kind.name()
                        ))
                    })?;
                    out.insert(name.clone(), v);
                }
            }
            Given::Toml(table, origin) => {
                for (name, v) in *table {
                    let from = format!("{origin}: {name}");
                    let p = find(name, &from)?;
                    let v = ParamValue::from_toml(v, Some(p.kind))
                        .map_err(|m| PipesError::Usage(format!("{from}: {m}")))?;
                    out.insert(name.clone(), v);
                }
            }
        }
    }
    if require {
        let missing: Vec<&str> = declared
            .iter()
            .filter(|p| !out.contains_key(&p.name))
            .map(|p| p.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(PipesError::Usage(format!(
                "the package's parameters {} have no default; give each with --param NAME=VALUE",
                missing.join(", ")
            )));
        }
    }
    Ok(out)
}

/// The duckdb type a SQLQuery Library parameter of FHIR type `fhir` is cast
/// to, when a package parameter of type `t` can give it a value.
pub fn sql_type(fhir: &str, t: Type) -> Option<&'static str> {
    let sql = match fhir {
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
    };
    let fits = match sql {
        "VARCHAR" | "DATE" | "TIMESTAMPTZ" | "TIME" => t == Type::String,
        "BIGINT" => t == Type::Integer,
        "DOUBLE" => matches!(t, Type::Decimal | Type::Integer),
        _ => t == Type::Boolean,
    };
    fits.then_some(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str::<toml::Table>(text).unwrap()["parameters"]
            .as_table()
            .unwrap()
            .clone()
    }

    fn message<T: std::fmt::Debug>(r: Result<T>) -> String {
        match r {
            Err(PipesError::Usage(m)) => m,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn plain_values_and_tables_parse() {
        let ps = parse(
            &table(
                "[parameters]\nprefix = \"dgt-\"\nwindow = 2\nratio = 0.5\nstrict = true\n\
                 asOf = { type = \"string\", description = \"When the records were extracted\" }\n\
                 scale = { default = 3, type = \"decimal\" }\n",
            ),
            "import.toml",
        )
        .unwrap();
        let by = |n: &str| ps.iter().find(|p| p.name == n).unwrap();
        assert_eq!(
            by("prefix").default,
            Some(ParamValue::String("dgt-".into()))
        );
        assert_eq!(by("window").kind, Type::Integer);
        assert_eq!(by("ratio").kind, Type::Decimal);
        assert_eq!(by("strict").default, Some(ParamValue::Boolean(true)));
        assert_eq!(by("asOf").default, None);
        assert_eq!(by("asOf").kind, Type::String);
        assert_eq!(
            by("asOf").description.as_deref(),
            Some("When the records were extracted")
        );
        assert_eq!(by("scale").default, Some(ParamValue::Decimal(3.0)));
    }

    #[test]
    fn bad_declarations_are_refused() {
        let m = |t: &str| {
            message(parse(
                &table(&format!("[parameters]\n{t}\n")),
                "import.toml",
            ))
        };
        assert!(m("1x = 1").contains("parameter name is letters"));
        assert!(m("resource = 1").contains("already has"));
        assert!(m("a = [1]").contains("this one is of type array"));
        assert!(m("a = { type = \"string\", x = 1 }").contains("unknown key `x`"));
        assert!(m("a = { description = \"d\" }").contains("give its type"));
        assert!(m("a = { type = \"date\" }").contains("type \"date\""));
        assert!(
            m("a = { default = \"x\", type = \"integer\" }")
                .contains("the value is of type string")
        );
    }

    #[test]
    fn values_take_defaults_then_each_source() {
        let ps = parse(
            &table("[parameters]\nprefix = \"dgt-\"\nwindow = 2\nasOf = { type = \"string\" }\n"),
            "import.toml",
        )
        .unwrap();
        let cli = vec![
            ("window".to_string(), "4".to_string()),
            ("asOf".to_string(), "2026-01-01".to_string()),
        ];
        let case = table("[parameters]\nprefix = \"x-\"\n");
        let v = values(
            &ps,
            &[Given::Text(&cli), Given::Toml(&case, "parameters.toml")],
            true,
        )
        .unwrap();
        assert_eq!(v["prefix"], ParamValue::String("x-".into()));
        assert_eq!(v["window"], ParamValue::Integer(4));
        assert_eq!(v["asOf"], ParamValue::String("2026-01-01".into()));

        let m = message(values(&ps, &[], true));
        assert!(m.contains("asOf have no default"), "{m}");
        assert!(!values(&ps, &[], false).unwrap().contains_key("asOf"));
        let unknown = vec![("windw".to_string(), "4".to_string())];
        let m = message(values(&ps, &[Given::Text(&unknown)], true));
        assert!(
            m.contains("--param windw: the package has no parameter windw; its parameters are: ")
                && m.contains("window"),
            "{m}"
        );
        let bad = vec![("window".to_string(), "two".to_string())];
        let m = message(values(&ps, &[Given::Text(&bad)], false));
        assert!(m.contains("\"two\" is not a whole number"), "{m}");
        let twice = vec![
            ("window".to_string(), "1".to_string()),
            ("window".to_string(), "2".to_string()),
        ];
        assert!(message(values(&ps, &[Given::Text(&twice)], false)).contains("given twice"));
    }

    #[test]
    fn assignments_and_sql_types() {
        assert_eq!(
            parse_assignment("a=b=c").unwrap(),
            ("a".to_string(), "b=c".to_string())
        );
        assert_eq!(parse_assignment("a=").unwrap().1, "");
        assert!(parse_assignment("a").is_err());
        assert!(parse_assignment("=b").is_err());
        assert_eq!(sql_type("dateTime", Type::String), Some("TIMESTAMPTZ"));
        assert_eq!(sql_type("decimal", Type::Integer), Some("DOUBLE"));
        assert_eq!(sql_type("integer", Type::Decimal), None);
        assert_eq!(sql_type("Coding", Type::String), None);
    }
}
