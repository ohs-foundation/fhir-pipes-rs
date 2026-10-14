//! geo geometry -> little-endian WKB, the encoding Parquet GEOMETRY expects.

use wkb::Endianness;
use wkb::writer::{WriteOptions, write_geometry};

/// Encodes a `geo` geometry to little-endian WKB.
///
/// The `wkb` crate 0.9 mis-encodes `geo::Rect` directly (it omits the ring's
/// point count), so a `Rect` is converted to a `Polygon` before encoding.
pub fn to_wkb(geom: &geo::Geometry<f64>) -> Vec<u8> {
    if let geo::Geometry::Rect(r) = geom {
        return to_wkb(&geo::Geometry::Polygon(r.to_polygon()));
    }
    let mut out = Vec::new();
    let options = WriteOptions {
        endianness: Endianness::LittleEndian,
    };
    write_geometry(&mut out, geom, &options).expect("writing WKB to a Vec cannot fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::polygon;

    #[test]
    fn point_encodes_as_21_little_endian_bytes() {
        let bytes = to_wkb(&geo::Geometry::Point(geo::Point::new(3.25, 6.25)));
        assert_eq!(bytes.len(), 21);
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..5], &1u32.to_le_bytes());
        assert_eq!(f64::from_le_bytes(bytes[5..13].try_into().unwrap()), 3.25);
    }

    #[test]
    fn rect_encodes_as_polygon() {
        let rect = geo::Rect::new((0., 0.), (1., 1.));
        let bytes = to_wkb(&geo::Geometry::Rect(rect));
        assert_eq!(bytes.len(), 93);
        assert_eq!(&bytes[1..5], &3u32.to_le_bytes());
        assert_eq!(&bytes[9..13], &5u32.to_le_bytes());
    }

    #[test]
    fn polygon_encodes_with_type_3() {
        let poly = polygon![(x: 0., y: 0.), (x: 1., y: 0.), (x: 1., y: 1.), (x: 0., y: 0.)];
        let bytes = to_wkb(&geo::Geometry::Polygon(poly));
        assert_eq!(&bytes[1..5], &3u32.to_le_bytes());
    }

    #[test]
    fn multipolygon_with_hole_has_full_layout() {
        let poly_with_hole = polygon![
            exterior: [
                (x: 0., y: 0.), (x: 10., y: 0.), (x: 10., y: 10.), (x: 0., y: 10.), (x: 0., y: 0.),
            ],
            interiors: [
                [
                    (x: 2., y: 2.), (x: 2., y: 8.), (x: 8., y: 8.), (x: 8., y: 2.), (x: 2., y: 2.),
                ],
            ],
        ];
        let simple_poly = polygon![
            (x: 20., y: 20.), (x: 30., y: 20.), (x: 25., y: 30.), (x: 20., y: 20.),
        ];
        let mp = geo::MultiPolygon(vec![poly_with_hole, simple_poly]);
        let bytes = to_wkb(&geo::Geometry::MultiPolygon(mp));
        assert_eq!(bytes.len(), 263);
        assert_eq!(&bytes[1..5], &6u32.to_le_bytes());
        assert_eq!(&bytes[5..9], &2u32.to_le_bytes());
        assert_eq!(&bytes[10..14], &3u32.to_le_bytes());
    }
}
