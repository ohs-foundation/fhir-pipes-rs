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
//!
//! A value that does not match its column's type is converted where nothing
//! is lost, and otherwise written as null:
//!
//! - an integral number such as `5.0` in an `integer` or `integer64` column
//!   is written as the integer `5`; `5.5` does not fit;
//! - with [`TemporalEncoding::Native`], a `dateTime` with a time
//!   (`2026-06-15T23:30:00-05:00`) in a `date` column is written as its date
//!   part as written (`2026-06-15`), without converting to UTC first;
//! - anything else that cannot be converted is written as null.
//!
//! [`OutputWriter::write_with_summary`] returns how many values of each
//! column were written as their date or as null.
//!
//! By default the whole file is built in memory and then written. With
//! [`ParquetWriter::with_row_group_size`] it is written a row group at a
//! time, so only one row group's arrays and encoded bytes are held at once.

use std::io::Write;
use std::sync::Arc;

use serde_json::Value;

use arrow::array::{
    ArrayRef, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use super::{ColumnSummary, OutputWriter, WriteSummary};
use crate::column::{ColumnInfo, ColumnType};
use crate::runner::ViewResult;
use crate::{Error, Result};

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
    /// in the wild) is read as UTC. A `dateTime` with a time in a `date`
    /// column is written as its date part as written. Values that cannot be
    /// parsed are written as null. Both are counted in the
    /// [`WriteSummary`](super::WriteSummary).
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

/// Parquet output writer configuration. Files are written with Snappy
/// compression.
#[derive(Debug, Clone, Default)]
pub struct ParquetWriter {
    /// How temporal columns are encoded.
    pub temporal: TemporalEncoding,
    /// The most rows in a row group, written out as soon as it is full; with
    /// `None`, the file is built whole before it is written.
    pub row_group_size: Option<usize>,
}

impl ParquetWriter {
    /// Create a new Parquet writer with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set how temporal columns are encoded.
    pub fn with_temporal(mut self, temporal: TemporalEncoding) -> Self {
        self.temporal = temporal;
        self
    }

    /// Write row groups of at most `rows` rows (at least one), each to the
    /// output as soon as it is encoded, rather than building the whole file
    /// first. The file reads back the same; only its row groups differ.
    pub fn with_row_group_size(mut self, rows: usize) -> Self {
        self.row_group_size = Some(rows.max(1));
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

    /// Build an Arrow schema from column info. Every column is nullable.
    fn build_schema(columns: &[ColumnInfo], temporal: TemporalEncoding) -> Schema {
        let fields: Vec<Field> = columns
            .iter()
            .map(|c| {
                Field::new(
                    &c.name,
                    Self::column_to_arrow_type(c.col_type, temporal),
                    true,
                )
            })
            .collect();

        Schema::new(fields)
    }

    /// Build Arrow arrays from result data, adding the values of each column
    /// that did not match its type to `counts`, one per column.
    fn build_arrays(
        columns: &[ColumnInfo],
        data: &[Vec<Value>],
        temporal: TemporalEncoding,
        counts: &mut [Counts],
    ) -> Vec<ArrayRef> {
        columns
            .iter()
            .zip(counts)
            .enumerate()
            .map(|(i, (col, counts))| {
                let values: Vec<Option<&Value>> = data
                    .iter()
                    .map(|row| row.get(i).filter(|v| !v.is_null()))
                    .collect();
                Self::values_to_array(col, &values, temporal, counts)
            })
            .collect()
    }

    /// The columns with values that did not match their type, in column order.
    fn summary(columns: &[ColumnInfo], counts: &[Counts]) -> WriteSummary {
        let columns = columns
            .iter()
            .zip(counts)
            .filter(|(_, c)| c.written_as_date > 0 || c.written_as_null > 0)
            .map(|(col, c)| ColumnSummary {
                name: col.name.clone(),
                col_type: col.col_type,
                written_as_date: c.written_as_date,
                written_as_null: c.written_as_null,
            })
            .collect();
        WriteSummary { columns }
    }

    /// Convert JSON values to an Arrow array, counting the values that had to
    /// be written as their date or as null.
    fn values_to_array(
        col: &ColumnInfo,
        values: &[Option<&Value>],
        temporal: TemporalEncoding,
        counts: &mut Counts,
    ) -> ArrayRef {
        let array: ArrayRef = match (col.col_type, temporal) {
            (ColumnType::Integer | ColumnType::Integer64, _) => Arc::new(
                values
                    .iter()
                    .map(|v| counts.fit(*v, convert::integer))
                    .collect::<Int64Array>(),
            ),
            (ColumnType::Decimal, _) => Arc::new(
                values
                    .iter()
                    .map(|v| counts.fit(*v, Value::as_f64))
                    .collect::<Float64Array>(),
            ),
            (ColumnType::Boolean, _) => Arc::new(
                values
                    .iter()
                    .map(|v| counts.fit(*v, Value::as_bool))
                    .collect::<BooleanArray>(),
            ),
            (ColumnType::Date, TemporalEncoding::Native) => Arc::new(
                values
                    .iter()
                    .map(|v| {
                        let (days, from_datetime) = counts.fit(*v, convert::date)?;
                        if from_datetime {
                            counts.written_as_date += 1;
                        }
                        Some(days)
                    })
                    .collect::<Date32Array>(),
            ),
            (ColumnType::DateTime | ColumnType::Instant, TemporalEncoding::Native) => Arc::new(
                values
                    .iter()
                    .map(|v| counts.fit(*v, convert::timestamp))
                    .collect::<TimestampMicrosecondArray>()
                    .with_timezone(temporal::UTC),
            ),
            (ColumnType::Time, TemporalEncoding::Native) => Arc::new(
                values
                    .iter()
                    .map(|v| counts.fit(*v, convert::time))
                    .collect::<Time64MicrosecondArray>(),
            ),
            // All string-like types
            _ => Arc::new(
                values
                    .iter()
                    .map(|v| {
                        v.map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .collect::<StringArray>(),
            ),
        };
        array
    }

    /// Write result data to `output` as Parquet: a row group at a time with
    /// a row group size, otherwise the whole file at the end. Each row group
    /// is encoded into a buffer that is emptied into `output` once it is
    /// written; the Parquet writer counts the bytes itself, so the offsets in
    /// the footer stay right.
    fn write_rows(&self, result: &ViewResult, output: &mut dyn Write) -> Result<WriteSummary> {
        let schema = Arc::new(Self::build_schema(&result.columns, self.temporal));
        let mut counts: Vec<Counts> = result.columns.iter().map(|_| Counts::default()).collect();
        let mut props = WriterProperties::builder().set_compression(Compression::SNAPPY);
        if let Some(rows) = self.row_group_size {
            props = props.set_max_row_group_row_count(Some(rows));
        }
        let mut arrow_writer =
            ArrowWriter::try_new(Vec::new(), schema.clone(), Some(props.build()))
                .map_err(|e| Error::Output(format!("Failed to create Parquet writer: {}", e)))?;
        // No rows is one empty batch, as with no row group size.
        let data = &result.data;
        let slices: Vec<&[Vec<Value>]> = match self.row_group_size {
            Some(rows) if !data.is_empty() => data.chunks(rows).collect(),
            _ => vec![data.as_slice()],
        };
        let streaming = self.row_group_size.is_some();
        for slice in slices {
            let arrays = Self::build_arrays(&result.columns, slice, self.temporal, &mut counts);
            let record_batch = RecordBatch::try_new(schema.clone(), arrays)
                .map_err(|e| Error::Output(format!("Failed to create record batch: {}", e)))?;
            arrow_writer
                .write(&record_batch)
                .map_err(|e| Error::Output(format!("Failed to write record batch: {}", e)))?;
            if streaming {
                arrow_writer
                    .flush()
                    .map_err(|e| Error::Output(format!("Failed to write row group: {}", e)))?;
                let buffer = arrow_writer.inner_mut();
                output
                    .write_all(buffer)
                    .map_err(|e| Error::Output(e.to_string()))?;
                buffer.clear();
            }
        }
        let buffer = arrow_writer
            .into_inner()
            .map_err(|e| Error::Output(format!("Failed to close Parquet writer: {}", e)))?;
        output
            .write_all(&buffer)
            .map_err(|e| Error::Output(e.to_string()))?;
        Ok(Self::summary(&result.columns, &counts))
    }
}

/// One column's values that were not written as they are.
#[derive(Debug, Default)]
struct Counts {
    written_as_date: usize,
    written_as_null: usize,
}

impl Counts {
    /// Convert a non-null value, counting it when it cannot be converted and
    /// is written as null.
    fn fit<T>(
        &mut self,
        value: Option<&Value>,
        convert: impl Fn(&Value) -> Option<T>,
    ) -> Option<T> {
        let converted = convert(value?);
        if converted.is_none() {
            self.written_as_null += 1;
        }
        converted
    }
}

/// JSON values to the Arrow value of a typed column; `None` when the value
/// does not fit.
mod convert {
    use serde_json::Value;

    use super::temporal;

    /// An integer, or an integral float within the i64 range (`5.0` → 5).
    /// `5.5` is not rounded.
    pub fn integer(v: &Value) -> Option<i64> {
        if let Some(n) = v.as_i64() {
            return Some(n);
        }
        let f = v.as_f64().filter(|_| v.is_f64())?;
        // i64::MAX as f64 is 2^63, one past the largest i64.
        (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
    }

    /// Days since the epoch, and whether the value was a dateTime with a time
    /// written as its date.
    pub fn date(v: &Value) -> Option<(i32, bool)> {
        let s = v.as_str()?;
        if let Some(days) = temporal::date_to_days(s) {
            return Some((days, false));
        }
        temporal::datetime_date_to_days(s).map(|days| (days, true))
    }

    pub fn timestamp(v: &Value) -> Option<i64> {
        temporal::datetime_to_micros_utc(v.as_str()?)
    }

    pub fn time(v: &Value) -> Option<i64> {
        temporal::time_to_micros(v.as_str()?)
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

    /// Days since the Unix epoch of a `dateTime` with a time, from its date
    /// part as written: `2026-06-15T23:30:00-05:00` is 2026-06-15, not the
    /// 16th it is in UTC. `None` for a value without a time, or one that
    /// is not a valid dateTime.
    pub fn datetime_date_to_days(s: &str) -> Option<i32> {
        let s = s.trim();
        if s.len() <= 10 {
            return None;
        }
        datetime_to_micros_utc(s)?;
        date_to_days(s.get(..10)?)
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
    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        self.write_with_summary(result, output).map(|_| ())
    }

    fn write_with_summary(
        &self,
        result: &ViewResult,
        output: &mut dyn Write,
    ) -> Result<WriteSummary> {
        let summary = self.write_rows(result, output)?;
        output.flush().map_err(|e| Error::Output(e.to_string()))?;
        Ok(summary)
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
        round_trip_with_summary(writer, result).0
    }

    /// As `round_trip`, with the writer's summary.
    fn round_trip_with_summary(
        writer: &ParquetWriter,
        result: &ViewResult,
    ) -> (RecordBatch, WriteSummary) {
        let mut output = Vec::new();
        let summary = writer.write_with_summary(result, &mut output).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(output))
            .unwrap()
            .build()
            .unwrap();
        let schema = reader.schema();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        assert!(batches.len() <= 1, "expected at most one record batch");
        let batch = batches
            .into_iter()
            .next()
            .unwrap_or_else(|| RecordBatch::new_empty(schema));
        (batch, summary)
    }

    /// A one-column result.
    fn column_of(col_type: ColumnType, values: Vec<Value>) -> ViewResult {
        ViewResult {
            columns: vec![ColumnInfo::new("c", col_type)],
            data: values.into_iter().map(|v| vec![v]).collect(),
        }
    }

    fn native() -> ParquetWriter {
        ParquetWriter::new().with_temporal(TemporalEncoding::Native)
    }

    /// The summary's (written_as_date, written_as_null) for column `c`.
    fn counts(summary: &WriteSummary) -> (usize, usize) {
        summary
            .columns
            .iter()
            .find(|c| c.name == "c")
            .map_or((0, 0), |c| (c.written_as_date, c.written_as_null))
    }

    #[test]
    fn test_parquet_writer_default() {
        let writer = ParquetWriter::new();
        assert_eq!(writer.temporal, TemporalEncoding::Text);
    }

    #[test]
    fn columns_are_snappy_compressed() {
        use parquet::file::reader::{FileReader, SerializedFileReader};
        let result = column_of(ColumnType::String, vec![json!("a"), json!("b")]);
        for writer in [ParquetWriter::new(), native()] {
            let mut output = Vec::new();
            writer.write(&result, &mut output).unwrap();
            let reader = SerializedFileReader::new(bytes::Bytes::from(output)).unwrap();
            let row_group = reader.metadata().row_group(0);
            assert_eq!(row_group.column(0).compression(), Compression::SNAPPY);
        }
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
    fn integral_floats_are_written_as_integers() {
        for col_type in [ColumnType::Integer, ColumnType::Integer64] {
            let result = column_of(
                col_type,
                vec![
                    json!(5.0),
                    json!(-3.0),
                    json!(7),
                    json!(5.5),
                    json!("5"),
                    json!(true),
                    json!(1e20),
                    json!(u64::MAX),
                    Value::Null,
                ],
            );
            let (batch, summary) = round_trip_with_summary(&ParquetWriter::new(), &result);
            let c = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!(c.value(0), 5);
            assert_eq!(c.value(1), -3);
            assert_eq!(c.value(2), 7);
            // 5.5 is not rounded; a string, a boolean and numbers past the
            // i64 range do not fit either.
            for i in 3..8 {
                assert!(c.is_null(i), "{col_type} row {i}");
            }
            // A null is not a mismatch.
            assert!(c.is_null(8));
            assert_eq!(counts(&summary), (0, 5), "{col_type}");
            assert_eq!(summary.columns[0].col_type, col_type);
        }
    }

    #[test]
    fn integer_range_edges() {
        assert_eq!(convert::integer(&json!(i64::MIN as f64)), Some(i64::MIN));
        // 2^63 is one past i64::MAX.
        assert_eq!(convert::integer(&json!(9_223_372_036_854_775_808.0)), None);
        assert_eq!(convert::integer(&json!(-0.0)), Some(0));
    }

    #[test]
    fn datetimes_in_a_date_column_are_written_as_their_date_as_written() {
        let result = column_of(
            ColumnType::Date,
            vec![
                json!("2026-06-15T08:30:00+01:00"),
                // 2026-06-16 in UTC; the date as written is kept.
                json!("2026-06-15T23:30:00-05:00"),
                json!("2026-06-15T00:00:00Z"),
                json!("2026-06-15"),
                json!("2026-06-15Tlater"),
            ],
        );
        let (batch, summary) = round_trip_with_summary(&native(), &result);
        let c = batch
            .column(0)
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap();
        let day = temporal::date_to_days("2026-06-15").unwrap();
        for i in 0..4 {
            assert_eq!(c.value(i), day, "row {i}");
        }
        assert!(c.is_null(4));
        assert_eq!(counts(&summary), (3, 1));
        assert_eq!(
            summary.messages(),
            [
                "c: 3 dateTime values written as their date",
                "c: 1 value did not fit date and was written as null",
            ]
        );
    }

    #[test]
    fn text_mode_writes_temporal_values_as_they_are() {
        let result = column_of(
            ColumnType::Date,
            vec![json!("2026-06-15T08:30:00+01:00"), json!("2026"), json!(12)],
        );
        let (batch, summary) = round_trip_with_summary(&ParquetWriter::new(), &result);
        let c = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(c.value(0), "2026-06-15T08:30:00+01:00");
        assert_eq!(c.value(1), "2026");
        assert_eq!(c.value(2), "12");
        assert!(summary.is_empty());
    }

    #[test]
    fn partial_dates_are_floored_and_not_counted() {
        // date: `2026` and `2026-06` floor to the first day.
        let (batch, summary) = round_trip_with_summary(
            &native(),
            &column_of(ColumnType::Date, vec![json!("2026"), json!("2026-06")]),
        );
        let c = batch
            .column(0)
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap();
        assert_eq!(c.value(0), temporal::date_to_days("2026-01-01").unwrap());
        assert_eq!(c.value(1), temporal::date_to_days("2026-06-01").unwrap());
        assert!(summary.is_empty());

        // dateTime and instant: a partial or date-only value floors to
        // midnight UTC.
        for col_type in [ColumnType::DateTime, ColumnType::Instant] {
            let (batch, summary) = round_trip_with_summary(
                &native(),
                &column_of(
                    col_type,
                    vec![json!("2026"), json!("2026-06"), json!("2026-06-15")],
                ),
            );
            let c = batch
                .column(0)
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            let m = |s| temporal::datetime_to_micros_utc(s).unwrap();
            assert_eq!(c.value(0), m("2026-01-01T00:00:00Z"), "{col_type}");
            assert_eq!(c.value(1), m("2026-06-01T00:00:00Z"), "{col_type}");
            assert_eq!(c.value(2), m("2026-06-15T00:00:00Z"), "{col_type}");
            assert!(summary.is_empty(), "{col_type}");
        }
    }

    #[test]
    fn values_that_do_not_fit_are_null_and_counted() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("amount", ColumnType::Decimal),
                ColumnInfo::new("active", ColumnType::Boolean),
                ColumnInfo::new("recorded", ColumnType::DateTime),
                ColumnInfo::new("at", ColumnType::Time),
                ColumnInfo::new("count", ColumnType::Integer),
            ],
            data: vec![
                vec![
                    json!({"a": 1}),
                    json!(3),
                    json!(false),
                    json!("2026-06-15T10:30:00+02:00"),
                    json!("10:30:00"),
                    json!(1),
                ],
                vec![
                    json!("b"),
                    json!("3.5"),
                    json!("true"),
                    json!(12345),
                    json!("noon"),
                    json!(1.5),
                ],
            ],
        };
        let (batch, summary) = round_trip_with_summary(&native(), &result);
        // A string column takes anything; the others null what does not fit.
        for i in 1..6 {
            assert!(batch.column(i).is_valid(0), "column {i} row 0");
            assert!(batch.column(i).is_null(1), "column {i} row 1");
        }
        assert_eq!(
            summary.messages(),
            [
                "amount: 1 value did not fit decimal and was written as null",
                "active: 1 value did not fit boolean and was written as null",
                "recorded: 1 value did not fit dateTime and was written as null",
                "at: 1 value did not fit time and was written as null",
                "count: 1 value did not fit integer and was written as null",
            ]
        );
    }

    #[test]
    fn test_parquet_empty_result() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![],
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
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Should write successfully with null value
        assert!(output.len() > 4);
    }

    /// Write a result and read it back: its schema, all its rows in one
    /// batch, and the row count of each row group.
    fn read_back(writer: &ParquetWriter, result: &ViewResult) -> (RecordBatch, Vec<i64>) {
        let mut output = Vec::new();
        writer.write(result, &mut output).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(output)).unwrap();
        let groups = builder
            .metadata()
            .row_groups()
            .iter()
            .map(|g| g.num_rows())
            .collect();
        let reader = builder.build().unwrap();
        let schema = reader.schema();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        let batch = arrow::compute::concat_batches(&schema, &batches).unwrap();
        (batch, groups)
    }

    /// `n` rows of every kind of column, with a value that does not fit its
    /// type now and then, in different columns in different rows.
    fn mixed_rows(n: usize) -> ViewResult {
        ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("count", ColumnType::Integer),
                ColumnInfo::new("ratio", ColumnType::Decimal),
                ColumnInfo::new("active", ColumnType::Boolean),
                ColumnInfo::new("born", ColumnType::Date),
                ColumnInfo::new("seen", ColumnType::DateTime),
            ],
            data: (0..n)
                .map(|i| {
                    vec![
                        json!(format!("r{i}")),
                        if i % 7 == 6 { json!("x") } else { json!(i) },
                        json!(i as f64 / 4.0),
                        if i % 3 == 0 {
                            Value::Null
                        } else {
                            json!(i % 2 == 0)
                        },
                        if i % 5 == 4 {
                            json!("2026-06-15T08:30:00Z")
                        } else {
                            json!("2026-06-15")
                        },
                        json!(format!("2026-06-15T08:{:02}:00Z", i % 60)),
                    ]
                })
                .collect(),
        }
    }

    #[test]
    fn row_groups_read_back_as_the_whole_file_does() {
        let result = mixed_rows(10);
        let (whole, groups) = read_back(&native(), &result);
        assert_eq!(groups, [10]);
        for (size, expect) in [
            (4, vec![4, 4, 2]),
            (5, vec![5, 5]),
            (10, vec![10]),
            (64, vec![10]),
            (1, vec![1; 10]),
        ] {
            let writer = native().with_row_group_size(size);
            let (batch, groups) = read_back(&writer, &result);
            assert_eq!(groups, expect, "row group size {size}");
            assert_eq!(batch.schema(), whole.schema(), "row group size {size}");
            assert_eq!(batch, whole, "row group size {size}");
        }
    }

    #[test]
    fn row_groups_give_the_whole_file_summary_in_column_order() {
        // The first mismatches are in `born` (row 4), then `count` (row 6):
        // the summary still lists `count` first.
        let result = mixed_rows(12);
        let mut whole = Vec::new();
        let expected = native().write_with_summary(&result, &mut whole).unwrap();
        let names: Vec<&str> = expected.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["count", "born"]);
        let mut out = Vec::new();
        let summary = native()
            .with_row_group_size(5)
            .write_with_summary(&result, &mut out)
            .unwrap();
        assert_eq!(summary, expected);
    }

    #[test]
    fn row_groups_with_no_rows_still_write_the_columns() {
        let result = mixed_rows(0);
        let (whole, _) = read_back(&native(), &result);
        let (batch, groups) = read_back(&native().with_row_group_size(4), &result);
        assert_eq!(batch.num_rows(), 0);
        assert!(groups.iter().all(|&n| n == 0));
        assert_eq!(batch.schema(), whole.schema());
    }

    #[test]
    fn a_row_group_is_written_out_before_the_next_is_built() {
        /// Records the size of each write it is given.
        struct Writes(Vec<usize>);
        impl Write for Writes {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.push(buf.len());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // 4000 distinct 64-character strings, which neither the dictionary
        // nor Snappy make much smaller: about 64 kB a row group.
        let result = column_of(
            ColumnType::String,
            (0..4000u64)
                .map(|i| {
                    let x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    json!(format!("{x:016x}{:016x}{:016x}{:016x}", !x, x ^ 7, x >> 3))
                })
                .collect(),
        );
        let mut whole = Writes(Vec::new());
        ParquetWriter::new().write(&result, &mut whole).unwrap();
        assert_eq!(whole.0.len(), 1, "the whole file in one write");
        let total: usize = whole.0.iter().sum();
        let mut groups = Writes(Vec::new());
        ParquetWriter::new()
            .with_row_group_size(1000)
            .write(&result, &mut groups)
            .unwrap();
        // A write for each row group and the footer, none of them more than
        // about a row group's bytes.
        assert!(groups.0.len() >= 5, "{:?}", groups.0);
        let largest = groups.0.iter().max().unwrap();
        assert!(*largest < total / 3, "{largest} of {total}");
    }
}
