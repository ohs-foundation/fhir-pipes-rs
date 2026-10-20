//! GeoJSON input: a FeatureCollection streamed one Feature at a time by a
//! serde visitor, or one Feature per line. A whole-country export never
//! sits in memory at once.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use geojson::Feature;
use geojson::feature::Id;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::diff::input::{ID_COLUMN, InputRow, column_from_json, is_writable, other_crs};
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::NdjsonReader;
use crate::geometry::convert;
use crate::report::Report;

/// One Feature -> one row. `line` is the 1-based feature index.
pub fn feature_to_row(f: Feature, line: usize, report: &mut Report) -> InputRow {
    let mut row = InputRow {
        line,
        ..Default::default()
    };
    let props = f.properties.unwrap_or_default();
    row.id = match props.get(ID_COLUMN) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => match f.id {
            Some(Id::String(s)) if !s.is_empty() => Some(s),
            Some(Id::Number(n)) => Some(n.to_string()),
            _ => None,
        },
    };
    let label = row.id.clone().unwrap_or_else(|| format!("<row {line}>"));
    for (name, value) in &props {
        if !is_writable(name) {
            continue;
        }
        match column_from_json(name, value) {
            Ok(v) => {
                row.columns.insert(name.clone(), v);
            }
            Err(msg) => {
                report.add("input_column_type", &label, &format!("{name}: {msg}"));
                row.unread.push(name.clone());
            }
        }
    }
    if let Some(g) = f.geometry {
        match convert(g) {
            Ok((geom, z_dropped)) => {
                if z_dropped {
                    report.add(
                        "boundary_z_dropped",
                        &label,
                        "3D coordinates flattened to 2D",
                    );
                }
                row.geometry = Some(geom);
            }
            Err(msg) => report.add("geometry_unparseable", &label, &msg),
        }
    }
    row
}

/// Stream `{"type":"FeatureCollection","features":[...]}` from disk,
/// handing each Feature to `sink` as soon as it is parsed. Keys other
/// than `features` are skipped without being materialised.
pub fn read_feature_collection<F>(path: &Path, mut sink: F) -> Result<()>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    let file = File::open(path).map_err(|e| PipesError::io(path, e))?;
    let mut de = serde_json::Deserializer::from_reader(BufReader::with_capacity(1 << 20, file));
    let mut state = State {
        sink: &mut sink,
        error: None,
        seen_features: false,
    };
    let outcome = Collection(&mut state).deserialize(&mut de);
    if let Some(e) = state.error.take() {
        return Err(e);
    }
    outcome.map_err(|e| PipesError::Usage(format!("{}: {e}", path.display())))?;
    if !state.seen_features {
        return Err(PipesError::Usage(format!(
            "{}: not a GeoJSON FeatureCollection (no \"features\" array)",
            path.display()
        )));
    }
    Ok(())
}

struct State<'s, F> {
    sink: &'s mut F,
    /// The sink's own error, stashed so it survives serde's error type.
    error: Option<PipesError>,
    seen_features: bool,
}

struct Collection<'a, 's, F>(&'a mut State<'s, F>);
struct Features<'a, 's, F>(&'a mut State<'s, F>);

impl<'de, 'a, 's, F> DeserializeSeed<'de> for Collection<'a, 's, F>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de, 'a, 's, F> Visitor<'de> for Collection<'a, 's, F>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a GeoJSON FeatureCollection object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "features" {
                self.0.seen_features = true;
                map.next_value_seed(Features(&mut *self.0))?;
            } else if key == "crs" {
                // GeoJSON is WGS 84 since RFC 7946; an older file can say
                // otherwise, and its coordinates are then not degrees.
                if let Some(crs) = other_crs(&map.next_value::<Value>()?) {
                    return Err(de::Error::custom(format!(
                        "the geometry is in {crs}, not WGS 84 longitude/latitude; \
                         reproject it first, for example with `ogr2ogr -t_srs EPSG:4326`"
                    )));
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

impl<'de, 'a, 's, F> DeserializeSeed<'de> for Features<'a, 's, F>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de, 'a, 's, F> Visitor<'de> for Features<'a, 's, F>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of GeoJSON Features")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        let mut index = 0usize;
        while let Some(feature) = seq.next_element::<Feature>()? {
            index += 1;
            if let Err(e) = (self.0.sink)(feature, index) {
                self.0.error = Some(e);
                return Err(de::Error::custom("stopped by fhir-pipes"));
            }
        }
        Ok(())
    }
}

/// One Feature per line (`.geojsonl`, `.geojsons`).
pub fn read_feature_lines<F>(path: &Path, mut sink: F) -> Result<()>
where
    F: FnMut(Feature, usize) -> Result<()>,
{
    let mut index = 0usize;
    for line in NdjsonReader::open(path)? {
        let line = line?;
        index += 1;
        let feature: Feature = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!(
                "{}: line {}: not a GeoJSON Feature: {e}",
                path.display(),
                line.number
            ))
        })?;
        sink(feature, index)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::diff::input::ColumnValue;

    fn tmp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn feature(json: &str) -> Feature {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn id_comes_from_the_property_then_the_feature_id() {
        let mut r = Report::default();
        let row = feature_to_row(
            feature(r#"{"type":"Feature","id":"f1","properties":{"id":"p1"},"geometry":null}"#),
            1,
            &mut r,
        );
        assert_eq!(row.id.as_deref(), Some("p1"));
        let row = feature_to_row(
            feature(r#"{"type":"Feature","id":"f1","properties":{},"geometry":null}"#),
            2,
            &mut r,
        );
        assert_eq!(row.id.as_deref(), Some("f1"));
        let row = feature_to_row(
            feature(r#"{"type":"Feature","id":7,"properties":{"id":""},"geometry":null}"#),
            3,
            &mut r,
        );
        assert_eq!(row.id.as_deref(), Some("7"));
        let row = feature_to_row(
            feature(r#"{"type":"Feature","properties":null,"geometry":null}"#),
            4,
            &mut r,
        );
        assert_eq!(row.id, None);
        assert_eq!(row.line, 4);
    }

    #[test]
    fn writable_columns_are_kept_derived_ignored_bad_types_reported() {
        let mut r = Report::default();
        let row = feature_to_row(
            feature(
                r#"{"type":"Feature","properties":{"id":"a","name":"N","admin1_name":"stale","alias":["x"],"position_latitude":"oops"},"geometry":{"type":"Point","coordinates":[3.25,6.25,10]}}"#,
            ),
            1,
            &mut r,
        );
        assert_eq!(
            row.columns.get("name"),
            Some(&ColumnValue::Text("N".into()))
        );
        assert_eq!(
            row.columns.get("alias"),
            Some(&ColumnValue::TextList(vec!["x".into()]))
        );
        assert!(!row.columns.contains_key("admin1_name"));
        assert!(!row.columns.contains_key("position_latitude"));
        assert_eq!(r.count("input_column_type"), 1);
        assert_eq!(r.count("boundary_z_dropped"), 1);
        assert_eq!(
            row.geometry,
            Some(geo::Geometry::Point(geo::Point::new(3.25, 6.25)))
        );
    }

    #[test]
    fn a_malformed_geometry_fails_the_read_as_a_usage_error() {
        // The geojson crate rejects a one-number position while parsing the
        // Feature itself, so this never reaches `convert`: the whole read
        // fails, naming the file, rather than one row being reported.
        let f = tmp(r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"id":"a"},"geometry":{"type":"Point","coordinates":[3.25]}}]}"#);
        let err = read_feature_collection(f.path(), |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("position")),
            "{err}"
        );
    }

    #[test]
    fn a_feature_collection_streams_every_feature_in_order() {
        let f = tmp(r#"{"name":"layer","features":[
            {"type":"Feature","properties":{"id":"a"},"geometry":null},
            {"type":"Feature","properties":{"id":"b"},"geometry":null},
            {"type":"Feature","properties":{"id":"c"},"geometry":null}],"type":"FeatureCollection"}"#);
        let mut seen = Vec::new();
        read_feature_collection(f.path(), |feature, index| {
            seen.push((
                index,
                feature.properties.unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (1, "a".to_string()),
                (2, "b".to_string()),
                (3, "c".to_string())
            ]
        );
    }

    #[test]
    fn a_sink_error_stops_the_read_and_comes_back_unchanged() {
        let f = tmp(r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"id":"a"},"geometry":null},
            {"type":"Feature","properties":{"id":"b"},"geometry":null}]}"#);
        let mut calls = 0;
        let err = read_feature_collection(f.path(), |_, _| {
            calls += 1;
            Err(PipesError::Usage("stop".into()))
        })
        .unwrap_err();
        assert!(matches!(&err, PipesError::Usage(m) if m == "stop"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_collection_in_another_crs_is_a_usage_error() {
        let f = tmp(
            r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::3857"}},"features":[]}"#,
        );
        let err = read_feature_collection(f.path(), |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("EPSG::3857")),
            "{err}"
        );
        let f = tmp(
            r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:OGC:1.3:CRS84"}},"features":[]}"#,
        );
        read_feature_collection(f.path(), |_, _| Ok(())).unwrap();
    }

    #[test]
    fn not_a_collection_is_a_usage_error() {
        let f = tmp(r#"{"type":"Feature","properties":{},"geometry":null}"#);
        let err = read_feature_collection(f.path(), |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("FeatureCollection")),
            "{err}"
        );
        let f = tmp(r#"[1,2]"#);
        assert!(matches!(
            read_feature_collection(f.path(), |_, _| Ok(())).unwrap_err(),
            PipesError::Usage(_)
        ));
    }

    #[test]
    fn feature_lines_are_read_one_per_line() {
        let f = tmp(
            "{\"type\":\"Feature\",\"properties\":{\"id\":\"a\"},\"geometry\":null}\n\n{\"type\":\"Feature\",\"properties\":{\"id\":\"b\"},\"geometry\":null}\n",
        );
        let mut seen = Vec::new();
        read_feature_lines(f.path(), |feature, index| {
            seen.push((
                index,
                feature.properties.unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], (2, "b".to_string()));
        let bad = tmp("{\"nope\":1}\n");
        assert!(matches!(
            read_feature_lines(bad.path(), |_, _| Ok(())).unwrap_err(),
            PipesError::Usage(_)
        ));
    }
}
