//! Boundary attachment bytes -> a `geo` geometry. Accepts a bare geometry, a
//! Feature, or a FeatureCollection, as the Python did.
//!
//! Geometries are converted to 2D: Z and M coordinates are dropped and
//! reported as `boundary_z_dropped`. `GeometryCollection` and empty
//! geometries pass through here and are classified by the geometry stage
//! that follows.

use geo::Geometry;
use geojson::{GeoJson, GeometryValue, Position};

use crate::report::Report;

/// A position with fewer than 2 numbers makes `geojson` 1.0's conversion to
/// `geo-types` index out of bounds and panic (it reads `position[0]` and
/// `position[1]` unconditionally). `Ok(true)` means at least one position in
/// this geometry carried a 3rd (or later) number that will be dropped.
fn validate_position(p: &Position) -> Result<bool, ()> {
    if p.len() < 2 {
        Err(())
    } else {
        Ok(p.len() > 2)
    }
}

fn validate_positions_seq(positions: &[Position]) -> Result<bool, ()> {
    let mut has_extra = false;
    for p in positions {
        has_extra |= validate_position(p)?;
    }
    Ok(has_extra)
}

fn validate_positions(value: &GeometryValue) -> Result<bool, ()> {
    match value {
        GeometryValue::Point { coordinates } => validate_position(coordinates),
        GeometryValue::MultiPoint { coordinates } | GeometryValue::LineString { coordinates } => {
            validate_positions_seq(coordinates)
        }
        GeometryValue::MultiLineString { coordinates } | GeometryValue::Polygon { coordinates } => {
            let mut has_extra = false;
            for ring in coordinates {
                has_extra |= validate_positions_seq(ring)?;
            }
            Ok(has_extra)
        }
        GeometryValue::MultiPolygon { coordinates } => {
            let mut has_extra = false;
            for polygon in coordinates {
                for ring in polygon {
                    has_extra |= validate_positions_seq(ring)?;
                }
            }
            Ok(has_extra)
        }
        GeometryValue::GeometryCollection { geometries } => {
            let mut has_extra = false;
            for g in geometries {
                has_extra |= validate_positions(&g.value)?;
            }
            Ok(has_extra)
        }
    }
}

/// GeoJSON geometry -> 2D `geo` geometry. `Ok((geom, true))` means a
/// third coordinate was dropped. Shared by the boundary path (which
/// reports as `boundary_*`) and diff's input path (`geometry_*`).
pub fn convert(g: geojson::Geometry) -> Result<(Geometry<f64>, bool), String> {
    let has_extra = validate_positions(&g.value)
        .map_err(|()| "coordinate position with fewer than 2 numbers".to_string())?;
    let geom = Geometry::<f64>::try_from(g).map_err(|err| err.to_string())?;
    Ok((geom, has_extra))
}

fn to_geo(g: geojson::Geometry, id: &str, report: &mut Report) -> Option<Geometry<f64>> {
    match convert(g) {
        Ok((geom, has_extra)) => {
            if has_extra {
                report.add("boundary_z_dropped", id, "3D coordinates flattened to 2D");
            }
            Some(geom)
        }
        Err(msg) => {
            report.add("boundary_unparseable", id, &msg);
            None
        }
    }
}

pub fn parse_boundary(bytes: &[u8], id: &str, report: &mut Report) -> Option<Geometry<f64>> {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(err) => {
            report.add("boundary_unparseable", id, &err.to_string());
            return None;
        }
    };
    let parsed: GeoJson = match text.parse() {
        Ok(p) => p,
        Err(err) => {
            report.add("boundary_unparseable", id, &err.to_string());
            return None;
        }
    };
    match parsed {
        GeoJson::Geometry(g) => to_geo(g, id, report),
        GeoJson::Feature(f) => match f.geometry {
            Some(g) => to_geo(g, id, report),
            None => {
                report.add("boundary_unparseable", id, "Feature has no geometry");
                None
            }
        },
        GeoJson::FeatureCollection(fc) => {
            let geoms: Vec<geojson::Geometry> =
                fc.features.into_iter().filter_map(|f| f.geometry).collect();
            match geoms.len() {
                0 => {
                    report.add(
                        "boundary_unparseable",
                        id,
                        "FeatureCollection has no geometry",
                    );
                    None
                }
                1 => to_geo(geoms.into_iter().next().unwrap(), id, report),
                n => {
                    report.add(
                        "boundary_multi_feature",
                        id,
                        &format!("{n} features folded into one geometry"),
                    );
                    let mut polygons = Vec::new();
                    let mut dropped_count = 0usize;
                    let mut dropped_kinds: Vec<&'static str> = Vec::new();
                    for g in geoms {
                        match to_geo(g, id, report) {
                            Some(Geometry::Polygon(p)) => polygons.push(p),
                            Some(Geometry::MultiPolygon(mp)) => polygons.extend(mp.0),
                            Some(other) => {
                                dropped_count += 1;
                                let k = kind_name(&other);
                                if !dropped_kinds.contains(&k) {
                                    dropped_kinds.push(k);
                                }
                            }
                            None => {}
                        }
                    }
                    if dropped_count > 0 {
                        report.add(
                            "boundary_part_dropped",
                            id,
                            &format!(
                                "{dropped_count} non-polygon part(s) ({}) dropped from fold",
                                dropped_kinds.join(", ")
                            ),
                        );
                    }
                    if polygons.is_empty() {
                        None
                    } else {
                        Some(Geometry::MultiPolygon(geo::MultiPolygon(polygons)))
                    }
                }
            }
        }
    }
}

pub fn kind_name(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) => "Line",
        Geometry::LineString(_) => "LineString",
        Geometry::Polygon(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
        Geometry::Rect(_) => "Rect",
        Geometry::Triangle(_) => "Triangle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Report;

    #[test]
    fn bare_geometry_feature_and_collection_all_parse() {
        let mut r = Report::default();
        let poly = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}"#;
        assert!(matches!(
            parse_boundary(poly.as_bytes(), "a", &mut r),
            Some(geo::Geometry::Polygon(_))
        ));
        let feature = format!(r#"{{"type":"Feature","properties":{{}},"geometry":{poly}}}"#);
        assert!(matches!(
            parse_boundary(feature.as_bytes(), "a", &mut r),
            Some(geo::Geometry::Polygon(_))
        ));
        let fc = format!(r#"{{"type":"FeatureCollection","features":[{feature}]}}"#);
        assert!(matches!(
            parse_boundary(fc.as_bytes(), "a", &mut r),
            Some(geo::Geometry::Polygon(_))
        ));
        assert_eq!(r.counts().len(), 0);
    }

    #[test]
    fn multi_feature_collection_folds_into_multipolygon_and_reports() {
        let mut r = Report::default();
        let poly = r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#;
        let fc = format!(r#"{{"type":"FeatureCollection","features":[{poly},{poly}]}}"#);
        match parse_boundary(fc.as_bytes(), "a", &mut r) {
            Some(geo::Geometry::MultiPolygon(mp)) => assert_eq!(mp.0.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.count("boundary_multi_feature"), 1);
    }

    #[test]
    fn garbage_is_reported_as_unparseable() {
        let mut r = Report::default();
        assert!(parse_boundary(b"not json", "a", &mut r).is_none());
        assert!(
            parse_boundary(
                br#"{"type":"FeatureCollection","features":[]}"#,
                "a",
                &mut r
            )
            .is_none()
        );
        assert_eq!(r.count("boundary_unparseable"), 2);
    }

    #[test]
    fn short_coordinate_position_is_reported_not_a_panic() {
        let mut r = Report::default();
        let poly = br#"{"type":"Polygon","coordinates":[[[0],[1,0],[1,1],[0,0]]]}"#;
        assert!(parse_boundary(poly, "a", &mut r).is_none());
        assert_eq!(r.count("boundary_unparseable"), 1);

        let mut r = Report::default();
        let multi_with_short_hole_position = br#"{"type":"MultiPolygon","coordinates":[[[[0,0],[10,0],[10,10],[0,10],[0,0]],[[2,2],[2],[8,8],[2,8],[2,2]]]]}"#;
        assert!(parse_boundary(multi_with_short_hole_position, "a", &mut r).is_none());
        assert_eq!(r.count("boundary_unparseable"), 1);
    }

    #[test]
    fn z_coordinates_are_dropped_and_reported() {
        let mut r = Report::default();
        let poly = br#"{"type":"Polygon","coordinates":[[[0,0,100],[1,0,100],[1,1,100],[0,1,100],[0,0,100]]]}"#;
        let geom = parse_boundary(poly, "a", &mut r).expect("should parse despite Z");
        match &geom {
            geo::Geometry::Polygon(p) => {
                let first = p.exterior().0[0];
                assert_eq!((first.x, first.y), (0.0, 0.0));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(r.count("boundary_z_dropped"), 1);

        let bytes = crate::geometry::to_wkb(&geom);
        assert_eq!(&bytes[1..5], &3u32.to_le_bytes());
    }

    #[test]
    fn mixed_fold_reports_dropped_parts_once() {
        let mut r = Report::default();
        let poly = r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#;
        let point_a = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]}}"#;
        let point_b = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1,1]}}"#;
        let fc =
            format!(r#"{{"type":"FeatureCollection","features":[{poly},{point_a},{point_b}]}}"#);
        match parse_boundary(fc.as_bytes(), "a", &mut r) {
            Some(geo::Geometry::MultiPolygon(mp)) => assert_eq!(mp.0.len(), 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.count("boundary_multi_feature"), 1);
        assert_eq!(r.count("boundary_part_dropped"), 1);
        assert_eq!(r.count("boundary_unparseable"), 0);
    }

    #[test]
    fn null_geometries_are_skipped() {
        let mut r = Report::default();
        let feature_with_null = br#"{"type":"Feature","geometry":null}"#;
        assert!(parse_boundary(feature_with_null, "a", &mut r).is_none());
        assert_eq!(r.count("boundary_unparseable"), 1);

        let mut r = Report::default();
        let poly = r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#;
        let null_feature = r#"{"type":"Feature","geometry":null}"#;
        let fc = format!(r#"{{"type":"FeatureCollection","features":[{null_feature},{poly}]}}"#);
        assert!(matches!(
            parse_boundary(fc.as_bytes(), "a", &mut r),
            Some(geo::Geometry::Polygon(_))
        ));
        assert_eq!(r.count("boundary_multi_feature"), 0);
    }
}
