//! One Parquet file per partition. Native GEOMETRY logical type comes from
//! the `geoarrow.wkb` extension on the geometry field (parquet's `geospatial`
//! feature); GeoParquet 1.1 `geo` metadata and the bbox column are written
//! for readers that predate the logical type. Matches what the spike in
//! docs/superpowers/spikes/2026-09-05-rust-native-geoparquet produced.
//!
//! No `ARROW:schema` blob is embedded. When that blob is present pyarrow
//! rebuilds the schema metadata from it and drops every other footer key --
//! including `geo`, which is appended at close and so can never be inside a
//! blob written at open. Tools that detect GeoParquet through pyarrow
//! (GeoPandas, portolan-cli, ...) would then see plain Parquet. Without the
//! blob they read the footer as written; every Arrow type here is recoverable
//! from the Parquet logical types alone.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::Array;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::ArrowWriterOptions;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::error::{PipesError, Result};
use arrow::datatypes::SchemaRef;

use crate::write::schema::{BBOX_COLUMN, GEOMETRY_COLUMN, RowBatch};

pub struct PartitionStats {
    pub rows: usize,
    pub row_groups: usize,
}

pub struct PartitionWriter {
    writer: ArrowWriter<File>,
    /// A clone of the writer's file descriptor, kept so `finish` can
    /// `sync_all` after `ArrowWriter::close` -- `close` consumes the writer
    /// (and the `File` inside it), so the only way to fsync afterwards is to
    /// have held a separate handle to the same file all along.
    file: File,
    path: PathBuf,
    /// Over the rows with a geometry only; meaningless while `with_geometry` is 0.
    bbox: [f64; 4],
    rows: usize,
    /// Rows whose geometry is not null.
    with_geometry: usize,
    row_groups: usize,
}

impl PartitionWriter {
    #[cfg(test)]
    pub fn create(path: &Path, row_group_size: usize) -> Result<Self> {
        Self::create_with_schema(
            path,
            row_group_size,
            crate::write::schema::output_schema(&crate::profile::Profile::base()),
        )
    }

    /// As [`create`](Self::create), for a batch schema other than the
    /// default (the optional-geometry one). Batches written must match it.
    pub fn create_with_schema(
        path: &Path,
        row_group_size: usize,
        schema: SchemaRef,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PipesError::io(parent, e))?;
        }
        let file = File::create(path).map_err(|e| PipesError::io(path, e))?;
        let file_for_sync = file.try_clone().map_err(|e| PipesError::io(path, e))?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(
                // Write-once, read-many output: worth spending more CPU than
                // the default level for a smaller file on disk.
                ZstdLevel::try_new(3).expect("3 is a valid zstd level"),
            ))
            // The explicit flush() after every write() below is what actually
            // delimits row groups; this is only a safety net in case a batch
            // somehow exceeds the caller's intended row-group size.
            .set_max_row_group_row_count(Some(row_group_size.max(1)))
            .build();
        let options = ArrowWriterOptions::new()
            .with_properties(props)
            .with_skip_arrow_metadata(true);
        let writer = ArrowWriter::try_new_with_options(file, schema, options)
            .map_err(|e| PipesError::parquet_at(path, e))?;
        Ok(Self {
            writer,
            file: file_for_sync,
            path: path.to_path_buf(),
            bbox: [f64::MAX, f64::MAX, f64::MIN, f64::MIN],
            rows: 0,
            with_geometry: 0,
            row_groups: 0,
        })
    }

    /// Flush the batch as one row group. Callers fill a batch to
    /// `row_group_size` and call this; the last partial batch is fine.
    pub fn write(&mut self, batch: &mut RowBatch) -> Result<()> {
        if batch.rows == 0 {
            return Ok(());
        }
        let rb = batch.finish()?;
        let geometry = rb
            .column_by_name(GEOMETRY_COLUMN)
            .expect("schema has geometry");
        let bbox = rb.column_by_name(BBOX_COLUMN).expect("schema has bbox");
        let bbox = bbox
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .expect("bbox is a struct");
        let bbox_cols: Vec<&arrow::array::Float64Array> = ["xmin", "ymin", "xmax", "ymax"]
            .iter()
            .map(|name| {
                bbox.column_by_name(name)
                    .expect("bbox field")
                    .as_any()
                    .downcast_ref::<arrow::array::Float64Array>()
                    .expect("f64")
            })
            .collect();
        for row in 0..rb.num_rows() {
            // A row without geometry (written by --keep-without-geometry)
            // has a null bbox too; it must not widen the file bbox.
            if geometry.is_null(row) {
                continue;
            }
            self.with_geometry += 1;
            for (i, col) in bbox_cols.iter().enumerate() {
                let v = col.value(row);
                if i < 2 {
                    self.bbox[i] = self.bbox[i].min(v);
                } else {
                    self.bbox[i] = self.bbox[i].max(v);
                }
            }
        }
        self.writer
            .write(&rb)
            .map_err(|e| PipesError::parquet_at(self.path.clone(), e))?;
        self.writer
            .flush()
            .map_err(|e| PipesError::parquet_at(self.path.clone(), e))?;
        self.row_groups = self.writer.flushed_row_groups().len();
        self.rows += rb.num_rows();
        Ok(())
    }

    /// Append the `geo` metadata and close the file. The bbox covers the
    /// rows that have a geometry; a file with none leaves it out (it is
    /// optional in GeoParquet).
    pub fn finish(mut self, geometry_types: &[String]) -> Result<PartitionStats> {
        let mut column = serde_json::json!({
            "encoding": "WKB",
            "geometry_types": geometry_types,
        });
        if self.with_geometry > 0 {
            column["bbox"] = serde_json::json!(self.bbox);
        }
        column["covering"] = serde_json::json!({"bbox": {
            "xmin": [BBOX_COLUMN, "xmin"], "ymin": [BBOX_COLUMN, "ymin"],
            "xmax": [BBOX_COLUMN, "xmax"], "ymax": [BBOX_COLUMN, "ymax"]}});
        let geo = serde_json::json!({
            "version": "1.1.0",
            "primary_column": GEOMETRY_COLUMN,
            "columns": {GEOMETRY_COLUMN: column}
        });
        self.writer
            .append_key_value_metadata(KeyValue::new("geo".into(), geo.to_string()));
        self.writer
            .close()
            .map_err(|e| PipesError::parquet_at(self.path.clone(), e))?;
        // Durability: the atomic dataset swap makes this file visible to
        // readers as soon as the directory rename lands, so its data and
        // metadata must already be on disk before that happens.
        self.file
            .sync_all()
            .map_err(|e| PipesError::io(self.path.clone(), e))?;
        Ok(PartitionStats {
            rows: self.rows,
            row_groups: self.row_groups,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::schema::{OutputRow, RowBatch};
    use parquet::basic::LogicalType;
    use parquet::file::reader::{FileReader, SerializedFileReader};

    fn row(id: &str, x: f64, y: f64) -> OutputRow {
        let mut wkb = vec![1u8];
        wkb.extend_from_slice(&1u32.to_le_bytes());
        wkb.extend_from_slice(&x.to_le_bytes());
        wkb.extend_from_slice(&y.to_le_bytes());
        OutputRow {
            id: id.into(),
            tier: "site".into(),
            path: format!("/{id}"),
            country: "NG".into(),
            geom_type: Some("point".into()),
            lon: Some(x),
            lat: Some(y),
            wkb,
            bbox: Some([x, y, x, y]),
            fhir_json: "{}".into(),
            ..Default::default()
        }
    }

    #[test]
    fn writes_native_geometry_type_with_stats_and_geo_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part-0.parquet");
        let mut writer = PartitionWriter::create(&path, 2).unwrap();
        let mut batch = RowBatch::new();
        for (i, (x, y)) in [(3.0, 6.0), (3.5, 6.5), (50.0, 50.0)].iter().enumerate() {
            batch.push(&row(&format!("r{i}"), *x, *y));
            if batch.rows == 2 {
                writer.write(&mut batch).unwrap();
            }
        }
        writer.write(&mut batch).unwrap();
        let stats = writer.finish(&["Point".to_string()]).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.row_groups, 2);

        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let meta = reader.metadata();
        let geom_col = meta
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .find(|c| c.name() == "geometry")
            .unwrap();
        assert!(
            matches!(geom_col.logical_type_ref(), Some(LogicalType::Geometry(_))),
            "{:?}",
            geom_col.logical_type_ref()
        );
        assert_eq!(meta.num_row_groups(), 2);
        let rg0 = meta
            .row_group(0)
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "geometry")
            .unwrap();
        let bbox = rg0.geo_statistics().unwrap().bounding_box().unwrap();
        assert_eq!((bbox.get_xmin(), bbox.get_xmax()), (3.0, 3.5));
        let rg1 = meta
            .row_group(1)
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "geometry")
            .unwrap();
        let bbox1 = rg1.geo_statistics().unwrap().bounding_box().unwrap();
        assert_eq!((bbox1.get_xmin(), bbox1.get_xmax()), (50.0, 50.0));
        let kv = meta.file_metadata().key_value_metadata().unwrap();
        assert!(
            kv.iter().all(|kv| kv.key != "ARROW:schema"),
            "an embedded Arrow schema makes pyarrow drop the geo key: {:?}",
            kv.iter().map(|kv| &kv.key).collect::<Vec<_>>()
        );
        let geo = kv.iter().find(|kv| kv.key == "geo").unwrap();
        let geo: serde_json::Value = serde_json::from_str(geo.value.as_ref().unwrap()).unwrap();
        assert_eq!(geo["primary_column"], "geometry");
        assert_eq!(
            geo["columns"]["geometry"]["bbox"],
            serde_json::json!([3.0, 6.0, 50.0, 50.0])
        );
        assert!(
            geo["columns"]["geometry"].get("crs").is_none(),
            "crs key must be absent, not null"
        );
        assert_eq!(
            geo["columns"]["geometry"]["geometry_types"],
            serde_json::json!(["Point"])
        );
    }

    #[test]
    fn empty_writer_finishes_without_a_bbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.parquet");
        let writer = PartitionWriter::create(&path, 10).unwrap();
        let stats = writer.finish(&[]).unwrap();
        assert_eq!(stats.rows, 0);
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let geo = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key == "geo")
            .unwrap();
        let geo: serde_json::Value = serde_json::from_str(geo.value.as_ref().unwrap()).unwrap();
        assert!(geo["columns"]["geometry"].get("bbox").is_none());
    }

    fn geo_metadata(path: &Path) -> serde_json::Value {
        let reader = SerializedFileReader::new(std::fs::File::open(path).unwrap()).unwrap();
        let kv = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap();
        let geo = kv.iter().find(|kv| kv.key == "geo").unwrap();
        serde_json::from_str(geo.value.as_ref().unwrap()).unwrap()
    }

    fn row_without_geometry(id: &str) -> OutputRow {
        OutputRow {
            id: id.into(),
            tier: "site".into(),
            path: format!("/{id}"),
            country: "NG".into(),
            fhir_json: "{}".into(),
            ..Default::default()
        }
    }

    #[test]
    fn rows_without_geometry_do_not_widen_the_file_bbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.parquet");
        let profile = crate::profile::Profile::base();
        let schema = crate::write::schema::output_schema_optional_geometry(&profile);
        let mut writer = PartitionWriter::create_with_schema(&path, 10, schema.clone()).unwrap();
        let mut batch = RowBatch::for_profile(&profile, true);
        batch.push(&row("a", 3.0, 6.0));
        batch.push(&row("b", 4.0, 7.0));
        batch.push(&row_without_geometry("ghost"));
        writer.write(&mut batch).unwrap();
        let stats = writer.finish(&["Point".to_string()]).unwrap();
        assert_eq!(stats.rows, 3);
        let geo = geo_metadata(&path);
        assert_eq!(
            geo["columns"]["geometry"]["bbox"],
            serde_json::json!([3.0, 6.0, 4.0, 7.0])
        );
    }

    #[test]
    fn a_file_with_no_geometry_at_all_has_no_bbox_and_no_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.parquet");
        let profile = crate::profile::Profile::base();
        let schema = crate::write::schema::output_schema_optional_geometry(&profile);
        let mut writer = PartitionWriter::create_with_schema(&path, 10, schema.clone()).unwrap();
        let mut batch = RowBatch::for_profile(&profile, true);
        batch.push(&row_without_geometry("ghost"));
        batch.push(&row_without_geometry("remote"));
        writer.write(&mut batch).unwrap();
        let stats = writer.finish(&[]).unwrap();
        assert_eq!(stats.rows, 2);
        let column = &geo_metadata(&path)["columns"]["geometry"];
        assert!(column.get("bbox").is_none(), "{column}");
        assert_eq!(column["geometry_types"], serde_json::json!([]));
        assert!(column.get("covering").is_some());

        // The geometry columns are optional in the file itself.
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let fields = reader
            .metadata()
            .file_metadata()
            .schema()
            .get_fields()
            .to_vec();
        for name in ["geometry", "geom_type", "lon", "lat", "bbox"] {
            let field = fields.iter().find(|f| f.name() == name).unwrap();
            assert!(field.is_optional(), "{name} should be optional");
        }
    }

    #[test]
    fn empty_batch_write_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-batch.parquet");
        let mut writer = PartitionWriter::create(&path, 10).unwrap();
        let mut batch = RowBatch::new();
        batch.push(&row("a", 1.0, 2.0));
        writer.write(&mut batch).unwrap();
        // An empty batch between two real writes must not add a row group.
        writer.write(&mut RowBatch::new()).unwrap();
        batch.push(&row("b", 3.0, 4.0));
        writer.write(&mut batch).unwrap();
        let stats = writer.finish(&["Point".to_string()]).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.row_groups, 2);
    }

    #[test]
    fn zero_row_group_size_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zero-size.parquet");
        let mut writer = PartitionWriter::create(&path, 0).unwrap();
        let mut batch = RowBatch::new();
        batch.push(&row("a", 1.0, 2.0));
        writer.write(&mut batch).unwrap();
        let stats = writer.finish(&["Point".to_string()]).unwrap();
        assert_eq!(stats.rows, 1);
    }

    #[test]
    fn duckdb_reads_the_written_partition_if_available() {
        if std::process::Command::new("duckdb")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("duckdb not on PATH, skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duckdb-check.parquet");
        let mut writer = PartitionWriter::create(&path, 10).unwrap();
        let mut batch = RowBatch::new();
        for (i, (x, y)) in [(3.0, 6.0), (3.5, 6.5), (50.0, 50.0)].iter().enumerate() {
            batch.push(&row(&format!("r{i}"), *x, *y));
        }
        writer.write(&mut batch).unwrap();
        writer.finish(&["Point".to_string()]).unwrap();

        let output = std::process::Command::new("duckdb")
            .args([
                "-csv",
                "-c",
                &format!("SELECT count(*) FROM '{}'", path.display()),
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        eprintln!("duckdb output: {stdout}");
        assert!(stdout.contains('3'), "expected count 3, got: {stdout}");
    }
}
