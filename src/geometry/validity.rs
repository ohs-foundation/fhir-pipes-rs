//! Structural validity checks for polygon boundaries, without `geo`'s
//! `Validation` trait: that trait's self-intersection check is an O(n^2)
//! all-pairs loop (see `geo::algorithm::validation::utils::linestring_has_self_intersection`),
//! which is unusable at registry scale -- 37ms for a 4,000-vertex ring,
//! 3.7s for 40,000, paid twice per polygon. This module replaces it with a
//! Bentley-Ottmann sweep (`geo::Intersections`), which finds the same
//! self-intersections in O(n log n).
//!
//! Only ring-level structure is checked: minimum distinct points, and
//! self-intersection, for the exterior ring and every interior (hole) ring.
//! Whether a hole actually lies inside its exterior, whether holes overlap
//! each other, and whether the parts of a MultiPolygon overlap (a
//! multi-feature fold can produce this) is deliberately NOT checked here --
//! that would need a second, more expensive geometric pass, and fhir-pipes' job
//! is to report registry problems, not to silently decide which ones
//! matter. This is stricter than `geo`'s own `Validation` in one respect:
//! an empty interior ring is reported as having fewer than 3 distinct
//! points rather than skipped.
//!
//! Non-finite coordinates (NaN, +-inf) cannot occur: geometry is parsed
//! from GeoJSON via `serde_json`, which rejects out-of-range numbers at
//! parse time.

use geo::{Coord, Line, LineString, MultiPolygon, Polygon};

/// Returns the first reason `geom` fails structural validity, if any. Points
/// are always valid; only `Polygon` and `MultiPolygon` are inspected.
pub fn check(geom: &geo::Geometry<f64>) -> Result<(), String> {
    match geom {
        geo::Geometry::Polygon(p) => check_polygon(p),
        geo::Geometry::MultiPolygon(mp) => check_multi_polygon(mp),
        _ => Ok(()),
    }
}

fn check_multi_polygon(mp: &MultiPolygon<f64>) -> Result<(), String> {
    for p in &mp.0 {
        check_polygon(p)?;
    }
    Ok(())
}

fn check_polygon(p: &Polygon<f64>) -> Result<(), String> {
    check_ring(p.exterior(), None)?;
    for (i, ring) in p.interiors().iter().enumerate() {
        check_ring(ring, Some(i))?;
    }
    Ok(())
}

fn check_ring(ls: &LineString<f64>, interior_index: Option<usize>) -> Result<(), String> {
    let label = match interior_index {
        None => "exterior ring".to_string(),
        Some(i) => format!("interior ring {i}"),
    };

    // A closed ring repeats its first coordinate as its last, so 4
    // coordinates is the minimum that can describe a (possibly degenerate)
    // triangle; below that, or with fewer than 3 distinct points among
    // them, the ring cannot enclose any area.
    let coords = &ls.0;
    if coords.len() < 4 || count_distinct(&coords[..coords.len() - 1]) < 3 {
        return Err(format!("{label} must have at least 3 distinct points"));
    }
    if ring_self_intersects(ls) {
        return Err(format!("{label} has a self-intersection"));
    }
    Ok(())
}

/// Distinct point count via sort + dedup: O(n log n), not the O(n^2) an
/// all-pairs comparison would cost on a large ring.
fn count_distinct(coords: &[Coord<f64>]) -> usize {
    let mut pts: Vec<(f64, f64)> = coords.iter().map(|c| (c.x, c.y)).collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    pts.dedup();
    pts.len()
}

/// Bentley-Ottmann sweep over the ring's segments, instead of the naive
/// all-pairs loop.
fn ring_self_intersects(ls: &LineString<f64>) -> bool {
    let lines: Vec<Line<f64>> = ls.lines().collect();
    for (l1, l2, _) in geo::Intersections::from_iter(lines.iter().copied()) {
        // Adjacent segments legitimately share an endpoint; so do the first and last.
        if l1.end == l2.start || l2.end == l1.start {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{polygon, Geometry};

    #[test]
    fn a_clean_triangle_is_valid() {
        let p = polygon![(x: 0., y: 0.), (x: 4., y: 0.), (x: 2., y: 3.), (x: 0., y: 0.)];
        assert_eq!(check(&Geometry::Polygon(p)), Ok(()));
    }

    #[test]
    fn a_bowtie_ring_self_intersects() {
        let p = polygon![(x: 0., y: 0.), (x: 2., y: 2.), (x: 2., y: 0.), (x: 0., y: 2.), (x: 0., y: 0.)];
        let err = check(&Geometry::Polygon(p)).unwrap_err();
        assert!(err.contains("self-intersection"), "{err}");
    }

    #[test]
    fn a_degenerate_ring_is_reported() {
        let p = polygon![(x: 1., y: 2.), (x: 3., y: 4.), (x: 1., y: 2.)];
        let err = check(&Geometry::Polygon(p)).unwrap_err();
        assert!(err.contains("at least 3 distinct points"), "{err}");
    }

    #[test]
    fn a_bad_hole_is_reported_with_its_index() {
        let p = Polygon::new(
            LineString::from(vec![(0., 0.), (10., 0.), (10., 10.), (0., 10.), (0., 0.)]),
            vec![LineString::from(vec![
                (2., 2.),
                (8., 8.),
                (8., 2.),
                (2., 8.),
                (2., 2.),
            ])],
        );
        let err = check(&Geometry::Polygon(p)).unwrap_err();
        assert!(err.starts_with("interior ring 0"), "{err}");
        assert!(err.contains("self-intersection"), "{err}");
    }
}
