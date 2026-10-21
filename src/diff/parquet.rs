//! GeoParquet input, one record batch at a time. Cells become JSON so the
//! column coercion is the one GeoJSON uses; the geometry column is found
//! through the `geo` file metadata and read as WKB.

use std::fs::File;
use std::path::Path;

use arrow::array::cast::AsArray;
use arrow::array::types::{Float32Type, Float64Type, Int32Type, Int64Type};
use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::datatypes::{DataType, Schema};
use geo_traits::to_geo::ToGeoGeometry;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::KeyValue;
use serde_json::Value;
use wkb::reader::read_wkb;

use crate::diff::input::{ID_COLUMN, InputRow, column_from_json, is_writable, other_crs};
use crate::error::{PipesError, Result};
use crate::report::Report;
use crate::write::schema::GEOMETRY_COLUMN;

const BATCH_ROWS: usize = 1024;

/// The sink gets the report back so it can add its own issues while the
/// reader holds the same one.
pub fn read_geoparquet<F>(path: &Path, report: &mut Report, mut sink: F) -> Result<()>
where
    F: FnMut(InputRow, &mut Report) -> Result<()>,
{
    let file = File::open(path).map_err(|e| PipesError::io(path, e))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| PipesError::parquet_at(path, e))?;
    let kv = builder.metadata().file_metadata().key_value_metadata();
    let geometry_column = geometry_column_name(kv, builder.schema());
    if let Some(column) = &geometry_column
        && let Some(crs) = geo_metadata(kv)
            .and_then(|g| g["columns"][column.as_str()].get("crs").cloned())
            .as_ref()
            .and_then(other_crs)
    {
        return Err(PipesError::Usage(format!(
            "{}: the geometry is in {crs}, not WGS 84 longitude/latitude (OGC:CRS84 or \
             EPSG:4326); reproject it first, for example with `ogr2ogr -t_srs EPSG:4326`",
            path.display()
        )));
    }
    let reader = builder
        .with_batch_size(BATCH_ROWS)
        .build()
        .map_err(|e| PipesError::parquet_at(path, e))?;
    let mut index = 0usize;
    for batch in reader {
        let batch = batch?;
        for r in 0..batch.num_rows() {
            index += 1;
            let row = row_from_batch(&batch, r, index, geometry_column.as_deref(), report);
            sink(row, report)?;
        }
    }
    Ok(())
}

/// The GeoParquet `geo` metadata names the primary geometry column; a file
/// without it may still have a column called `geometry`. `None` means an
/// attribute-only edit.
pub fn geometry_column_name(kv: Option<&Vec<KeyValue>>, schema: &Schema) -> Option<String> {
    let from_meta =
        geo_metadata(kv).and_then(|g| g.get("primary_column")?.as_str().map(str::to_string));
    let name = from_meta.unwrap_or_else(|| GEOMETRY_COLUMN.to_string());
    schema.column_with_name(&name).map(|_| name)
}

/// The GeoParquet `geo` file metadata, parsed.
fn geo_metadata(kv: Option<&Vec<KeyValue>>) -> Option<Value> {
    kv.and_then(|kv| kv.iter().find(|k| k.key == "geo"))
        .and_then(|k| k.value.as_deref())
        .and_then(|v| serde_json::from_str::<Value>(v).ok())
}

fn row_from_batch(
    batch: &RecordBatch,
    r: usize,
    index: usize,
    geometry_column: Option<&str>,
    report: &mut Report,
) -> InputRow {
    let mut row = InputRow {
        line: index,
        ..Default::default()
    };
    let schema = batch.schema();
    if let Some((i, _)) = schema.column_with_name(ID_COLUMN) {
        row.id = match cell_to_json(batch.column(i), r) {
            Some(Value::String(s)) if !s.is_empty() => Some(s),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
    }
    let label = row.id.clone().unwrap_or_else(|| format!("<row {index}>"));
    for (i, field) in schema.fields().iter().enumerate() {
        let name = field.name();
        let col = batch.column(i);
        if Some(name.as_str()) == geometry_column {
            if col.is_null(r) {
                continue;
            }
            match wkb_cell(col, r) {
                Some(bytes) => match read_wkb(bytes).ok().and_then(|w| w.try_to_geometry()) {
                    Some(g) => row.geometry = Some(g),
                    None => report.add(
                        "geometry_unparseable",
                        &label,
                        "geometry column is not valid WKB",
                    ),
                },
                None => report.add(
                    "geometry_unparseable",
                    &label,
                    &format!(
                        "geometry column has type {}, not binary WKB",
                        col.data_type()
                    ),
                ),
            }
            continue;
        }
        if !is_writable(name) {
            continue;
        }
        let Some(json) = cell_to_json(col, r) else {
            report.add(
                "input_column_type",
                &label,
                &format!("{name}: unsupported column type {}", col.data_type()),
            );
            row.unread.push(name.clone());
            continue;
        };
        match column_from_json(name, &json) {
            Ok(v) => {
                row.columns.insert(name.clone(), v);
            }
            Err(msg) => {
                report.add("input_column_type", &label, &format!("{name}: {msg}"));
                row.unread.push(name.clone());
            }
        }
    }
    row
}

fn wkb_cell(col: &ArrayRef, r: usize) -> Option<&[u8]> {
    match col.data_type() {
        DataType::Binary => Some(col.as_binary::<i32>().value(r)),
        DataType::LargeBinary => Some(col.as_binary::<i64>().value(r)),
        _ => None,
    }
}

/// A float cell as JSON. NaN and the infinities have no JSON number; they
/// become text, which a number column refuses, rather than null, which
/// would clear the field.
fn json_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(v.to_string()))
}

fn list_to_json(items: &ArrayRef) -> Value {
    Value::Array(
        (0..items.len())
            .map(|i| cell_to_json(items, i).unwrap_or(Value::Null))
            .collect(),
    )
}

/// One cell as JSON. `None` means the column's Arrow type is not one diff
/// understands; a null cell is `Some(Value::Null)`.
pub fn cell_to_json(col: &ArrayRef, r: usize) -> Option<Value> {
    if col.is_null(r) {
        return Some(Value::Null);
    }
    Some(match col.data_type() {
        DataType::Utf8 => Value::String(col.as_string::<i32>().value(r).to_string()),
        DataType::LargeUtf8 => Value::String(col.as_string::<i64>().value(r).to_string()),
        DataType::Float64 => json_f64(col.as_primitive::<Float64Type>().value(r)),
        DataType::Float32 => json_f64(col.as_primitive::<Float32Type>().value(r) as f64),
        DataType::Int64 => Value::from(col.as_primitive::<Int64Type>().value(r)),
        DataType::Int32 => Value::from(col.as_primitive::<Int32Type>().value(r)),
        DataType::Boolean => Value::Bool(col.as_boolean().value(r)),
        DataType::List(_) => list_to_json(&col.as_list::<i32>().value(r)),
        DataType::LargeList(_) => list_to_json(&col.as_list::<i64>().value(r)),
        DataType::Struct(_) => {
            let s = col.as_struct();
            let mut o = serde_json::Map::new();
            for (field, child) in s.fields().iter().zip(s.columns()) {
                o.insert(
                    field.name().clone(),
                    cell_to_json(child, r).unwrap_or(Value::Null),
                );
            }
            Value::Object(o)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    use arrow::array::builder::{ListBuilder, StringBuilder};
    use arrow::array::{BinaryArray, Float64Array, RecordBatch, StringArray};
    use arrow::datatypes::{Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    use crate::diff::input::{ColumnValue, Identifier};
    use crate::geometry::to_wkb;

    fn write_fixture(path: &Path) {
        write_fixture_with(
            path,
            r#"{"version":"1.1.0","primary_column":"shape","columns":{"shape":{"encoding":"WKB"}}}"#,
        );
    }

    fn write_fixture_with(path: &Path, geo: &str) {
        let mut alias = ListBuilder::new(StringBuilder::new());
        alias.values().append_value("GC");
        alias.append(true);
        alias.append(false);
        let point = to_wkb(&geo::Geometry::Point(geo::Point::new(3.25, 6.25)));
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
            Field::new(
                "alias",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ),
            Field::new("identifier", DataType::Utf8, true),
            Field::new("position_longitude", DataType::Float64, true),
            Field::new("admin1_name", DataType::Utf8, true),
            Field::new("shape", DataType::Binary, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![Some("clinic"), None])),
                Arc::new(StringArray::from(vec![Some("Gama Clinic"), Some("x")])),
                Arc::new(alias.finish()),
                Arc::new(StringArray::from(vec![
                    Some(r#"[{"system":"s","value":"v"}]"#),
                    None,
                ])),
                Arc::new(Float64Array::from(vec![Some(3.25), None])),
                Arc::new(StringArray::from(vec![Some("Kano"), None])),
                Arc::new(BinaryArray::from(vec![
                    Some(point.as_slice()),
                    Some(b"junk".as_slice()),
                ])),
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo.to_string())]))
            .build();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn a_geometry_in_another_crs_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edits.parquet");
        write_fixture_with(
            &path,
            r#"{"version":"1.1.0","primary_column":"shape","columns":{"shape":{"encoding":"WKB",
                "crs":{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator","id":{"authority":"EPSG","code":3857}}}}}"#,
        );
        let err = read_geoparquet(&path, &mut Report::default(), |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(&err, PipesError::Usage(m) if m.contains("EPSG:3857")),
            "{err}"
        );
        // WGS 84 named either way reads.
        for crs in [
            r#"{"id":{"authority":"OGC","code":"CRS84"}}"#,
            r#"{"id":{"authority":"EPSG","code":4326}}"#,
        ] {
            write_fixture_with(
                &path,
                &format!(
                    r#"{{"version":"1.1.0","primary_column":"shape","columns":{{"shape":{{"encoding":"WKB","crs":{crs}}}}}}}"#
                ),
            );
            read_geoparquet(&path, &mut Report::default(), |_, _| Ok(())).unwrap();
        }
    }

    #[test]
    fn a_nan_cell_is_not_a_cleared_field() {
        assert_eq!(json_f64(f64::NAN), json!("NaN"));
        assert!(column_from_json("position_longitude", &json_f64(f64::NAN)).is_err());
    }

    #[test]
    fn reads_rows_with_columns_and_wkb_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edits.parquet");
        write_fixture(&path);
        let mut report = Report::default();
        let mut rows = Vec::new();
        read_geoparquet(&path, &mut report, |row, _| {
            rows.push(row);
            Ok(())
        })
        .unwrap();
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!(r.id.as_deref(), Some("clinic"));
        assert_eq!(r.line, 1);
        assert_eq!(
            r.columns.get("name"),
            Some(&ColumnValue::Text("Gama Clinic".into()))
        );
        assert_eq!(
            r.columns.get("alias"),
            Some(&ColumnValue::TextList(vec!["GC".into()]))
        );
        assert_eq!(
            r.columns.get("identifier"),
            Some(&ColumnValue::Identifiers(vec![Identifier {
                system: Some("s".into()),
                value: Some("v".into())
            }]))
        );
        assert_eq!(
            r.columns.get("position_longitude"),
            Some(&ColumnValue::Number(3.25))
        );
        assert!(!r.columns.contains_key("admin1_name"));
        assert_eq!(
            r.geometry,
            Some(geo::Geometry::Point(geo::Point::new(3.25, 6.25)))
        );

        let r = &rows[1];
        assert_eq!(r.id, None);
        assert_eq!(r.columns.get("alias"), Some(&ColumnValue::Null));
        assert_eq!(
            r.columns.get("position_longitude"),
            Some(&ColumnValue::Null)
        );
        assert!(r.geometry.is_none());
        assert_eq!(report.count("geometry_unparseable"), 1);
    }

    /// QGIS, GDAL and DuckDB write Snappy by default and our geo outputs are
    /// Zstd; the parquet crate only decodes codecs enabled at build time, so
    /// this guards the feature list in Cargo.toml.
    #[test]
    fn snappy_lz4_zstd_and_gzip_compressed_input_is_readable() {
        use parquet::basic::Compression;
        let dir = tempfile::tempdir().unwrap();
        for (name, compression) in [
            ("snappy", Compression::SNAPPY),
            ("lz4", Compression::LZ4_RAW),
            ("zstd", Compression::ZSTD(Default::default())),
            ("gzip", Compression::GZIP(Default::default())),
        ] {
            let path = dir.path().join(format!("{name}.parquet"));
            let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec![Some("a")]))],
            )
            .unwrap();
            let props = WriterProperties::builder()
                .set_compression(compression)
                .build();
            let mut w =
                ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, Some(props))
                    .unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            let mut ids = Vec::new();
            read_geoparquet(&path, &mut Report::default(), |row, _| {
                ids.push(row.id.unwrap());
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(ids, vec!["a"]);
        }
    }

    #[test]
    fn a_file_without_geo_metadata_falls_back_to_a_geometry_column_or_none() {
        let schema = Schema::new(vec![Field::new("geometry", DataType::Binary, true)]);
        assert_eq!(
            geometry_column_name(None, &schema).as_deref(),
            Some("geometry")
        );
        let schema = Schema::new(vec![Field::new("name", DataType::Utf8, true)]);
        assert_eq!(geometry_column_name(None, &schema), None);
        let kv = vec![KeyValue::new(
            "geo".into(),
            r#"{"primary_column":"shape"}"#.to_string(),
        )];
        let schema = Schema::new(vec![Field::new("shape", DataType::Binary, true)]);
        assert_eq!(
            geometry_column_name(Some(&kv), &schema).as_deref(),
            Some("shape")
        );
    }
}
