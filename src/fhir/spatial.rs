//! Spatial index cells: the tile a point falls in under a tiling scheme.
//!
//! The ICR IG's `spatial-index` extension carries, per Location, the scheme
//! (quadkey | h3 | geohash), the level (quadkey zoom / H3 resolution /
//! geohash precision) and the cell key. The cell is a pure function of the
//! position, so fhir-pipes owns it: `import location-sites` writes it at import,
//! `fhir-pipes index` backfills it onto an existing snapshot as an ordinary
//! version-checked edit, and `fhir-pipes diff` recomputes it whenever a position
//! edit comes through. Quadkey and geohash are computed here; H3 needs a
//! library and is left for a later round (its cells are still parsed and
//! carried).

use std::ops::RangeInclusive;

use serde_json::{Map, Value, json};

use crate::error::{PipesError, Result};
use crate::report::Report;

pub const SCHEMES: [&str; 3] = ["quadkey", "h3", "geohash"];
const MAX_MERCATOR_LAT: f64 = 85.051_128_78;
const GEOHASH_ALPHABET: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

type Object = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialCell {
    pub scheme: String,
    pub level: u32,
    pub cell: String,
}

/// The levels a scheme allows: quadkey zoom 1-23, geohash precision 1-12,
/// H3 resolution 0-15. `None` for a scheme fhir-pipes does not know.
pub fn level_range(scheme: &str) -> Option<RangeInclusive<u32>> {
    match scheme {
        "quadkey" => Some(1..=23),
        "geohash" => Some(1..=12),
        "h3" => Some(0..=15),
        _ => None,
    }
}

/// `SCHEME:LEVEL`, e.g. `quadkey:18`.
pub fn parse_scheme_level(arg: &str) -> Result<(String, u32)> {
    let usage = || {
        PipesError::Usage(format!(
            "--spatial-index must be SCHEME:LEVEL (e.g. quadkey:18), got {arg:?}"
        ))
    };
    let (scheme, level) = arg.split_once(':').ok_or_else(usage)?;
    let level: u32 = level.trim().parse().map_err(|_| usage())?;
    let scheme = scheme.trim();
    let in_range = level_range(scheme).is_some_and(|r| r.contains(&level));
    match scheme {
        "quadkey" if in_range => {}
        "quadkey" => {
            return Err(PipesError::Usage(format!(
                "quadkey zoom must be 1-23, got {level}"
            )));
        }
        "geohash" if in_range => {}
        "geohash" => {
            return Err(PipesError::Usage(format!(
                "geohash precision must be 1-12, got {level}"
            )));
        }
        "h3" => {
            return Err(PipesError::Usage(
                "h3 cells are not computed yet; use quadkey or geohash".to_string(),
            ));
        }
        _ => {
            return Err(PipesError::Usage(format!(
                "--spatial-index scheme must be one of {}, got {scheme:?}",
                SCHEMES.join(", ")
            )));
        }
    }
    Ok((scheme.to_string(), level))
}

/// Bing/XYZ tile quadkey of the point at `level` (zoom 1-23): base-4 digits,
/// one per zoom from 1, so every prefix is the containing tile at that
/// shorter zoom. Latitude is clamped to the Web Mercator range.
pub fn quadkey(lon: f64, lat: f64, level: u32) -> String {
    let lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    let lon = ((lon + 180.0).rem_euclid(360.0)) - 180.0;
    let n = 1u64 << level;
    let x = (((lon + 180.0) / 360.0) * n as f64) as i64;
    let sin_lat = lat.to_radians().sin();
    let y = ((0.5 - ((1.0 + sin_lat) / (1.0 - sin_lat)).ln() / (4.0 * std::f64::consts::PI))
        * n as f64) as i64;
    let x = x.clamp(0, n as i64 - 1) as u64;
    let y = y.clamp(0, n as i64 - 1) as u64;
    let mut out = String::with_capacity(level as usize);
    for i in (1..=level).rev() {
        let mask = 1u64 << (i - 1);
        let mut digit = 0u8;
        if x & mask != 0 {
            digit += 1;
        }
        if y & mask != 0 {
            digit += 2;
        }
        out.push((b'0' + digit) as char);
    }
    out
}

/// Base-32 geohash of the point at `precision` characters (1-12).
pub fn geohash(lon: f64, lat: f64, precision: u32) -> String {
    let (mut lat_lo, mut lat_hi) = (-90.0f64, 90.0f64);
    let (mut lon_lo, mut lon_hi) = (-180.0f64, 180.0f64);
    let mut out = String::with_capacity(precision as usize);
    let (mut bits, mut bit_count, mut even) = (0usize, 0u8, true);
    while out.len() < precision as usize {
        if even {
            let mid = (lon_lo + lon_hi) / 2.0;
            if lon >= mid {
                bits = (bits << 1) | 1;
                lon_lo = mid;
            } else {
                bits <<= 1;
                lon_hi = mid;
            }
        } else {
            let mid = (lat_lo + lat_hi) / 2.0;
            if lat >= mid {
                bits = (bits << 1) | 1;
                lat_lo = mid;
            } else {
                bits <<= 1;
                lat_hi = mid;
            }
        }
        even = !even;
        bit_count += 1;
        if bit_count == 5 {
            out.push(GEOHASH_ALPHABET[bits] as char);
            bits = 0;
            bit_count = 0;
        }
    }
    out
}

/// The cell for a scheme fhir-pipes can compute; `None` for h3 (carried, not
/// computed) and for a level outside the scheme's range.
pub fn compute_cell(scheme: &str, level: u32, lon: f64, lat: f64) -> Option<String> {
    if !level_range(scheme)?.contains(&level) {
        return None;
    }
    match scheme {
        "quadkey" => Some(quadkey(lon, lat, level)),
        "geohash" => Some(geohash(lon, lat, level)),
        _ => None,
    }
}

/// One spatial index extension entry under `url` (the profile's
/// `spatial_index_extension`).
pub fn extension_json(url: &str, scheme: &str, level: u32, cell: &str) -> Value {
    json!({
        "url": url,
        "extension": [
            {"url": "system", "valueCode": scheme},
            {"url": "level", "valueUnsignedInt": level},
            {"url": "cell", "valueString": cell},
        ]
    })
}

fn sub(ext: &Object, name: &str) -> Option<Value> {
    ext.get("extension")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|e| e.get("url").and_then(Value::as_str) == Some(name))
        .and_then(|e| {
            e.iter()
                .find(|(k, _)| k.starts_with("value"))
                .map(|(_, v)| v.clone())
        })
}

/// One `spatial-index` extension entry: `None` if `ext` is not one (another
/// url), `Some(Err(reason))` if it is malformed. A level outside the
/// scheme's range (see `level_range`) is malformed: computing a cell at it
/// would overflow or allocate without bound.
pub fn cell_from_extension(
    ext: &Object,
    url: &str,
) -> Option<std::result::Result<SpatialCell, String>> {
    if ext.get("url").and_then(Value::as_str) != Some(url) {
        return None;
    }
    let scheme = sub(ext, "system").and_then(|v| v.as_str().map(str::to_string));
    let level = sub(ext, "level").and_then(|v| v.as_u64());
    let cell = sub(ext, "cell").and_then(|v| v.as_str().map(str::to_string));
    let (Some(scheme), Some(level), Some(cell)) = (scheme, level, cell) else {
        return Some(Err(
            "spatial-index extension lacks system, level or cell".to_string()
        ));
    };
    let checked = u32::try_from(level)
        .ok()
        .filter(|l| level_range(&scheme).is_none_or(|r| r.contains(l)));
    let Some(level) = checked else {
        let allowed = level_range(&scheme).map_or(String::new(), |r| {
            format!(" (allowed {}-{})", r.start(), r.end())
        });
        return Some(Err(format!(
            "spatial-index {scheme} level {level} is out of range{allowed}"
        )));
    };
    Some(Ok(SpatialCell {
        scheme,
        level,
        cell,
    }))
}

/// Every spatial-index entry on a resource object, well-formed or not.
fn entries_of(obj: &Object, url: &str) -> Vec<std::result::Result<SpatialCell, String>> {
    obj.get("extension")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|e| cell_from_extension(e, url))
        .collect()
}

/// Every well-formed spatial-index cell on a resource object.
pub fn cells_of(obj: &Object, url: &str) -> Vec<SpatialCell> {
    entries_of(obj, url)
        .into_iter()
        .filter_map(|c| c.ok())
        .collect()
}

/// The deepest quadkey on the resource: its prefixes are every coarser tile,
/// so one column carries the whole hierarchy.
pub fn deepest_quadkey(cells: &[SpatialCell]) -> Option<&SpatialCell> {
    cells
        .iter()
        .filter(|c| c.scheme == "quadkey")
        .max_by_key(|c| c.level)
}

/// Set the cell for (scheme, level), replacing an existing entry at the same
/// scheme and level (the IG allows one per pair). Returns true if anything changed.
pub fn upsert_cell(obj: &mut Object, url: &str, scheme: &str, level: u32, cell: &str) -> bool {
    let exts = obj
        .entry("extension")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(exts) = exts else {
        return false;
    };
    for ext in exts.iter_mut() {
        let Some(Ok(existing)) = ext.as_object().and_then(|e| cell_from_extension(e, url)) else {
            continue;
        };
        if existing.scheme == scheme && existing.level == level {
            if existing.cell == cell {
                return false;
            }
            *ext = extension_json(url, scheme, level, cell);
            return true;
        }
    }
    exts.push(extension_json(url, scheme, level, cell));
    true
}

/// Recompute every computable cell already on the resource for a new
/// position (a position edit must not leave stale cells behind). H3 cells,
/// which fhir-pipes cannot compute, are dropped rather than left wrong. A
/// malformed entry is reported as `malformed_field` against `id` and left
/// as it is.
pub fn refresh_cells(
    obj: &mut Object,
    url: &str,
    lon: f64,
    lat: f64,
    id: &str,
    report: &mut Report,
) -> usize {
    let mut changed = 0;
    for entry in entries_of(obj, url) {
        let existing = match entry {
            Ok(c) => c,
            Err(reason) => {
                report.add("malformed_field", id, &format!("{reason}; not recomputed"));
                continue;
            }
        };
        match compute_cell(&existing.scheme, existing.level, lon, lat) {
            Some(cell) => {
                if upsert_cell(obj, url, &existing.scheme, existing.level, &cell) {
                    changed += 1;
                }
            }
            None => {
                remove_cell(obj, url, &existing.scheme, existing.level);
                changed += 1;
            }
        }
    }
    changed
}

pub fn remove_cell(obj: &mut Object, url: &str, scheme: &str, level: u32) {
    if let Some(Value::Array(exts)) = obj.get_mut("extension") {
        exts.retain(|e| {
            e.as_object()
                .and_then(|e| cell_from_extension(e, url))
                .and_then(|c| c.ok())
                .is_none_or(|c| !(c.scheme == scheme && c.level == level))
        });
        if exts.is_empty() {
            obj.remove("extension");
        }
    }
}

/// Drop every spatial-index entry, malformed ones too (the position was
/// cleared).
pub fn remove_all_cells(obj: &mut Object, url: &str) {
    if let Some(Value::Array(exts)) = obj.get_mut("extension") {
        exts.retain(|e| {
            e.as_object()
                .and_then(|e| cell_from_extension(e, url))
                .is_none()
        });
        if exts.is_empty() {
            obj.remove("extension");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://icr.healthcampaigns.org/StructureDefinition/spatial-index";

    // Independent slippy-map tile arithmetic to cross-check quadkey().
    fn tile(lon: f64, lat: f64, level: u32) -> (u64, u64) {
        let n = (1u64 << level) as f64;
        let x = ((lon + 180.0) / 360.0 * n) as u64;
        let lat = lat.to_radians();
        let y =
            ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u64;
        (x, y)
    }

    fn quadkey_from_tile(x: u64, y: u64, level: u32) -> String {
        (1..=level)
            .rev()
            .map(|i| {
                let m = 1u64 << (i - 1);
                char::from(b'0' + (x & m != 0) as u8 + 2 * (y & m != 0) as u8)
            })
            .collect()
    }

    #[test]
    fn quadkey_matches_the_slippy_tile_formula() {
        for (lon, lat, level) in [
            (9.9, 10.5, 18),
            (-12.9469, 9.0144, 18),
            (3.39, 6.45, 12),
            (-122.33, 47.61, 3),
            (0.0, 0.0, 1),
        ] {
            let (x, y) = tile(lon, lat, level);
            assert_eq!(quadkey(lon, lat, level), quadkey_from_tile(x, y, level));
        }
    }

    #[test]
    fn quadkey_is_prefix_hierarchical_and_base4() {
        let q18 = quadkey(9.9, 10.5, 18);
        assert_eq!(q18.len(), 18);
        assert!(q18.chars().all(|c| ('0'..='3').contains(&c)));
        for level in [1, 5, 10, 17] {
            assert!(q18.starts_with(&quadkey(9.9, 10.5, level)));
        }
        assert_eq!(quadkey(-90.0, 45.0, 1), "0");
        assert_eq!(quadkey(90.0, -45.0, 1), "3");
    }

    #[test]
    fn geohash_known_value() {
        // Wikipedia's worked example.
        assert_eq!(geohash(10.40744, 57.64911, 11), "u4pruydqqvj");
        assert_eq!(geohash(10.40744, 57.64911, 4), "u4pr");
    }

    #[test]
    fn parse_scheme_level_validates() {
        assert_eq!(
            parse_scheme_level("quadkey:18").unwrap(),
            ("quadkey".into(), 18)
        );
        assert_eq!(
            parse_scheme_level("geohash:8").unwrap(),
            ("geohash".into(), 8)
        );
        for bad in [
            "quadkey",
            "quadkey:x",
            "s2:5",
            "quadkey:24",
            "quadkey:0",
            "geohash:13",
            "h3:9",
        ] {
            assert!(parse_scheme_level(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_level_outside_the_scheme_range_is_malformed_and_not_computed() {
        let entry = |scheme: &str, level: u64| {
            json!({"url": URL, "extension": [
                {"url": "system", "valueCode": scheme},
                {"url": "level", "valueUnsignedInt": level},
                {"url": "cell", "valueString": "0"},
            ]})
        };
        for (scheme, level) in [
            ("quadkey", 0),
            ("quadkey", 24),
            ("quadkey", 70),
            ("quadkey", u64::from(u32::MAX)),
            ("quadkey", u64::from(u32::MAX) + 1),
            ("geohash", 0),
            ("geohash", 13),
            ("h3", 16),
        ] {
            let ext = entry(scheme, level);
            let parsed = cell_from_extension(ext.as_object().unwrap(), URL).unwrap();
            assert!(
                matches!(&parsed, Err(m) if m.contains("out of range")),
                "{scheme}:{level} {parsed:?}"
            );
            if let Ok(level) = u32::try_from(level) {
                assert_eq!(compute_cell(scheme, level, 9.9, 10.5), None);
            }
        }
        for (scheme, level) in [("quadkey", 1), ("quadkey", 23), ("geohash", 12), ("h3", 0)] {
            let ext = entry(scheme, level);
            assert!(
                cell_from_extension(ext.as_object().unwrap(), URL)
                    .unwrap()
                    .is_ok()
            );
        }
        let other = json!({"url": "https://example.org/other"});
        assert!(cell_from_extension(other.as_object().unwrap(), URL).is_none());
    }

    #[test]
    fn cells_round_trip_through_the_extension_and_upsert_replaces_in_place() {
        let mut obj = Object::new();
        assert!(upsert_cell(&mut obj, URL, "quadkey", 18, "0123"));
        assert!(
            !upsert_cell(&mut obj, URL, "quadkey", 18, "0123"),
            "unchanged"
        );
        assert!(upsert_cell(&mut obj, URL, "quadkey", 10, "01"));
        assert!(
            upsert_cell(&mut obj, URL, "quadkey", 18, "0321"),
            "replaced"
        );
        let cells = cells_of(&obj, URL);
        assert_eq!(cells.len(), 2);
        assert_eq!(deepest_quadkey(&cells).unwrap().cell, "0321");
        assert_eq!(obj["extension"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn refresh_recomputes_existing_cells_for_a_new_position() {
        let mut obj = Object::new();
        upsert_cell(&mut obj, URL, "quadkey", 18, &quadkey(9.9, 10.5, 18));
        upsert_cell(&mut obj, URL, "geohash", 6, &geohash(9.9, 10.5, 6));
        upsert_cell(&mut obj, URL, "h3", 9, "8928308280fffff");
        let mut report = Report::default();
        assert_eq!(
            refresh_cells(&mut obj, URL, 3.39, 6.45, "a", &mut report),
            3
        );
        assert_eq!(report.counts().len(), 0);
        let cells = cells_of(&obj, URL);
        assert_eq!(cells.len(), 2, "h3 dropped: fhir-pipes cannot recompute it");
        assert_eq!(
            deepest_quadkey(&cells).unwrap().cell,
            quadkey(3.39, 6.45, 18)
        );
        remove_all_cells(&mut obj, URL);
        assert!(obj.get("extension").is_none());
    }
}
