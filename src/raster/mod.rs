//! Rasters fhir-pipes can read without GDAL: a GeoTIFF decoded by the pure-Rust
//! `tiff` crate, plus the georeferencing that maps a pixel to a WGS84
//! coordinate. Only north-up geographic (lon/lat) rasters — WorldPop's
//! layout — are supported; anything else is a usage error, never a silent
//! misplacement.

pub mod fetch;
pub mod geotiff;
pub mod zonal;

use crate::error::{PipesError, Result};

/// Affine georeferencing for a north-up raster. Pixel (col, row) covers
/// lon [x0 + col·px, x0 + (col+1)·px) and lat (y0 − (row+1)·py, y0 − row·py].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoTransform {
    pub x0: f64,
    pub y0: f64,
    /// Pixel width in degrees, > 0.
    pub px: f64,
    /// Pixel height in degrees, > 0 (rows run north to south).
    pub py: f64,
}

impl GeoTransform {
    /// Rejects what `col_of`/`row_of` cannot represent: non-finite values, or a
    /// zero/negative pixel size (a negative `py` would silently flip north and south).
    pub fn new(x0: f64, y0: f64, px: f64, py: f64) -> Result<GeoTransform> {
        if ![x0, y0, px, py].iter().all(|v| v.is_finite()) {
            return Err(PipesError::Usage(format!(
                "geotransform has a non-finite value: origin ({x0}, {y0}), pixel ({px}, {py})"
            )));
        }
        if !(px > 0.0 && py > 0.0) {
            return Err(PipesError::Usage(format!(
                "pixel size must be positive, got ({px}, {py})"
            )));
        }
        Ok(GeoTransform { x0, y0, px, py })
    }

    /// Fractional column of a longitude: 0.0 is the left edge of column 0.
    pub fn col_of(&self, lon: f64) -> f64 {
        (lon - self.x0) / self.px
    }
    /// Fractional row of a latitude: 0.0 is the top edge of row 0.
    pub fn row_of(&self, lat: f64) -> f64 {
        (self.y0 - lat) / self.py
    }
    pub fn lat_center(&self, row: u32) -> f64 {
        self.y0 - (f64::from(row) + 0.5) * self.py
    }
}

/// One decoded chunk. Edge chunks are smaller than `Raster::tile_size`;
/// `data` is row-major with `width` values per row and no padding.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

/// A single-band float raster read one tile at a time.
pub trait Raster {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn transform(&self) -> GeoTransform;
    fn nodata(&self) -> Option<f32>;
    /// Nominal (tile_width, tile_height). For a stripped TIFF this is
    /// (width, rows_per_strip).
    fn tile_size(&self) -> (u32, u32);
    /// Decode the chunk at tile column `tx`, tile row `ty`. `tx` must be
    /// `< tiles_across()` and `ty < tiles_down()`; implementations return
    /// `PipesError::Usage` otherwise, never panic.
    fn read_tile(&mut self, tx: u32, ty: u32) -> Result<Tile>;

    fn tiles_across(&self) -> u32 {
        self.width().div_ceil(self.tile_size().0.max(1))
    }
    fn tiles_down(&self) -> u32 {
        self.height().div_ceil(self.tile_size().1.max(1))
    }
}

/// True for a value that carries data: finite and not the nodata marker.
/// Exact `f32` equality on purpose: callers must parse the file's nodata to
/// `f32` so it matches the stored samples; a NaN nodata is covered by the
/// finiteness check.
pub fn is_data(v: f32, nodata: Option<f32>) -> bool {
    v.is_finite() && nodata.is_none_or(|nd| v != nd)
}

/// In-memory raster for tests: the whole grid in one Vec, tiled on demand.
#[cfg(test)]
pub struct MemRaster {
    pub width: u32,
    pub height: u32,
    pub transform: GeoTransform,
    pub nodata: Option<f32>,
    pub tile: (u32, u32),
    pub data: Vec<f32>,
}

#[cfg(test)]
impl MemRaster {
    /// The same grid as `tests/fixtures/raster/pop_small_cog.tif`: 40×30,
    /// 0.1° pixels from (3.0, 7.0), value r*100 + c + 1, nodata at
    /// (0,0), (5,5), (29,39), 16×16 tiles.
    pub fn fixture() -> MemRaster {
        let (w, h) = (40u32, 30u32);
        let mut data = Vec::with_capacity((w * h) as usize);
        for r in 0..h {
            for c in 0..w {
                let v = if matches!((r, c), (0, 0) | (5, 5) | (29, 39)) {
                    -99999.0
                } else {
                    (r * 100 + c + 1) as f32
                };
                data.push(v);
            }
        }
        MemRaster {
            width: w,
            height: h,
            transform: GeoTransform {
                x0: 3.0,
                y0: 7.0,
                px: 0.1,
                py: 0.1,
            },
            nodata: Some(-99999.0),
            tile: (16, 16),
            data,
        }
    }
}

#[cfg(test)]
impl Raster for MemRaster {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn transform(&self) -> GeoTransform {
        self.transform
    }
    fn nodata(&self) -> Option<f32> {
        self.nodata
    }
    fn tile_size(&self) -> (u32, u32) {
        self.tile
    }
    fn read_tile(&mut self, tx: u32, ty: u32) -> Result<Tile> {
        if tx >= self.tiles_across() || ty >= self.tiles_down() {
            return Err(PipesError::Usage(format!(
                "tile ({tx}, {ty}) is outside the grid"
            )));
        }
        let (tw, th) = self.tile;
        let (c0, r0) = (tx * tw, ty * th);
        let w = tw.min(self.width - c0);
        let h = th.min(self.height - r0);
        let mut data = Vec::with_capacity((w * h) as usize);
        for r in r0..r0 + h {
            for c in c0..c0 + w {
                data.push(self.data[(r * self.width + c) as usize]);
            }
        }
        Ok(Tile {
            width: w,
            height: h,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: GeoTransform = GeoTransform {
        x0: 3.0,
        y0: 7.0,
        px: 0.1,
        py: 0.1,
    };

    #[test]
    fn pixel_and_geo_coordinates_round_trip() {
        assert_eq!(T.col_of(3.0), 0.0);
        assert_eq!(T.row_of(7.0), 0.0);
        assert!((T.col_of(3.25) - 2.5).abs() < 1e-9);
        assert!((T.row_of(6.75) - 2.5).abs() < 1e-9);
        assert!((T.col_of(3.05) - 0.5).abs() < 1e-9);
        assert!((T.lat_center(0) - 6.95).abs() < 1e-9);
        assert!((T.col_of(6.95) - 39.5).abs() < 1e-9);
        assert!((T.lat_center(29) - 4.05).abs() < 1e-9);
    }

    #[test]
    fn mem_raster_tiles_match_the_fixture_layout() {
        let mut r = MemRaster::fixture();
        assert_eq!((r.tiles_across(), r.tiles_down()), (3, 2));
        let t = r.read_tile(0, 0).unwrap();
        assert_eq!((t.width, t.height), (16, 16));
        assert_eq!(t.data[0], -99999.0);
        assert_eq!(t.data[1], 2.0);
        assert_eq!(t.data[16], 101.0);
        let t = r.read_tile(2, 1).unwrap();
        assert_eq!((t.width, t.height), (8, 14));
        assert_eq!(t.data[0], 1633.0, "row 16, col 32");
        assert_eq!(*t.data.last().unwrap(), -99999.0);
        assert!(!is_data(-99999.0, r.nodata()));
        assert!(!is_data(f32::NAN, r.nodata()));
        assert!(is_data(0.0, r.nodata()));
        assert!(r.read_tile(3, 0).is_err());
        assert!(r.read_tile(0, 2).is_err());
    }

    #[test]
    fn new_rejects_non_finite_and_non_positive_pixel_sizes() {
        assert_eq!(
            GeoTransform::new(f64::NAN, 7.0, 0.1, 0.1)
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            GeoTransform::new(3.0, 7.0, 0.0, 0.1)
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            GeoTransform::new(3.0, 7.0, 0.1, -0.1)
                .unwrap_err()
                .exit_code(),
            2
        );
        assert!(GeoTransform::new(3.0, 7.0, 0.1, 0.1).is_ok());
    }

    #[test]
    fn pixel_edges_are_left_and_top_closed() {
        // Tolerance, not exact equality: (3.1 - 3.0) / 0.1 lands a few ULPs
        // off 1.0 under IEEE-754 arithmetic.
        assert!(
            (T.col_of(3.1) - 1.0).abs() < 1e-9,
            "lon 3.1 is the left edge of column 1"
        );
        assert!(
            (T.row_of(6.9) - 1.0).abs() < 1e-9,
            "lat 6.9 is the top edge of row 1"
        );
    }

    #[test]
    fn a_pixel_centre_maps_back_to_its_own_pixel() {
        for (c, r) in [(0u32, 0u32), (39, 0), (0, 29), (39, 29), (17, 11)] {
            let lon = T.x0 + (f64::from(c) + 0.5) * T.px;
            assert_eq!(T.col_of(lon).floor() as u32, c);
            assert_eq!(T.row_of(T.lat_center(r)).floor() as u32, r);
        }
    }

    #[test]
    fn southern_hemisphere_rows_still_run_north_to_south() {
        let s = GeoTransform::new(30.0, -1.0, 0.5, 0.5).unwrap();
        assert!((s.lat_center(0) - (-1.25)).abs() < 1e-12);
        assert!((s.lat_center(3) - (-2.75)).abs() < 1e-12);
        assert_eq!(s.row_of(-2.0), 2.0);
    }

    #[test]
    fn without_a_nodata_marker_every_finite_value_is_data() {
        assert!(is_data(-99999.0, None));
        assert!(is_data(0.0, None));
        assert!(!is_data(f32::INFINITY, None));
        assert!(!is_data(f32::NAN, Some(f32::NAN)));
    }
}
