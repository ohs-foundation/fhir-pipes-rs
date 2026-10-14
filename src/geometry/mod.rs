//! One Location's geometry, computed in two passes over the same
//! classification: pass one (`summarize`) validates and reports, keeping
//! only the small summary (kind + bbox); pass two (`build`) trusts pass
//! one's validation and only computes the representative point, producing
//! the full result (geometry + WKB + point).
//!
//! Invalid geometry is reported and passed through unchanged. fhir-pipes does not
//! repair; that hides registry problems from the people who must fix them.

pub mod geojson;
pub mod validity;
pub mod wkb;

use geo::{BoundingRect, Geometry, InteriorPoint, Intersects};

pub use geojson::{convert, kind_name, parse_boundary};
pub use wkb::to_wkb;

use crate::fhir::{Boundary, Location};
use crate::report::Report;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeomKind {
    Point,
    Polygon,
}

impl GeomKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GeomKind::Point => "point",
            GeomKind::Polygon => "polygon",
        }
    }
}

/// What pass one retains per Location: 40 bytes, no vertex data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeometrySummary {
    pub kind: GeomKind,
    /// xmin, ymin, xmax, ymax
    pub bbox: [f64; 4],
}

/// What pass two produces per Location.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryResult {
    pub kind: GeomKind,
    pub geometry: Geometry<f64>,
    pub bbox: [f64; 4],
    /// Representative point: the FHIR position when present, else a point inside the polygon.
    pub lon: f64,
    pub lat: f64,
}

impl GeometryResult {
    pub fn wkb(&self) -> Vec<u8> {
        to_wkb(&self.geometry)
    }
}

fn bbox_of(geom: &Geometry<f64>) -> Option<[f64; 4]> {
    let r = geom.bounding_rect()?;
    Some([r.min().x, r.min().y, r.max().x, r.max().y])
}

/// Parse the boundary (if inline) and classify it, falling back to the FHIR
/// position when there is no usable boundary. Reports and returns `None`
/// when the Location has nothing usable at all. Shared by both passes so
/// they always agree on kind and bbox.
fn classify(loc: &Location, report: &mut Report) -> Option<(Geometry<f64>, GeomKind, [f64; 4])> {
    let polygon = match &loc.boundary {
        Some(Boundary::Inline(bytes)) => parse_boundary(bytes, &loc.id, report),
        Some(Boundary::Url(_)) | None => None,
    };

    if let Some(geom) = polygon {
        let kind = match &geom {
            Geometry::Point(_) => GeomKind::Point,
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => GeomKind::Polygon,
            other => {
                report.add(
                    "geometry_unexpected_type",
                    &loc.id,
                    &format!("unsupported type {}", kind_name(other)),
                );
                return None;
            }
        };
        let Some(bbox) = bbox_of(&geom) else {
            report.add("geometry_empty", &loc.id, "empty geometry");
            return None;
        };
        return Some((geom, kind, bbox));
    }

    if let Some((lon, lat)) = loc.position {
        return Some((
            Geometry::Point(geo::Point::new(lon, lat)),
            GeomKind::Point,
            [lon, lat, lon, lat],
        ));
    }

    report.add("no_geometry", &loc.id, "no position and no usable boundary");
    None
}

/// Pass one: classify, validate the boundary's structure, cross-check the
/// FHIR position against it, and report. Keeps only kind and bbox -- no
/// coordinates, no representative point.
pub fn summarize(loc: &Location, report: &mut Report) -> Option<GeometrySummary> {
    let (geom, kind, bbox) = classify(loc, report)?;

    if kind == GeomKind::Polygon {
        if let Err(reason) = validity::check(&geom) {
            report.add(
                "geometry_invalid",
                &loc.id,
                &format!("{reason}; written as is"),
            );
        }
    }

    // Checked for both kinds: a Point boundary can also disagree with the
    // FHIR position. `Intersects` (not `Contains`) so a position exactly on
    // a polygon's boundary still counts as inside; for a Point boundary,
    // `Intersects` is exact coordinate equality. The position-only path in
    // `classify` synthesises the geometry from the position itself, so it
    // can never false-positive here.
    if let Some((lon, lat)) = loc.position {
        if !geom.intersects(&geo::Point::new(lon, lat)) {
            report.add(
                "position_outside_boundary",
                &loc.id,
                &format!("position ({lon}, {lat}) falls outside the Location's own boundary"),
            );
        }
    }

    Some(GeometrySummary { kind, bbox })
}

/// Pass two: classify and pick the representative point. Trusts pass one to
/// have already validated the boundary; does not check validity or the
/// position again.
pub fn build(loc: &Location, report: &mut Report) -> Option<GeometryResult> {
    let (geom, kind, bbox) = classify(loc, report)?;

    let (lon, lat) = match (loc.position, &geom) {
        (Some(p), _) => p,
        (None, Geometry::Point(p)) => (p.x(), p.y()),
        (None, g) => match g.interior_point() {
            Some(p) => (p.x(), p.y()),
            // `interior_point` only returns `None` for an empty geometry,
            // and `classify` already rejected those (via `bbox_of`), so
            // this should be unreachable in practice. Report rather than
            // fall back to the bbox centre, which can land outside a
            // crescent-shaped polygon -- fhir-pipes promises the representative
            // point is inside the boundary.
            None => {
                report.add(
                    "geometry_no_interior_point",
                    &loc.id,
                    "no interior point could be computed for a non-empty geometry",
                );
                return None;
            }
        },
    };

    Some(GeometryResult {
        kind,
        geometry: geom,
        bbox,
        lon,
        lat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fhir::{Boundary, Location};
    use crate::report::Report;

    fn loc(position: Option<(f64, f64)>, boundary: Option<&str>) -> Location {
        Location {
            id: "x".into(),
            position,
            boundary: boundary.map(|b| Boundary::Inline(b.as_bytes().to_vec())),
            ..Default::default()
        }
    }
    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[3,6],[4,6],[4,7],[3,7],[3,6]]]}"#;
    const BOWTIE: &str = r#"{"type":"Polygon","coordinates":[[[0,0],[2,2],[2,0],[0,2],[0,0]]]}"#;

    #[test]
    fn position_only_is_a_point() {
        let mut r = Report::default();
        let g = build(&loc(Some((3.25, 6.25)), None), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Point);
        assert_eq!((g.lon, g.lat), (3.25, 6.25));
        assert_eq!(g.bbox, [3.25, 6.25, 3.25, 6.25]);
    }

    #[test]
    fn polygon_gets_an_interior_representative_point() {
        let mut r = Report::default();
        let g = build(&loc(None, Some(SQUARE)), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Polygon);
        assert!(g.lon > 3.0 && g.lon < 4.0 && g.lat > 6.0 && g.lat < 7.0);
        assert_eq!(g.bbox, [3.0, 6.0, 4.0, 7.0]);
    }

    #[test]
    fn polygon_plus_position_uses_the_position_as_representative_point() {
        let mut r = Report::default();
        let g = build(&loc(Some((3.2, 6.2)), Some(SQUARE)), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Polygon);
        assert_eq!((g.lon, g.lat), (3.2, 6.2));
    }

    #[test]
    fn invalid_polygon_is_kept_and_reported_not_repaired() {
        let mut r = Report::default();
        let s = summarize(&loc(None, Some(BOWTIE)), &mut r).unwrap();
        assert_eq!(s.kind, GeomKind::Polygon);
        assert_eq!(r.count("geometry_invalid"), 1);
        assert_eq!(r.count("geometry_repaired"), 0);
        let detail = &r
            .issues
            .iter()
            .find(|i| i.kind == "geometry_invalid")
            .unwrap()
            .detail;
        assert!(detail.contains("self-intersection"), "{detail}");
    }

    #[test]
    fn build_does_not_report_validity() {
        let mut r = Report::default();
        let g = build(&loc(None, Some(BOWTIE)), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Polygon);
        assert_eq!(r.counts().len(), 0);
    }

    #[test]
    fn nothing_usable_is_reported_as_no_geometry() {
        let mut r = Report::default();
        assert!(build(&loc(None, None), &mut r).is_none());
        assert_eq!(r.count("no_geometry"), 1);
        let mut r = Report::default();
        assert!(build(
            &loc(
                None,
                Some(r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#)
            ),
            &mut r
        )
        .is_none());
        assert_eq!(r.count("geometry_unexpected_type"), 1);
    }

    #[test]
    fn empty_polygon_is_reported_not_written() {
        let mut r = Report::default();
        assert!(build(
            &loc(None, Some(r#"{"type":"Polygon","coordinates":[]}"#)),
            &mut r
        )
        .is_none());
        assert_eq!(r.count("geometry_empty"), 1);
    }

    #[test]
    fn summarize_matches_build_but_keeps_no_geometry() {
        let mut r = Report::default();
        let s = summarize(&loc(Some((3.2, 6.2)), Some(SQUARE)), &mut r).unwrap();
        assert_eq!(s.kind, GeomKind::Polygon);
        assert_eq!(s.bbox, [3.0, 6.0, 4.0, 7.0]);
        assert_eq!(std::mem::size_of::<GeometrySummary>(), 40);
    }

    #[test]
    fn degenerate_ring_is_reported_with_reason() {
        let mut r = Report::default();
        let s = summarize(
            &loc(
                None,
                Some(r#"{"type":"Polygon","coordinates":[[[1,2],[3,4],[1,2]]]}"#),
            ),
            &mut r,
        )
        .unwrap();
        assert_eq!(s.kind, GeomKind::Polygon);
        let detail = &r
            .issues
            .iter()
            .find(|i| i.kind == "geometry_invalid")
            .unwrap()
            .detail;
        assert!(detail.contains("at least 3 distinct points"), "{detail}");
    }

    #[test]
    fn large_valid_ring_validates_quickly() {
        // A 20,000-vertex circle: no reason for geometry_invalid, and the
        // Bentley-Ottmann sweep should make this fast even in a debug
        // build. The naive O(n^2) self-intersection check this replaces
        // would take seconds at this size.
        let n = 20_000;
        let mut coords = Vec::with_capacity(n + 1);
        for i in 0..n {
            let theta = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
            coords.push(format!("[{},{}]", theta.cos(), theta.sin()));
        }
        coords.push(coords[0].clone());
        let geojson = format!(
            r#"{{"type":"Polygon","coordinates":[[{}]]}}"#,
            coords.join(",")
        );

        let mut r = Report::default();
        let start = std::time::Instant::now();
        let s = summarize(&loc(None, Some(&geojson)), &mut r).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(s.kind, GeomKind::Polygon);
        assert_eq!(r.count("geometry_invalid"), 0);
        // Measured ~25ms in a debug build, ~1.7ms in release, so 500ms
        // leaves a wide margin either way; the naive O(n^2) check this
        // replaces would take seconds at this size.
        assert!(
            elapsed.as_secs_f64() < 0.5,
            "validating a 20,000-vertex ring took {elapsed:?}"
        );
    }

    #[test]
    fn position_outside_own_boundary_is_reported() {
        let mut r = Report::default();
        summarize(&loc(Some((10.0, 10.0)), Some(SQUARE)), &mut r).unwrap();
        assert_eq!(r.count("position_outside_boundary"), 1);

        let mut r = Report::default();
        summarize(&loc(Some((3.5, 6.5)), Some(SQUARE)), &mut r).unwrap();
        assert_eq!(r.count("position_outside_boundary"), 0);
    }

    #[test]
    fn point_boundary_disagreeing_with_position_is_reported() {
        let mut r = Report::default();
        summarize(
            &loc(
                Some((5.0, 6.0)),
                Some(r#"{"type":"Point","coordinates":[1,2]}"#),
            ),
            &mut r,
        )
        .unwrap();
        assert_eq!(r.count("position_outside_boundary"), 1);
    }

    #[test]
    fn unparseable_boundary_with_position_still_yields_point() {
        let mut r = Report::default();
        let g = build(&loc(Some((1.0, 2.0)), Some("not json")), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Point);
        assert_eq!((g.lon, g.lat), (1.0, 2.0));
        assert_eq!(r.count("boundary_unparseable"), 1);
    }

    #[test]
    fn multipolygon_is_polygon_kind_with_interior_point() {
        use geo::Contains;
        let mp = r#"{"type":"MultiPolygon","coordinates":[
            [[[0,0],[1,0],[1,1],[0,1],[0,0]]],
            [[[10,10],[11,10],[11,11],[10,11],[10,10]]]
        ]}"#;
        let mut r = Report::default();
        let g = build(&loc(None, Some(mp)), &mut r).unwrap();
        assert_eq!(g.kind, GeomKind::Polygon);
        let point = geo::Point::new(g.lon, g.lat);
        assert!(g.geometry.contains(&point));
    }
}
