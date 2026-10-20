//! One edited row, whatever file it came from. The readers keep the built-in
//! writable columns and every column that is not built in (a profile column,
//! or one diff then refuses); each reader coerces its native types into
//! `ColumnValue` and the rebuild step never sees a file format.

use std::collections::BTreeMap;

use geo::Geometry;
use serde_json::Value;

pub use crate::fhir::location::Identifier;

/// Built-in writable text columns. A profile's columns are text too.
pub const TEXT_COLUMNS: [&str; 7] = [
    "name",
    "status",
    "description",
    "type",
    "physical_type",
    "part_of",
    "managing_organization",
];
pub const NUMBER_COLUMNS: [&str; 2] = ["position_longitude", "position_latitude"];
pub const ALIAS_COLUMN: &str = "alias";
pub const IDENTIFIER_COLUMN: &str = "identifier";
/// Whole-list identifier columns: the Location's and the Organization's.
pub const IDENTIFIER_COLUMNS: [&str; 2] = ["identifier", "organization_identifier"];
pub const ID_COLUMN: &str = "id";

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnValue {
    /// Present and empty: the FHIR field is removed.
    Null,
    Text(String),
    Number(f64),
    TextList(Vec<String>),
    Identifiers(Vec<Identifier>),
}

#[derive(Debug, Clone, Default)]
pub struct InputRow {
    pub id: Option<String>,
    /// Only writable columns, keyed by README column name. Absent means untouched.
    pub columns: BTreeMap<String, ColumnValue>,
    /// Writable columns whose value could not be read (reported as
    /// input_column_type and left out of `columns`). diff still checks their
    /// names against the profile, so a number cannot slip an undeclared
    /// column past it.
    pub unread: Vec<String>,
    /// 2D, unrounded; `None` means no geometry edit.
    pub geometry: Option<Geometry<f64>>,
    /// 1-based feature or row index, for messages.
    pub line: usize,
}

/// A built-in column diff writes back.
pub fn is_builtin_writable(name: &str) -> bool {
    TEXT_COLUMNS.contains(&name)
        || NUMBER_COLUMNS.contains(&name)
        || name == ALIAS_COLUMN
        || IDENTIFIER_COLUMNS.contains(&name)
}

/// A column the readers keep: a built-in writable one, or any column that is
/// not built in. Built-in derived columns (admin1_name, path, …) are dropped.
pub fn is_writable(name: &str) -> bool {
    is_builtin_writable(name) || !crate::write::schema::is_builtin_column(name)
}

fn text_list(v: &Value) -> Result<ColumnValue, String> {
    let Value::Array(items) = v else {
        return Err("expected a list of strings".into());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str() {
            Some(s) => out.push(s.to_string()),
            None => return Err("expected a list of strings".into()),
        }
    }
    Ok(if out.is_empty() {
        ColumnValue::Null
    } else {
        ColumnValue::TextList(out)
    })
}

fn identifiers(v: &Value) -> Result<ColumnValue, String> {
    let Value::Array(items) = v else {
        return Err("expected a list of {system, value} objects".into());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Value::Object(o) = item else {
            return Err("expected a list of {system, value} objects".into());
        };
        out.push(Identifier {
            system: o.get("system").and_then(Value::as_str).map(str::to_string),
            value: o.get("value").and_then(Value::as_str).map(str::to_string),
        });
    }
    Ok(if out.is_empty() {
        ColumnValue::Null
    } else {
        ColumnValue::Identifiers(out)
    })
}

/// A string holding JSON of the expected shape is accepted for the list
/// columns: GDAL flattens nested fields to JSON text on export.
fn list_or_json_string(
    v: &Value,
    parse: fn(&Value) -> Result<ColumnValue, String>,
) -> Result<ColumnValue, String> {
    match v {
        Value::String(s) if s.trim().is_empty() => Ok(ColumnValue::Null),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(inner @ Value::Array(_)) => parse(&inner),
            _ => Err("expected a list, or a string holding a JSON list".into()),
        },
        other => parse(other),
    }
}

/// Coerce one cell to the column's type. `Err` carries a message for the
/// `input_column_type` report; the caller drops the column for that row.
pub fn column_from_json(name: &str, v: &Value) -> Result<ColumnValue, String> {
    if v.is_null() {
        return Ok(ColumnValue::Null);
    }
    // Built-in text columns, and every column that is not built in: a
    // profile's columns are text, and diff refuses the rest by name.
    if TEXT_COLUMNS.contains(&name) || !crate::write::schema::is_builtin_column(name) {
        return match v {
            Value::String(s) if s.is_empty() => Ok(ColumnValue::Null),
            Value::String(s) => Ok(ColumnValue::Text(s.clone())),
            _ => Err("expected text".into()),
        };
    }
    if NUMBER_COLUMNS.contains(&name) {
        return match v {
            Value::Number(n) => n
                .as_f64()
                .map(ColumnValue::Number)
                .ok_or_else(|| "expected a number".into()),
            Value::String(s) if s.trim().is_empty() => Ok(ColumnValue::Null),
            // "NaN" and "inf" parse as f64, but are not coordinates.
            Value::String(s) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .map(ColumnValue::Number)
                .ok_or_else(|| "expected a number".into()),
            _ => Err("expected a number".into()),
        };
    }
    if name == ALIAS_COLUMN {
        return list_or_json_string(v, text_list);
    }
    if IDENTIFIER_COLUMNS.contains(&name) {
        return list_or_json_string(v, identifiers);
    }
    Err(format!("{name} is not a writable column"))
}

/// The CRS a GeoParquet `crs` or a GeoJSON `crs` member names, when it is
/// not longitude/latitude on WGS 84 (OGC:CRS84 or EPSG:4326). `None` for
/// WGS 84, and for an unknown CRS (null, or no name to go by), which the
/// coordinate range check then covers.
pub fn other_crs(crs: &Value) -> Option<String> {
    let wgs84 = |s: &str| {
        let s = s.to_ascii_uppercase();
        s.ends_with("CRS84") || (s.contains("EPSG") && s.trim_end_matches(':').ends_with("4326"))
    };
    let name = match crs {
        Value::String(s) => s.clone(),
        Value::Object(o) => match (o.get("id"), o.get("properties")) {
            // PROJJSON: {"id": {"authority": "EPSG", "code": 4326}}.
            (Some(id), _) => {
                let code = match &id["code"] {
                    Value::Number(n) => n.to_string(),
                    other => other.as_str().unwrap_or_default().to_string(),
                };
                format!("{}:{code}", id["authority"].as_str().unwrap_or_default())
            }
            // GeoJSON 2008: {"type": "name", "properties": {"name": "urn:…"}}.
            (None, Some(p)) => p["name"].as_str()?.to_string(),
            (None, None) => {
                let name = o.get("name")?.as_str()?;
                return (!name.contains("WGS 84")).then(|| name.to_string());
            }
        },
        _ => return None,
    };
    (!wgs84(&name)).then_some(name)
}

/// Why a coordinate cannot be a WGS 84 longitude/latitude, if it cannot.
pub fn out_of_range(lon: f64, lat: f64) -> Option<String> {
    if !lon.is_finite() || !lat.is_finite() {
        return Some(format!("({lon}, {lat}) is not a finite coordinate"));
    }
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Some(format!(
            "({lon}, {lat}) is outside longitude -180..180 and latitude -90..90; \
             is the file in another CRS?"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crs_names_that_are_not_wgs84() {
        for wgs84 in [
            json!("EPSG:4326"),
            json!("OGC:CRS84"),
            json!({"type": "name", "properties": {"name": "urn:ogc:def:crs:OGC:1.3:CRS84"}}),
            json!({"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::4326"}}),
            json!({"id": {"authority": "EPSG", "code": 4326}}),
            json!({"name": "WGS 84 (CRS84)"}),
            Value::Null,
        ] {
            assert_eq!(other_crs(&wgs84), None, "{wgs84}");
        }
        assert_eq!(
            other_crs(
                &json!({"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::3857"}})
            ),
            Some("urn:ogc:def:crs:EPSG::3857".into())
        );
        assert_eq!(
            other_crs(&json!({"id": {"authority": "EPSG", "code": 32631}})),
            Some("EPSG:32631".into())
        );
    }

    #[test]
    fn coordinates_out_of_range() {
        assert_eq!(out_of_range(3.0, 6.0), None);
        assert!(out_of_range(f64::NAN, 6.0).is_some());
        assert!(
            out_of_range(334_111.0, 6.0)
                .unwrap()
                .contains("another CRS")
        );
        assert!(column_from_json("position_latitude", &json!("NaN")).is_err());
        assert!(column_from_json("position_latitude", &json!("inf")).is_err());
    }

    #[test]
    fn writable_columns_are_the_readme_second_group() {
        assert!(is_writable("name"));
        assert!(is_writable("position_longitude"));
        assert!(is_writable("identifier"));
        assert!(is_writable("pcode"));
        assert!(!is_writable("admin1_name"));
        assert!(!is_writable("version_id"));
        assert!(!is_writable("id"));
        assert!(!is_writable("geometry"));
        assert!(is_writable("nhfr_code"));
        assert!(is_writable("facility_level_text"));
        assert!(is_writable("organization_identifier"));
        assert!(!is_writable("organization_json"));
    }

    #[test]
    fn text_columns_coerce_and_empty_string_clears() {
        assert_eq!(
            column_from_json("name", &json!("Gama")),
            Ok(ColumnValue::Text("Gama".into()))
        );
        assert_eq!(column_from_json("name", &json!("")), Ok(ColumnValue::Null));
        assert_eq!(
            column_from_json("name", &json!(null)),
            Ok(ColumnValue::Null)
        );
        assert!(column_from_json("name", &json!(5)).is_err());
    }

    #[test]
    fn number_columns_accept_numbers_and_numeric_strings() {
        assert_eq!(
            column_from_json("position_longitude", &json!(3.25)),
            Ok(ColumnValue::Number(3.25))
        );
        assert_eq!(
            column_from_json("position_latitude", &json!("6.25")),
            Ok(ColumnValue::Number(6.25))
        );
        assert!(column_from_json("position_latitude", &json!("north")).is_err());
        assert!(column_from_json("position_latitude", &json!(true)).is_err());
    }

    #[test]
    fn alias_accepts_a_list_or_a_json_string_of_one() {
        let want = Ok(ColumnValue::TextList(vec!["a".into(), "b".into()]));
        assert_eq!(column_from_json("alias", &json!(["a", "b"])), want);
        assert_eq!(column_from_json("alias", &json!("[\"a\",\"b\"]")), want);
        assert_eq!(column_from_json("alias", &json!([])), Ok(ColumnValue::Null));
        assert!(column_from_json("alias", &json!([1])).is_err());
        assert!(column_from_json("alias", &json!("not json")).is_err());
    }

    #[test]
    fn identifier_accepts_objects_or_a_json_string_of_them() {
        let want = Ok(ColumnValue::Identifiers(vec![
            Identifier {
                system: Some("s".into()),
                value: Some("v".into()),
            },
            Identifier {
                system: None,
                value: Some("w".into()),
            },
        ]));
        let list = json!([{"system": "s", "value": "v"}, {"value": "w"}]);
        assert_eq!(column_from_json("identifier", &list), want);
        assert_eq!(
            column_from_json("identifier", &json!(list.to_string())),
            want
        );
        assert_eq!(column_from_json("organization_identifier", &list), want);
        assert!(column_from_json("identifier", &json!(["s|v"])).is_err());
    }
}
