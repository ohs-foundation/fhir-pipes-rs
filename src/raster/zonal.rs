//! Zonal sum under the pixel-centroid rule: a pixel belongs to the polygon
//! its centre falls in. Non-overlapping polygons therefore partition the
//! pixels, and children sum exactly to their parent — the property a
//! denominator hierarchy needs. (pixel's `all_touched` counts every pixel
//! a polygon touches, over-counting along every shared boundary; coverage
//! weighting is exact too but needs polygon∩pixel clipping per edge pixel.)
//!
//! Scanline fill with the even-odd rule over every ring of the geometry
//! (exteriors and holes, every part of a MultiPolygon), one raster row at a
//! time; only the tiles the row's spans touch are decoded, and at most one
//! tile row is held in memory.
//!
//! Ties: a pixel centre exactly on a boundary belongs to the polygon on its
//! east (larger longitude) and its north (larger latitude) side — min-closed,
//! max-open in both axes — so a shared edge is never claimed twice. Even-odd
//! runs across every ring of a MultiPolygon at once, so genuinely
//! self-overlapping parts cancel rather than double-count; that is the safer
//! failure and what makes winding order irrelevant.
//!
//! Negative values are data (they are counted) but contribute nothing to the
//! sum: a population grid's resampling can leave small negatives, and a head
//! count must never be reduced by them.

use std::collections::HashMap;

use geo::{BoundingRect, Coord, Geometry, LineString, Polygon};

use super::{GeoTransform, Raster, Tile, is_data};
use crate::error::{PipesError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ZonalSum {
    pub sum: f64,
    /// Pixels with data (not nodata) whose centre fell inside.
    pub pixels: u64,
}

impl ZonalSum {
    fn add(&mut self, v: f32, nodata: Option<f32>) {
        if is_data(v, nodata) {
            self.sum += f64::from(v.max(0.0));
            self.pixels += 1;
        }
    }
}

/// Every ring of an areal geometry. `None` for anything without area.
fn rings_of(geom: &Geometry<f64>) -> Option<Vec<&LineString<f64>>> {
    fn push<'a>(p: &'a Polygon<f64>, out: &mut Vec<&'a LineString<f64>>) {
        out.push(p.exterior());
        out.extend(p.interiors().iter());
    }
    let mut out = Vec::new();
    match geom {
        Geometry::Polygon(p) => push(p, &mut out),
        Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| push(p, &mut out)),
        _ => return None,
    }
    Some(out)
}

/// Longitudes where the horizontal line at `lat` crosses a ring edge, sorted.
/// Half-open rule — an endpoint exactly on the line counts for the edge on
/// one side only — so a vertex on the scanline is never counted twice.
fn crossings(rings: &[&LineString<f64>], lat: f64, out: &mut Vec<f64>) {
    out.clear();
    for ring in rings {
        let pts = &ring.0;
        if pts.len() < 2 {
            continue;
        }
        let mut j = pts.len() - 1;
        for i in 0..pts.len() {
            let (a, b): (Coord<f64>, Coord<f64>) = (pts[i], pts[j]);
            if (a.y > lat) != (b.y > lat) {
                out.push(a.x + (lat - a.y) * (b.x - a.x) / (b.y - a.y));
            }
            j = i;
        }
    }
    out.sort_unstable_by(f64::total_cmp);
}

/// Inclusive column range whose centres lie in [xa, xb), clipped to the raster.
fn columns(t: &GeoTransform, width: u32, xa: f64, xb: f64) -> Option<(u32, u32)> {
    let start = (t.col_of(xa) - 0.5).ceil().max(0.0);
    let end = ((t.col_of(xb) - 0.5).ceil() - 1.0).min(f64::from(width) - 1.0);
    (end >= start).then_some((start as u32, end as u32))
}

/// Sum of the raster's data pixels whose centres fall inside `geom`.
///
/// `geom` must be in the raster's coordinate system (fhir-pipes' boundaries are
/// WGS84 GeoJSON and `GeoTiff` refuses projected rasters, so this holds by
/// construction).
pub fn zonal_sum(raster: &mut impl Raster, geom: &Geometry<f64>) -> Result<ZonalSum> {
    let rings = rings_of(geom).ok_or_else(|| {
        PipesError::Usage("zonal sum needs a Polygon or MultiPolygon".to_string())
    })?;
    let Some(rect) = geom.bounding_rect() else {
        return Ok(ZonalSum::default());
    };
    let t = raster.transform();
    let (tw, th) = raster.tile_size();
    let width = raster.width();
    let nodata = raster.nodata();
    // Rows whose centres could lie within the bbox, clipped to the raster,
    // with a row of slack on each side: `row_of` and `lat_center` are
    // different expressions, so a boundary within an ULP of a row centre can
    // disagree between them. The prefilter must never exclude a row the
    // even-odd fill would claim; an extra empty row costs one `crossings` call.
    let r0 = (t.row_of(rect.max().y) - 1.5).ceil().max(0.0);
    let r1 = (t.row_of(rect.min().y) + 0.5)
        .floor()
        .min(f64::from(raster.height()) - 1.0);
    if r1 < r0 {
        return Ok(ZonalSum::default());
    }
    let (r0, r1) = (r0 as u32, r1 as u32);

    let mut total = ZonalSum::default();
    let mut xs: Vec<f64> = Vec::new();
    let mut tiles: HashMap<u32, Tile> = HashMap::new();
    let mut current_ty: Option<u32> = None;
    for row in r0..=r1 {
        let ty = row / th;
        if current_ty != Some(ty) {
            tiles.clear();
            current_ty = Some(ty);
        }
        crossings(&rings, t.lat_center(row), &mut xs);
        for pair in xs.as_chunks::<2>().0 {
            let Some((c0, c1)) = columns(&t, width, pair[0], pair[1]) else {
                continue;
            };
            // One tile lookup per tile the span crosses, not per pixel.
            let mut col = c0;
            while col <= c1 {
                let tx = col / tw;
                let last = c1.min(tx * tw + (tw - 1));
                let tile = match tiles.entry(tx) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let tile = raster.read_tile(tx, ty)?;
                        e.insert(tile)
                    }
                };
                let start = ((row - ty * th) * tile.width) as usize;
                let (from, to) = ((col - tx * tw) as usize, (last - tx * tw) as usize);
                for &v in &tile.data[start + from..=start + to] {
                    total.add(v, nodata);
                }
                col = last + 1;
            }
        }
    }
    Ok(total)
}

/// Sum of every data pixel in the raster — the figure the per-unit sums
/// should approach when the units tile the country.
pub fn raster_total(raster: &mut impl Raster) -> Result<ZonalSum> {
    let mut total = ZonalSum::default();
    let nodata = raster.nodata();
    for ty in 0..raster.tiles_down() {
        for tx in 0..raster.tiles_across() {
            let tile = raster.read_tile(tx, ty)?;
            for &v in &tile.data {
                total.add(v, nodata);
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::MemRaster;
    use geo::{MultiPolygon, polygon};

    fn boxed(x0: f64, y0: f64, x1: f64, y1: f64) -> Geometry<f64> {
        Geometry::Polygon(
            polygon![(x: x0, y: y1), (x: x1, y: y1), (x: x1, y: y0), (x: x0, y: y0), (x: x0, y: y1)],
        )
    }

    fn sum(geom: &Geometry<f64>) -> (f64, u64) {
        let z = zonal_sum(&mut MemRaster::fixture(), geom).unwrap();
        (z.sum, z.pixels)
    }

    #[test]
    fn the_whole_raster_and_an_oversized_polygon_give_the_total() {
        assert_eq!(sum(&boxed(3.0, 4.0, 7.0, 7.0)), (1_761_153.0, 1197));
        assert_eq!(sum(&boxed(-10.0, -10.0, 50.0, 50.0)), (1_761_153.0, 1197));
        let t = raster_total(&mut MemRaster::fixture()).unwrap();
        assert_eq!((t.sum, t.pixels), (1_761_153.0, 1197));
    }

    #[test]
    fn a_small_square_counts_only_pixels_whose_centre_is_inside() {
        // 5×5 pixels, one of them nodata.
        assert_eq!(sum(&boxed(3.0, 6.5, 3.5, 7.0)), (5_074.0, 24));
    }

    #[test]
    fn adjacent_polygons_partition_the_pixels() {
        let left = sum(&boxed(3.0, 4.0, 5.0, 7.0));
        let right = sum(&boxed(5.0, 4.0, 7.0, 7.0));
        assert_eq!(left, (875_793.0, 598));
        assert_eq!(right, (885_360.0, 599));
        assert_eq!(left.0 + right.0, 1_761_153.0);
        assert_eq!(left.1 + right.1, 1197);
    }

    #[test]
    fn a_triangle_uses_sloped_crossings() {
        let tri = Geometry::Polygon(
            polygon![(x: 3.0, y: 7.0), (x: 7.0, y: 7.0), (x: 3.0, y: 4.0), (x: 3.0, y: 7.0)],
        );
        assert_eq!(sum(&tri), (578_783.0, 598));
    }

    #[test]
    fn a_polygon_across_four_tiles_reads_each_tile_once_per_tile_row() {
        assert_eq!(sum(&boxed(4.5, 5.2, 5.9, 6.7)), (214_725.0, 210));
    }

    #[test]
    fn a_hole_is_subtracted_and_a_multipolygon_is_summed() {
        let outer = polygon![(x: 3.0, y: 7.0), (x: 7.0, y: 7.0), (x: 7.0, y: 4.0), (x: 3.0, y: 4.0), (x: 3.0, y: 7.0)];
        let hole = polygon![(x: 3.0, y: 7.0), (x: 3.5, y: 7.0), (x: 3.5, y: 6.5), (x: 3.0, y: 6.5), (x: 3.0, y: 7.0)];
        let with_hole = Geometry::Polygon(geo::Polygon::new(
            outer.exterior().clone(),
            vec![hole.exterior().clone()],
        ));
        assert_eq!(sum(&with_hole), (1_756_079.0, 1173));

        let left = polygon![(x: 3.0, y: 7.0), (x: 5.0, y: 7.0), (x: 5.0, y: 4.0), (x: 3.0, y: 4.0), (x: 3.0, y: 7.0)];
        let right = polygon![(x: 5.0, y: 7.0), (x: 7.0, y: 7.0), (x: 7.0, y: 4.0), (x: 5.0, y: 4.0), (x: 5.0, y: 7.0)];
        let mp = Geometry::MultiPolygon(MultiPolygon(vec![left, right]));
        assert_eq!(sum(&mp), (1_761_153.0, 1197));
    }

    #[test]
    fn outside_the_raster_is_zero_and_a_line_is_a_usage_error() {
        assert_eq!(sum(&boxed(20.0, 20.0, 21.0, 21.0)), (0.0, 0));
        let line = Geometry::LineString(LineString::from(vec![(3.0, 4.0), (7.0, 7.0)]));
        let err = zonal_sum(&mut MemRaster::fixture(), &line).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn negative_values_count_as_pixels_but_add_nothing() {
        let mut m = MemRaster::fixture();
        // Row 0, col 1 holds 2.0; make it -2.0. Sum drops by 2, pixel count unchanged.
        m.data[1] = -2.0;
        let z = zonal_sum(&mut m, &boxed(3.0, 4.0, 7.0, 7.0)).unwrap();
        assert_eq!((z.sum, z.pixels), (1_761_151.0, 1197));
    }

    #[test]
    fn a_split_exactly_on_a_row_centre_loses_nothing() {
        // lat 6.95 is row 0's centre. Row 0 belongs to the northern polygon (min-closed).
        let top = sum(&boxed(3.0, 6.95, 7.0, 7.0));
        let bottom = sum(&boxed(3.0, 4.0, 7.0, 6.95));
        assert_eq!(top, (819.0, 39));
        assert_eq!(bottom, (1_760_334.0, 1158));
        assert_eq!(top.0 + bottom.0, 1_761_153.0);
    }

    #[test]
    fn a_split_exactly_on_a_column_centre_assigns_the_column_east() {
        // lon 3.05 is column 0's centre: [xa, xb) gives it to the eastern polygon.
        assert_eq!(sum(&boxed(3.0, 4.0, 3.05, 7.0)), (0.0, 0));
        assert_eq!(sum(&boxed(3.05, 4.0, 7.0, 7.0)), (1_761_153.0, 1197));
    }

    #[test]
    fn a_vertex_on_the_scanline_is_counted_once() {
        // Diamond with its top and bottom vertices exactly on row centres (6.95, 6.75)
        // and its side vertices on the 6.85 centre: local max, monotone pass, local min.
        let diamond = Geometry::Polygon(
            polygon![(x: 3.5, y: 6.95), (x: 3.6, y: 6.85), (x: 3.5, y: 6.75), (x: 3.4, y: 6.85), (x: 3.5, y: 6.95)],
        );
        assert_eq!(sum(&diamond), (211.0, 2));
    }

    #[test]
    fn degenerate_rings_are_empty_not_a_panic() {
        let point_ring =
            Geometry::Polygon(polygon![(x: 3.5, y: 6.5), (x: 3.5, y: 6.5), (x: 3.5, y: 6.5)]);
        let collinear = Geometry::Polygon(
            polygon![(x: 3.0, y: 6.5), (x: 5.0, y: 6.5), (x: 7.0, y: 6.5), (x: 3.0, y: 6.5)],
        );
        let empty = Geometry::Polygon(geo::Polygon::new(LineString::new(vec![]), vec![]));
        for g in [point_ring, collinear, empty] {
            assert_eq!(sum(&g), (0.0, 0));
        }
    }

    #[test]
    fn a_polygon_hanging_off_the_north_west_edge_matches_its_clipped_twin() {
        assert_eq!(sum(&boxed(2.0, 6.4, 3.25, 8.5)), (3_017.0, 11));
        assert_eq!(sum(&boxed(3.0, 6.4, 3.25, 7.0)), (3_017.0, 11));
    }

    #[test]
    fn the_real_cog_agrees_with_the_in_memory_grid() {
        use crate::raster::geotiff::{FIXTURE, GeoTiff};
        let mut g = GeoTiff::open(FIXTURE.to_vec(), "fixture").unwrap();
        let z = zonal_sum(&mut g, &boxed(3.0, 4.0, 5.0, 7.0)).unwrap();
        assert_eq!((z.sum, z.pixels), (875_793.0, 598));
        let z = zonal_sum(&mut g, &boxed(4.5, 5.2, 5.9, 6.7)).unwrap();
        assert_eq!((z.sum, z.pixels), (214_725.0, 210));
        let t = raster_total(&mut g).unwrap();
        assert_eq!((t.sum, t.pixels), (1_761_153.0, 1197));
    }
}
