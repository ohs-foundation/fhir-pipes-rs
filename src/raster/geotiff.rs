//! GeoTIFF over an in-memory byte buffer via the pure-Rust `tiff` crate:
//! one band, 32-bit float samples, tiled or stripped, any compression the
//! crate decodes (WorldPop COGs are DEFLATE). The whole file is held in
//! memory: a 100 m country raster is 100–200 MB compressed, which the
//! deployment target has to spare and is far cheaper than one HTTP range
//! request per tile. Only the first IFD (full resolution) is read; COG
//! overviews are ignored.

use std::io::Cursor;

use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;

use super::{GeoTransform, Raster, Tile};
use crate::error::{PipesError, Result};

/// GeoKey 1024 (GTModelType): 2 = geographic lat/lon.
const GT_MODEL_TYPE_KEY: u16 = 1024;
const MODEL_TYPE_GEOGRAPHIC: u16 = 2;
/// GeoKey 1025 (GTRasterType): 1 = PixelIsArea (what `GeoTransform` models),
/// 2 = PixelIsPoint (coordinates name pixel centres; would need a half-pixel shift).
const GT_RASTER_TYPE_KEY: u16 = 1025;
const RASTER_TYPE_PIXEL_IS_POINT: u16 = 2;

#[derive(Debug)]
pub struct GeoTiff {
    decoder: Decoder<Cursor<Vec<u8>>>,
    width: u32,
    height: u32,
    transform: GeoTransform,
    nodata: Option<f32>,
    tile: (u32, u32),
    label: String,
}

fn tiff_err(label: &str, e: tiff::TiffError) -> PipesError {
    PipesError::Usage(format!("{label}: {e}"))
}

impl GeoTiff {
    /// Parse the header, georeferencing and nodata, and decode the first
    /// tile to confirm the sample type, so a wrong file fails here and not
    /// half way through a run. `label` names the file in errors.
    pub fn open(bytes: Vec<u8>, label: &str) -> Result<GeoTiff> {
        let mut decoder = Decoder::new(Cursor::new(bytes)).map_err(|e| tiff_err(label, e))?;
        let (width, height) = decoder.dimensions().map_err(|e| tiff_err(label, e))?;
        let samples = decoder
            .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)
            .map_err(|e| tiff_err(label, e))?
            .unwrap_or(1);
        if samples != 1 {
            return Err(PipesError::Usage(format!(
                "{label}: {samples}-band raster; fhir-pipes reads single-band population grids (extract one band first)"
            )));
        }
        let transform = geotransform(&mut decoder, label)?;
        check_geokeys(&mut decoder, label)?;
        let nodata = match decoder
            .find_tag(Tag::GdalNodata)
            .map_err(|e| tiff_err(label, e))?
        {
            Some(v) => {
                let s = v.into_string().map_err(|e| tiff_err(label, e))?;
                let s = s
                    .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                    .to_string();
                Some(s.parse::<f32>().map_err(|_| {
                    PipesError::Usage(format!("{label}: GDAL_NODATA {s:?} is not a number"))
                })?)
            }
            None => None,
        };
        let tile = decoder.chunk_dimensions();
        // Load-bearing, not cosmetic: the tiff crate does not reject height
        // == 0, and its strip arithmetic would divide by zero.
        if tile.0 == 0 || tile.1 == 0 || width == 0 || height == 0 {
            return Err(PipesError::Usage(format!(
                "{label}: empty raster or tile ({width}×{height}, tiles {}×{})",
                tile.0, tile.1
            )));
        }
        let mut g = GeoTiff {
            decoder,
            width,
            height,
            transform,
            nodata,
            tile,
            label: label.to_string(),
        };
        g.read_tile(0, 0)?;
        Ok(g)
    }
}

/// ModelPixelScale + ModelTiepoint → GeoTransform. A tiepoint elsewhere
/// than pixel (0,0) is folded in. ModelTransformation (rotated / sheared
/// rasters) is not supported: WorldPop grids are north-up.
fn geotransform(decoder: &mut Decoder<Cursor<Vec<u8>>>, label: &str) -> Result<GeoTransform> {
    let f64s = |decoder: &mut Decoder<Cursor<Vec<u8>>>, tag: Tag| -> Result<Option<Vec<f64>>> {
        decoder
            .find_tag(tag)
            .map_err(|e| tiff_err(label, e))?
            .map(|v| v.into_f64_vec().map_err(|e| tiff_err(label, e)))
            .transpose()
    };
    let scale = f64s(decoder, Tag::ModelPixelScaleTag)?;
    let tie = f64s(decoder, Tag::ModelTiepointTag)?;
    let (Some(scale), Some(tie)) = (scale, tie) else {
        return Err(PipesError::Usage(format!(
            "{label}: no ModelPixelScale/ModelTiepoint tags; only north-up GeoTIFFs are supported"
        )));
    };
    if scale.len() < 2 {
        return Err(PipesError::Usage(format!(
            "{label}: malformed ModelPixelScale ({} values)",
            scale.len()
        )));
    }
    if tie.len() != 6 {
        return Err(PipesError::Usage(format!(
            "{label}: ModelTiepoint must have exactly 6 values (one tiepoint) when ModelPixelScale is present, got {}",
            tie.len()
        )));
    }
    let (px, py) = (scale[0], scale[1]);
    GeoTransform::new(tie[3] - tie[0] * px, tie[4] + tie[1] * py, px, py)
        .map_err(|e| PipesError::Usage(format!("{label}: {e}")))
}

/// Refuse what `GeoTransform` cannot express: a projected raster (its pixel
/// coordinates would be metres while the boundaries are lon/lat) and a
/// PixelIsPoint raster (coordinates name pixel centres, not corners). A file
/// without a GeoKeyDirectory is allowed through — nothing to check.
fn check_geokeys(decoder: &mut Decoder<Cursor<Vec<u8>>>, label: &str) -> Result<()> {
    let Some(keys) = decoder
        .find_tag_unsigned_vec::<u16>(Tag::GeoKeyDirectoryTag)
        .map_err(|e| tiff_err(label, e))?
    else {
        return Ok(());
    };
    // Header is 4 shorts, then one (key, location, count, value) per entry;
    // location 0 means the value is inline.
    for entry in keys.as_chunks::<4>().0.iter().skip(1) {
        let (key, location, value) = (entry[0], entry[1], entry[3]);
        if location != 0 {
            continue;
        }
        if key == GT_MODEL_TYPE_KEY && value != MODEL_TYPE_GEOGRAPHIC {
            return Err(PipesError::Usage(format!(
                "{label}: not a geographic (lon/lat) raster (GTModelType = {value}); reproject to EPSG:4326 first"
            )));
        }
        if key == GT_RASTER_TYPE_KEY && value == RASTER_TYPE_PIXEL_IS_POINT {
            return Err(PipesError::Usage(format!(
                "{label}: PixelIsPoint rasters are not supported (coordinates name pixel centres); rewrite as PixelIsArea"
            )));
        }
    }
    Ok(())
}

impl Raster for GeoTiff {
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
        let across = self.tiles_across();
        let down = self.tiles_down();
        if tx >= across || ty >= down {
            return Err(PipesError::Usage(format!(
                "{}: tile ({tx}, {ty}) is outside the {across}×{down} grid",
                self.label
            )));
        }
        let index = match self.decoder.get_chunk_type() {
            ChunkType::Tile => ty * across + tx,
            ChunkType::Strip => ty,
        };
        let (width, height) = self.decoder.chunk_data_dimensions(index);
        match self
            .decoder
            .read_chunk(index)
            .map_err(|e| tiff_err(&self.label, e))?
        {
            DecodingResult::F32(data) => {
                if data.len() != (width as usize) * (height as usize) {
                    return Err(PipesError::Environment(format!(
                        "{}: tile {index} decoded to {} values, expected {width}×{height}",
                        self.label,
                        data.len()
                    )));
                }
                Ok(Tile {
                    width,
                    height,
                    data,
                })
            }
            other => Err(PipesError::Usage(format!(
                "{}: expected 32-bit float samples, got {}",
                self.label,
                sample_name(&other)
            ))),
        }
    }
}

fn sample_name(r: &DecodingResult) -> &'static str {
    match r {
        DecodingResult::U8(_) => "8-bit unsigned",
        DecodingResult::U16(_) => "16-bit unsigned",
        DecodingResult::U32(_) => "32-bit unsigned",
        DecodingResult::U64(_) => "64-bit unsigned",
        DecodingResult::I8(_) => "8-bit signed",
        DecodingResult::I16(_) => "16-bit signed",
        DecodingResult::I32(_) => "32-bit signed",
        DecodingResult::I64(_) => "64-bit signed",
        DecodingResult::F16(_) => "16-bit float",
        DecodingResult::F32(_) => "32-bit float",
        DecodingResult::F64(_) => "64-bit float",
    }
}

#[cfg(test)]
pub const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/raster/pop_small_cog.tif");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{is_data, MemRaster};

    fn fixture() -> GeoTiff {
        GeoTiff::open(FIXTURE.to_vec(), "pop_small_cog.tif").unwrap()
    }

    #[test]
    fn header_georeferencing_and_nodata_are_read() {
        let g = fixture();
        assert_eq!((g.width(), g.height()), (40, 30));
        assert_eq!(g.tile_size(), (16, 16));
        assert_eq!((g.tiles_across(), g.tiles_down()), (3, 2));
        assert_eq!(g.nodata(), Some(-99999.0));
        let t = g.transform();
        assert!((t.x0 - 3.0).abs() < 1e-12 && (t.y0 - 7.0).abs() < 1e-12);
        assert!((t.px - 0.1).abs() < 1e-12 && (t.py - 0.1).abs() < 1e-12);
    }

    #[test]
    fn tiles_decode_with_edge_tiles_trimmed() {
        let mut g = fixture();
        let t = g.read_tile(0, 0).unwrap();
        assert_eq!((t.width, t.height), (16, 16));
        assert_eq!(t.data[0], -99999.0, "nodata at (0,0)");
        assert_eq!(t.data[1], 2.0, "row 0 col 1");
        assert_eq!(t.data[16], 101.0, "row 1 col 0");
        let t = g.read_tile(2, 1).unwrap();
        assert_eq!((t.width, t.height), (8, 14));
        assert_eq!(t.data[0], 1633.0, "row 16 col 32");
        assert_eq!(*t.data.last().unwrap(), -99999.0, "nodata at (29,39)");
        let err = g.read_tile(3, 0).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(g.read_tile(0, 2).is_err());
    }

    #[test]
    fn every_tile_matches_the_in_memory_raster_and_sums_to_the_known_total() {
        let mut g = fixture();
        let mut m = MemRaster::fixture();
        let (mut sum, mut n) = (0.0f64, 0u64);
        for ty in 0..g.tiles_down() {
            for tx in 0..g.tiles_across() {
                let t = g.read_tile(tx, ty).unwrap();
                assert_eq!(t, m.read_tile(tx, ty).unwrap(), "tile ({tx}, {ty})");
                for &v in &t.data {
                    if is_data(v, g.nodata()) {
                        sum += f64::from(v);
                        n += 1;
                    }
                }
            }
        }
        assert_eq!((sum, n), (1_761_153.0, 1197));
    }

    #[test]
    fn not_a_tiff_is_a_usage_error() {
        let err = GeoTiff::open(b"not a tiff".to_vec(), "x.tif").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().starts_with("x.tif:"), "{err}");
    }

    /// A stripped Float32 GeoTIFF of the MemRaster fixture grid, written by the
    /// tiff encoder: `rows_per_strip` rows per chunk, the given tiepoint
    /// (i, j, x, y) and GeoKey directory shorts.
    fn encoded(rows_per_strip: u32, tiepoint: [f64; 4], geokeys: &[u16]) -> Vec<u8> {
        use tiff::encoder::{colortype::Gray32Float, TiffEncoder};

        let m = MemRaster::fixture();
        let mut buf = Cursor::new(Vec::new());
        {
            let mut tiff = TiffEncoder::new(&mut buf).unwrap();
            let mut image = tiff.new_image::<Gray32Float>(m.width, m.height).unwrap();
            image.rows_per_strip(rows_per_strip).unwrap();
            {
                let enc = image.encoder();
                let [i, j, x, y] = tiepoint;
                enc.write_tag(Tag::ModelPixelScaleTag, &[0.1f64, 0.1, 0.0][..])
                    .unwrap();
                enc.write_tag(Tag::ModelTiepointTag, &[i, j, 0.0, x, y, 0.0][..])
                    .unwrap();
                enc.write_tag(Tag::GdalNodata, "-99999").unwrap();
                if !geokeys.is_empty() {
                    enc.write_tag(Tag::GeoKeyDirectoryTag, geokeys).unwrap();
                }
            }
            image.write_data(&m.data).unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn stripped_tiffs_read_through_the_strip_branch() {
        let bytes = encoded(8, [0.0, 0.0, 3.0, 7.0], &[]);
        let mut g = GeoTiff::open(bytes, "stripped.tif").unwrap();
        assert_eq!(g.tile_size(), (40, 8));
        assert_eq!((g.tiles_across(), g.tiles_down()), (1, 4));

        let mut m = MemRaster::fixture();
        m.tile = g.tile_size();
        let (mut sum, mut n) = (0.0f64, 0u64);
        for ty in 0..g.tiles_down() {
            for tx in 0..g.tiles_across() {
                let t = g.read_tile(tx, ty).unwrap();
                assert_eq!(t, m.read_tile(tx, ty).unwrap(), "tile ({tx}, {ty})");
                for &v in &t.data {
                    if is_data(v, g.nodata()) {
                        sum += f64::from(v);
                        n += 1;
                    }
                }
            }
        }
        assert_eq!((sum, n), (1_761_153.0, 1197));

        let err = g.read_tile(0, 4).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn a_non_zero_tiepoint_is_folded_into_the_origin() {
        let bytes = encoded(30, [2.0, 3.0, 3.2, 6.7], &[]);
        let g = GeoTiff::open(bytes, "tiepoint.tif").unwrap();
        let t = g.transform();
        assert!((t.x0 - 3.0).abs() < 1e-12, "x0 = {}", t.x0);
        assert!((t.y0 - 7.0).abs() < 1e-12, "y0 = {}", t.y0);
    }

    #[test]
    fn projected_and_pixel_is_point_rasters_are_refused() {
        let bytes = encoded(30, [0.0, 0.0, 3.0, 7.0], &[1, 1, 0, 1, 1024, 0, 1, 1]);
        let err = GeoTiff::open(bytes, "x.tif").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("not a geographic"), "{err}");

        let bytes = encoded(30, [0.0, 0.0, 3.0, 7.0], &[1, 1, 0, 1, 1025, 0, 1, 2]);
        let err = GeoTiff::open(bytes, "x.tif").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("PixelIsPoint"), "{err}");

        let bytes = encoded(
            30,
            [0.0, 0.0, 3.0, 7.0],
            &[1, 1, 0, 2, 1024, 0, 1, 2, 1025, 0, 1, 1],
        );
        assert!(GeoTiff::open(bytes, "x.tif").is_ok());
    }

    #[test]
    fn multi_band_rasters_are_refused() {
        use tiff::encoder::{colortype::RGB32Float, TiffEncoder};

        let (w, h) = (4u32, 4u32);
        let mut buf = Cursor::new(Vec::new());
        {
            let mut tiff = TiffEncoder::new(&mut buf).unwrap();
            let image = tiff.new_image::<RGB32Float>(w, h).unwrap();
            let data = vec![0.0f32; (w * h * 3) as usize];
            image.write_data(&data).unwrap();
        }
        let err = GeoTiff::open(buf.into_inner(), "rgb.tif").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("3-band"), "{err}");
    }
}
