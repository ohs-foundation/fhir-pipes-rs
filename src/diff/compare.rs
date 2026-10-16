//! What "changed" means: coordinates compare after rounding to seven
//! decimals (about a centimetre), so a float round trip through a GIS
//! tool is not an edit; resources compare as JSON values, whatever the
//! order of their object keys.

use geo::{Geometry, MapCoords};
use serde_json::Value;

use crate::geometry::to_wkb;

pub const DECIMALS: i32 = 7;

pub fn round_coord(v: f64) -> f64 {
    let f = 10f64.powi(DECIMALS);
    (v * f).round() / f
}

pub fn round_geometry(g: &Geometry<f64>) -> Geometry<f64> {
    g.map_coords(|c| geo::coord! { x: round_coord(c.x), y: round_coord(c.y) })
}

/// The comparison key for a geometry: little-endian WKB of the rounded
/// geometry. Two geometries with the same key are the same edit.
pub fn wkb_key(g: &Geometry<f64>) -> Vec<u8> {
    to_wkb(&round_geometry(g))
}

/// Whether two resources differ. Object key order does not count, array
/// order does. `serde_json` is built with `preserve_order`, which makes
/// `Map` an `IndexMap`: its equality looks each key up rather than
/// comparing positions, so `!=` needs no sorted copy of either side.
pub fn differs(a: &Value, b: &Value) -> bool {
    a != b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rounds_to_seven_decimals() {
        assert_eq!(round_coord(3.123456789), 3.1234568);
        assert_eq!(round_coord(3.25), 3.25);
        assert_eq!(round_coord(-0.00000004), -0.0);
    }

    #[test]
    fn float_noise_in_the_eighth_decimal_gives_the_same_key() {
        let a = geo::Geometry::Point(geo::Point::new(3.25, 6.25));
        let b = geo::Geometry::Point(geo::Point::new(3.250000004, 6.249999996));
        assert_eq!(wkb_key(&a), wkb_key(&b));
        let c = geo::Geometry::Point(geo::Point::new(3.2500001, 6.25));
        assert_ne!(wkb_key(&a), wkb_key(&c));
    }

    /// Object keys sorted recursively; arrays keep their order. What
    /// `differs` replaced: comparing these copies.
    fn canonical(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), canonical(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }

    #[test]
    fn object_key_order_is_kept_in_this_build() {
        // `preserve_order` is on: without it the keys would come out sorted
        // and the order-insensitive equality below would prove nothing.
        let v: Value = serde_json::from_str(r#"{"b":1,"a":{"z":1,"y":2}}"#).unwrap();
        assert_eq!(v.to_string(), r#"{"b":1,"a":{"z":1,"y":2}}"#);
        assert_eq!(canonical(&v).to_string(), r#"{"a":{"y":2,"z":1},"b":1}"#);
    }

    #[test]
    fn differs_ignores_key_order_at_any_depth_but_not_array_order() {
        let base = json!({"resourceType": "Location", "id": "a",
            "identifier": [{"system": "s", "value": "1"}, {"system": "t", "value": "2"}],
            "position": {"longitude": 3.25, "latitude": 6.25}});
        let cases = [
            // Reordered keys, top level and nested: equal.
            (
                json!({"id": "a", "position": {"latitude": 6.25, "longitude": 3.25},
                "identifier": [{"value": "1", "system": "s"}, {"value": "2", "system": "t"}],
                "resourceType": "Location"}),
                false,
            ),
            // Array order counts.
            (
                json!({"resourceType": "Location", "id": "a",
                "identifier": [{"system": "t", "value": "2"}, {"system": "s", "value": "1"}],
                "position": {"longitude": 3.25, "latitude": 6.25}}),
                true,
            ),
            // A changed value, an extra key, a missing key, a number type.
            (
                json!({"resourceType": "Location", "id": "b",
                "identifier": [{"system": "s", "value": "1"}, {"system": "t", "value": "2"}],
                "position": {"longitude": 3.25, "latitude": 6.25}}),
                true,
            ),
            (
                json!({"resourceType": "Location", "id": "a", "name": "x",
                "identifier": [{"system": "s", "value": "1"}, {"system": "t", "value": "2"}],
                "position": {"longitude": 3.25, "latitude": 6.25}}),
                true,
            ),
            (
                json!({"resourceType": "Location", "id": "a",
                "position": {"longitude": 3.25, "latitude": 6.25}}),
                true,
            ),
            (
                json!({"resourceType": "Location", "id": "a",
                "identifier": [{"system": "s", "value": "1"}, {"system": "t", "value": "2"}],
                "position": {"longitude": 3, "latitude": 6.25}}),
                true,
            ),
        ];
        // The reordered case is equal as a value, not as text.
        assert_ne!(cases[0].0.to_string(), base.to_string());
        for (other, expected) in cases {
            assert_eq!(differs(&other, &base), expected, "{other}");
            assert_eq!(differs(&base, &other), expected, "{other}");
            // The same answer as comparing sorted copies.
            assert_eq!(canonical(&other) != canonical(&base), expected, "{other}");
        }
    }
}
