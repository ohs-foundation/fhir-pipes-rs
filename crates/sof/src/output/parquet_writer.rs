//! Parquet output writer for view results.
//!
//! This module is only available when the `parquet` feature is enabled.
//!
//! Temporal columns (`date`, `dateTime`, `instant`, `time`) are written as
//! UTF-8 strings by default, preserving the FHIR representation exactly as the
//! SQL-on-FHIR spec's ANSI type mapping recommends. Opt in to
//! [`TemporalEncoding::Native`] to write them as Arrow `Date32`,
//! `Timestamp(µs, UTC)` and `Time64(µs)` instead, so downstream engines
//! (DuckDB, ClickHouse, Spark, pandas) see real temporal types without a cast.

use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use arrow::array::{
    ArrayRef, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use super::{AsyncOutputWriter, OutputWriter};
use crate::column::{ColumnInfo, ColumnType};
use crate::runner::ViewResult;
use crate::{Error, Result};

/// Parquet compression codecs.
#[derive(Debug, Clone, Copy, Default)]
pub enum ParquetCompression {
    /// No compression.
    None,
    /// Snappy compression (default, good balance of speed and ratio).
    #[default]
    Snappy,
    /// Gzip compression (better ratio, slower).
    Gzip,
    /// LZ4 compression (faster, lower ratio).
    Lz4,
    /// Zstd compression (good balance).
    Zstd,
}

impl From<ParquetCompression> for Compression {
    fn from(compression: ParquetCompression) -> Self {
        match compression {
            ParquetCompression::None => Compression::UNCOMPRESSED,
            ParquetCompression::Snappy => Compression::SNAPPY,
            ParquetCompression::Gzip => Compression::GZIP(Default::default()),
            ParquetCompression::Lz4 => Compression::LZ4,
            ParquetCompression::Zstd => Compression::ZSTD(Default::default()),
        }
    }
}

/// How temporal columns (`date`, `dateTime`, `instant`, `time`) are encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TemporalEncoding {
    /// Keep the FHIR string representation (`Utf8`). This is the default and
    /// matches the spec's ANSI type mapping, which preserves partial dates
    /// such as `2026` or `2026-03` verbatim.
    #[default]
    Text,

    /// Write native Arrow temporal types:
    ///
    /// | column type          | Arrow type                    |
    /// |----------------------|-------------------------------|
    /// | `date`               | `Date32`                      |
    /// | `dateTime`, `instant`| `Timestamp(Microsecond, UTC)` |
    /// | `time`               | `Time64(Microsecond)`         |
    ///
    /// FHIR allows reduced-precision values. Those are **floored** to the
    /// start of the period they name: `2026` → `2026-01-01`, `2026-03` →
    /// `2026-03-01`, and a `dateTime` without a time part → midnight UTC.
    /// A `dateTime` with a time part but no offset (not valid FHIR, but seen
    /// in the wild) is read as UTC. Values that cannot be parsed are written
    /// as null and logged at `warn`.
    Native,
}

impl TemporalEncoding {
    /// Parse from a string (`text` | `native`), case-insensitive.
    ///
    /// # Errors
    ///
    /// Returns an error for any other value.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "text" | "string" => Ok(Self::Text),
            "native" => Ok(Self::Native),
            other => Err(Error::Output(format!(
                "Unknown temporal encoding: {other} (expected `text` or `native`)"
            ))),
        }
    }
}

impl std::str::FromStr for TemporalEncoding {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Parquet output writer configuration.
#[derive(Debug, Clone, Default)]
pub struct ParquetWriter {
    /// Compression codec to use.
    pub compression: ParquetCompression,
    /// How temporal columns are encoded.
    pub temporal: TemporalEncoding,
}

impl ParquetWriter {
    /// Create a new Parquet writer with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the compression codec.
    pub fn with_compression(mut self, compression: ParquetCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Set how temporal columns are encoded.
    pub fn with_temporal(mut self, temporal: TemporalEncoding) -> Self {
        self.temporal = temporal;
        self
    }

    /// Convert a column type to an Arrow data type.
    fn column_to_arrow_type(col_type: ColumnType, temporal: TemporalEncoding) -> DataType {
        match (col_type, temporal) {
            (ColumnType::Integer | ColumnType::Integer64, _) => DataType::Int64,
            (ColumnType::Decimal, _) => DataType::Float64,
            (ColumnType::Boolean, _) => DataType::Boolean,
            (ColumnType::Date, TemporalEncoding::Native) => DataType::Date32,
            (ColumnType::DateTime | ColumnType::Instant, TemporalEncoding::Native) => {
                DataType::Timestamp(TimeUnit::Microsecond, Some(temporal::UTC.into()))
            }
            (ColumnType::Time, TemporalEncoding::Native) => DataType::Time64(TimeUnit::Microsecond),
            // Everything else (and all temporal types in `Text` mode) is a string.
            (
                ColumnType::String
                | ColumnType::Date
                | ColumnType::DateTime
                | ColumnType::Instant
                | ColumnType::Time
                | ColumnType::Base64Binary
                | ColumnType::Json,
                _,
            ) => DataType::Utf8,
        }
    }

    /// Build an Arrow schema from column info.
    fn build_schema(columns: &[ColumnInfo], temporal: TemporalEncoding) -> Schema {
        let fields: Vec<Field> = columns
            .iter()
            .map(|c| {
                Field::new(
                    &c.name,
                    Self::column_to_arrow_type(c.col_type, temporal),
                    c.nullable,
                )
            })
            .collect();

        Schema::new(fields)
    }

    /// Build Arrow arrays from result data.
    fn build_arrays(
        columns: &[ColumnInfo],
        data: &[Vec<Value>],
        temporal: TemporalEncoding,
    ) -> Result<Vec<ArrayRef>> {
        columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let values: Vec<Option<&Value>> = data
                    .iter()
                    .map(|row| row.get(i).filter(|v| !v.is_null()))
                    .collect();

                Self::values_to_array(col, &values, temporal)
            })
            .collect()
    }

    /// Convert JSON values to an Arrow array.
    fn values_to_array(
        col: &ColumnInfo,
        values: &[Option<&Value>],
        temporal: TemporalEncoding,
    ) -> Result<ArrayRef> {
        match (col.col_type, temporal) {
            (ColumnType::Integer | ColumnType::Integer64, _) => {
                let arr: Int64Array = values.iter().map(|v| v.and_then(|v| v.as_i64())).collect();
                Ok(Arc::new(arr))
            }
            (ColumnType::Decimal, _) => {
                let arr: Float64Array = values.iter().map(|v| v.and_then(|v| v.as_f64())).collect();
                Ok(Arc::new(arr))
            }
            (ColumnType::Boolean, _) => {
                let arr: BooleanArray =
                    values.iter().map(|v| v.and_then(|v| v.as_bool())).collect();
                Ok(Arc::new(arr))
            }
            (ColumnType::Date, TemporalEncoding::Native) => {
                let arr: Date32Array = values
                    .iter()
                    .map(|v| Self::temporal_value(col, v, temporal::date_to_days))
                    .collect();
                Ok(Arc::new(arr))
            }
            (ColumnType::DateTime | ColumnType::Instant, TemporalEncoding::Native) => {
                let arr: TimestampMicrosecondArray = values
                    .iter()
                    .map(|v| Self::temporal_value(col, v, temporal::datetime_to_micros_utc))
                    .collect::<TimestampMicrosecondArray>()
                    .with_timezone(temporal::UTC);
                Ok(Arc::new(arr))
            }
            (ColumnType::Time, TemporalEncoding::Native) => {
                let arr: Time64MicrosecondArray = values
                    .iter()
                    .map(|v| Self::temporal_value(col, v, temporal::time_to_micros))
                    .collect();
                Ok(Arc::new(arr))
            }
            // All string-like types
            _ => {
                let arr: StringArray = values
                    .iter()
                    .map(|v| {
                        v.map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .collect();
                Ok(Arc::new(arr))
            }
        }
    }

    /// Apply a temporal parser to one JSON value, logging (and nulling) values
    /// that are not strings or do not parse. The log names the column, not the
    /// value, which may be patient data (as upstream octofhir/sof 0.1.3).
    fn temporal_value<T>(
        col: &ColumnInfo,
        value: &Option<&Value>,
        parse: fn(&str) -> Option<T>,
    ) -> Option<T> {
        let value = (*value)?;
        let Value::String(s) = value else {
            tracing::warn!(
                column = %col.name,
                "non-string value in {} column written as null",
                col.col_type
            );
            return None;
        };
        let parsed = parse(s);
        if parsed.is_none() {
            tracing::warn!(
                column = %col.name,
                "unparseable {} value written as null",
                col.col_type
            );
        }
        parsed
    }

    /// Write result data to a buffer as Parquet.
    fn write_to_buffer(&self, result: &ViewResult) -> Result<Vec<u8>> {
        let schema = Arc::new(Self::build_schema(&result.columns, self.temporal));
        let arrays = Self::build_arrays(&result.columns, &result.data, self.temporal)?;

        let record_batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|e| Error::Output(format!("Failed to create record batch: {}", e)))?;

        let mut buffer = Vec::new();
        {
            let props = WriterProperties::builder()
                .set_compression(self.compression.into())
                .build();

            let mut arrow_writer = ArrowWriter::try_new(&mut buffer, schema, Some(props))
                .map_err(|e| Error::Output(format!("Failed to create Parquet writer: {}", e)))?;

            arrow_writer
                .write(&record_batch)
                .map_err(|e| Error::Output(format!("Failed to write record batch: {}", e)))?;

            arrow_writer
                .close()
                .map_err(|e| Error::Output(format!("Failed to close Parquet writer: {}", e)))?;
        }

        Ok(buffer)
    }
}

/// FHIR temporal string parsing for [`TemporalEncoding::Native`].
///
/// FHIR primitives allow reduced precision: `date` is `YYYY`, `YYYY-MM` or
/// `YYYY-MM-DD`; `dateTime` is any of those or a full timestamp with a
/// mandatory offset; `instant` is always a full timestamp with an offset;
/// `time` is `hh:mm:ss` with optional fractional seconds. Partial values are
/// floored to the start of the period they name.
mod temporal {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

    /// Timezone name attached to timestamp columns.
    pub const UTC: &str = "UTC";

    /// Parse a FHIR `date` (or the date part of a `dateTime`), flooring
    /// partials to the first day of the period.
    pub fn parse_date(s: &str) -> Option<NaiveDate> {
        let s = s.trim();
        match s.len() {
            4 => NaiveDate::from_ymd_opt(s.parse().ok()?, 1, 1),
            7 => {
                let (y, m) = s.split_once('-')?;
                NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, 1)
            }
            10 => NaiveDate::parse_from_str(s, "%Y-%m-%d").ok(),
            _ => None,
        }
    }

    /// Days since the Unix epoch for an Arrow `Date32`.
    pub fn date_to_days(s: &str) -> Option<i32> {
        let date = parse_date(s)?;
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
        i32::try_from(date.signed_duration_since(epoch).num_days()).ok()
    }

    /// Microseconds since the Unix epoch, in UTC, for an Arrow
    /// `Timestamp(Microsecond, UTC)`.
    ///
    /// Accepts a FHIR `dateTime` or `instant`. A date-only value floors to
    /// midnight UTC. A timestamp with an offset is normalized to UTC. A
    /// timestamp with no offset (invalid FHIR) is read as UTC.
    pub fn datetime_to_micros_utc(s: &str) -> Option<i64> {
        let s = s.trim();
        if s.len() <= 10 {
            return parse_date(s)?
                .and_hms_opt(0, 0, 0)?
                .and_utc()
                .timestamp_micros()
                .into();
        }
        if let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(s) {
            return Some(dt.to_utc().timestamp_micros());
        }
        // No offset: FHIR requires one when a time is present, but be lenient.
        for fmt in [
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%dT%H:%M",
        ] {
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
                return Some(naive.and_utc().timestamp_micros());
            }
        }
        None
    }

    /// Microseconds since midnight for an Arrow `Time64(Microsecond)`.
    pub fn time_to_micros(s: &str) -> Option<i64> {
        let s = s.trim();
        let t = NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
            .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
            .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
            .ok()?;
        let secs = i64::from(t.num_seconds_from_midnight());
        let micros = secs * 1_000_000 + i64::from(t.nanosecond() / 1_000);
        // chrono reads 23:59:60 as a leap second, past the end of the day an
        // Arrow Time64 can hold.
        (micros < 86_400_000_000).then_some(micros)
    }
}

impl OutputWriter for ParquetWriter {
    fn content_type(&self) -> &'static str {
        "application/vnd.apache.parquet"
    }

    fn file_extension(&self) -> &'static str {
        "parquet"
    }

    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        let buffer = self.write_to_buffer(result)?;
        output
            .write_all(&buffer)
            .map_err(|e| Error::Output(e.to_string()))?;
        output.flush().map_err(|e| Error::Output(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl AsyncOutputWriter for ParquetWriter {
    fn content_type(&self) -> &'static str {
        "application/vnd.apache.parquet"
    }

    fn file_extension(&self) -> &'static str {
        "parquet"
    }

    async fn write<W: AsyncWrite + Unpin + Send>(
        &self,
        result: &ViewResult,
        mut writer: W,
    ) -> Result<()> {
        // Write to buffer first (Parquet writer needs sync write)
        let buffer = self.write_to_buffer(result)?;

        // Write buffer to async writer
        writer
            .write_all(&buffer)
            .await
            .map_err(|e| Error::Output(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::Output(e.to_string()))?;

        Ok(())
    }

    async fn write_streaming<W: AsyncWrite + Unpin + Send>(
        &self,
        columns: &[ColumnInfo],
        rows: Pin<Box<dyn Stream<Item = Vec<Value>> + Send>>,
        writer: W,
    ) -> Result<()> {
        // Parquet is a columnar format, so we need to buffer all rows
        // to build efficient column chunks
        let mut data = Vec::new();
        let mut rows = rows;
        while let Some(row) = rows.next().await {
            data.push(row);
        }

        let result = ViewResult {
            columns: columns.to_vec(),
            data,
            row_count: 0,
        };

        AsyncOutputWriter::write(self, &result, writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Array;
    use arrow::record_batch::RecordBatchReader;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use serde_json::json;

    /// Write a result and read it back as a single record batch.
    fn round_trip(writer: &ParquetWriter, result: &ViewResult) -> RecordBatch {
        let mut output = Vec::new();
        OutputWriter::write(writer, result, &mut output).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(output))
            .unwrap()
            .build()
            .unwrap();
        let schema = reader.schema();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        assert!(batches.len() <= 1, "expected at most one record batch");
        batches
            .into_iter()
            .next()
            .unwrap_or_else(|| RecordBatch::new_empty(schema))
    }

    #[test]
    fn test_parquet_writer_default() {
        let writer = ParquetWriter::new();
        assert!(matches!(writer.compression, ParquetCompression::Snappy));
        assert_eq!(writer.temporal, TemporalEncoding::Text);
    }

    #[test]
    fn test_parquet_compression_conversion() {
        assert!(matches!(
            Compression::from(ParquetCompression::None),
            Compression::UNCOMPRESSED
        ));
        assert!(matches!(
            Compression::from(ParquetCompression::Snappy),
            Compression::SNAPPY
        ));
    }

    #[test]
    fn test_temporal_encoding_parse() {
        assert_eq!(
            TemporalEncoding::parse("text").unwrap(),
            TemporalEncoding::Text
        );
        assert_eq!(
            TemporalEncoding::parse("Native").unwrap(),
            TemporalEncoding::Native
        );
        assert!(TemporalEncoding::parse("epoch").is_err());
    }

    #[test]
    fn test_column_to_arrow_type_text() {
        let t = TemporalEncoding::Text;
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Integer, t),
            DataType::Int64
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Decimal, t),
            DataType::Float64
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Boolean, t),
            DataType::Boolean
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::String, t),
            DataType::Utf8
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Date, t),
            DataType::Utf8
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::DateTime, t),
            DataType::Utf8
        );
    }

    #[test]
    fn test_column_to_arrow_type_native() {
        let t = TemporalEncoding::Native;
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Date, t),
            DataType::Date32
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::DateTime, t),
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Instant, t),
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Time, t),
            DataType::Time64(TimeUnit::Microsecond)
        );
        // Non-temporal types are unaffected by the encoding.
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::String, t),
            DataType::Utf8
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Integer, t),
            DataType::Int64
        );
    }

    #[test]
    fn test_build_schema() {
        let columns = vec![
            ColumnInfo::new("id", ColumnType::String),
            ColumnInfo::new("value", ColumnType::Integer),
            ColumnInfo::new("active", ColumnType::Boolean),
        ];

        let schema = ParquetWriter::build_schema(&columns, TemporalEncoding::Text);
        assert_eq!(schema.fields().len(), 3);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(1).name(), "value");
        assert_eq!(schema.field(1).data_type(), &DataType::Int64);
        assert_eq!(schema.field(2).name(), "active");
        assert_eq!(schema.field(2).data_type(), &DataType::Boolean);
    }

    #[test]
    fn test_parse_date_full_and_partial() {
        let d = |s: &str| temporal::parse_date(s).map(|d| d.to_string());
        assert_eq!(d("2026-06-15").as_deref(), Some("2026-06-15"));
        assert_eq!(d("2026-06").as_deref(), Some("2026-06-01"));
        assert_eq!(d("2026").as_deref(), Some("2026-01-01"));
        assert_eq!(d("2026-13"), None);
        assert_eq!(d("2026-02-30"), None);
        assert_eq!(d("June 2026"), None);
        assert_eq!(d(""), None);
    }

    #[test]
    fn test_date_to_days() {
        assert_eq!(temporal::date_to_days("1970-01-01"), Some(0));
        assert_eq!(temporal::date_to_days("1970-01-02"), Some(1));
        assert_eq!(temporal::date_to_days("1969-12-31"), Some(-1));
        assert_eq!(temporal::date_to_days("2026-06-15"), Some(20_619));
    }

    #[test]
    fn test_datetime_to_micros_utc() {
        let m = temporal::datetime_to_micros_utc;
        assert_eq!(m("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(m("1970-01-01T00:00:01Z"), Some(1_000_000));
        assert_eq!(m("1970-01-01T00:00:00.5Z"), Some(500_000));
        // Offsets normalize to UTC.
        assert_eq!(m("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(m("1969-12-31T23:00:00-01:00"), Some(0));
        // Date-only floors to midnight UTC.
        assert_eq!(m("1970-01-02"), Some(86_400 * 1_000_000));
        assert_eq!(m("1970-01"), Some(0));
        assert_eq!(m("1970"), Some(0));
        // Missing offset is read as UTC (lenient).
        assert_eq!(m("1970-01-01T00:00:02"), Some(2_000_000));
        assert_eq!(m("not a date"), None);
    }

    #[test]
    fn test_time_to_micros() {
        assert_eq!(temporal::time_to_micros("00:00:00"), Some(0));
        assert_eq!(temporal::time_to_micros("00:00:01"), Some(1_000_000));
        assert_eq!(temporal::time_to_micros("00:00:00.25"), Some(250_000));
        assert_eq!(
            temporal::time_to_micros("13:45:30"),
            Some((13 * 3600 + 45 * 60 + 30) * 1_000_000)
        );
        assert_eq!(temporal::time_to_micros("25:00:00"), None);
        // A leap second is past the end of the day a Time64 can hold.
        assert_eq!(temporal::time_to_micros("23:59:60"), None);
        assert_eq!(
            temporal::time_to_micros("23:59:59.999999"),
            Some(86_399_999_999)
        );
    }

    #[test]
    fn test_parquet_write() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("value", ColumnType::Integer),
            ],
            data: vec![vec![json!("1"), json!(100)], vec![json!("2"), json!(200)]],
            row_count: 2,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Parquet files start with "PAR1" magic bytes
        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }

    #[test]
    fn test_parquet_text_temporal_round_trip() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("start", ColumnType::Date),
            ],
            data: vec![
                vec![json!("a"), json!("2026-06-15")],
                vec![json!("b"), json!("2026-06")],
            ],
            row_count: 2,
        };

        let batch = round_trip(&ParquetWriter::new(), &result);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Utf8);
        let col = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        // Text mode preserves partial dates verbatim.
        assert_eq!(col.value(0), "2026-06-15");
        assert_eq!(col.value(1), "2026-06");
    }

    #[test]
    fn test_parquet_native_temporal_round_trip() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("start", ColumnType::Date),
                ColumnInfo::new("recorded", ColumnType::DateTime),
                ColumnInfo::new("issued", ColumnType::Instant),
                ColumnInfo::new("at", ColumnType::Time),
                ColumnInfo::new("count", ColumnType::Integer),
            ],
            data: vec![
                vec![
                    json!("full"),
                    json!("2026-06-15"),
                    json!("2026-06-15T10:30:00+02:00"),
                    json!("2026-06-15T08:30:00.250Z"),
                    json!("10:30:00"),
                    json!(1),
                ],
                vec![
                    json!("partial"),
                    json!("2026-06"),
                    json!("2026"),
                    Value::Null,
                    Value::Null,
                    json!(2),
                ],
                vec![
                    json!("bad"),
                    json!("June 2026"),
                    json!(12345),
                    json!("soon"),
                    json!("noon"),
                    json!(3),
                ],
            ],
            row_count: 3,
        };

        let writer = ParquetWriter::new().with_temporal(TemporalEncoding::Native);
        let batch = round_trip(&writer, &result);
        let schema = batch.schema();
        assert_eq!(schema.field(1).data_type(), &DataType::Date32);
        assert_eq!(
            schema.field(2).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(
            schema.field(3).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(
            schema.field(4).data_type(),
            &DataType::Time64(TimeUnit::Microsecond)
        );
        assert_eq!(schema.field(5).data_type(), &DataType::Int64);

        let start = batch
            .column(1)
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap();
        assert_eq!(
            start.value(0),
            temporal::date_to_days("2026-06-15").unwrap()
        );
        assert_eq!(
            start.value(1),
            temporal::date_to_days("2026-06-01").unwrap()
        );
        assert!(start.is_null(2));

        let recorded = batch
            .column(2)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(
            recorded.value(0),
            temporal::datetime_to_micros_utc("2026-06-15T08:30:00Z").unwrap()
        );
        assert_eq!(
            recorded.value(1),
            temporal::datetime_to_micros_utc("2026-01-01T00:00:00Z").unwrap()
        );
        assert!(recorded.is_null(2));

        let issued = batch
            .column(3)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(
            issued.value(0),
            temporal::datetime_to_micros_utc("2026-06-15T08:30:00.250Z").unwrap()
        );
        assert!(issued.is_null(1));
        assert!(issued.is_null(2));

        let at = batch
            .column(4)
            .as_any()
            .downcast_ref::<Time64MicrosecondArray>()
            .unwrap();
        assert_eq!(at.value(0), temporal::time_to_micros("10:30:00").unwrap());
        assert!(at.is_null(1));
        assert!(at.is_null(2));

        let count = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(count.value(2), 3);
    }

    #[test]
    fn test_parquet_empty_result() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![],
            row_count: 0,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Even empty files have the Parquet header
        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }

    #[test]
    fn test_parquet_native_empty_result() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("when", ColumnType::DateTime)],
            data: vec![],
            row_count: 0,
        };

        let writer = ParquetWriter::new().with_temporal(TemporalEncoding::Native);
        let batch = round_trip(&writer, &result);
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(
            batch.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }

    #[test]
    fn test_parquet_null_values() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("name", ColumnType::String),
            ],
            data: vec![vec![json!("1"), Value::Null]],
            row_count: 1,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Should write successfully with null value
        assert!(output.len() > 4);
    }

    #[test]
    fn test_content_type_and_extension() {
        let writer = ParquetWriter::new();
        assert_eq!(
            <ParquetWriter as OutputWriter>::content_type(&writer),
            "application/vnd.apache.parquet"
        );
        assert_eq!(
            <ParquetWriter as OutputWriter>::file_extension(&writer),
            "parquet"
        );
    }

    #[tokio::test]
    async fn test_async_parquet_writer() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("count", ColumnType::Integer),
            ],
            data: vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]],
            row_count: 2,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        AsyncOutputWriter::write(&writer, &result, &mut output)
            .await
            .unwrap();

        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }
}
