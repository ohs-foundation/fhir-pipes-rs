//! Output format writers for view results.
//!
//! This module provides writers for different output formats:
//! - CSV
//! - NDJSON (Newline Delimited JSON)
//! - JSON Array
//! - Parquet (when the `parquet` feature is enabled)

mod csv;
mod ndjson;

pub use csv::CsvWriter;
pub use ndjson::{JsonArrayWriter, NdjsonWriter};

#[cfg(feature = "parquet")]
mod parquet_writer;

#[cfg(feature = "parquet")]
pub use parquet_writer::{ParquetWriter, TemporalEncoding};

use std::io::Write;

use crate::column::ColumnType;
use crate::runner::ViewResult;
use crate::{Error, Result};

/// Trait for writing view results to different output formats.
pub trait OutputWriter: Send + Sync {
    /// Write the view result to the output.
    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()>;

    /// Write the view result to the output, and return the values that were
    /// not written as they are because they did not match their column's
    /// type. CSV and JSON write every value as it is, so their summary is
    /// empty; Parquet's typed columns can need a value converted or nulled.
    fn write_with_summary(
        &self,
        result: &ViewResult,
        output: &mut dyn Write,
    ) -> Result<WriteSummary> {
        self.write(result, output)?;
        Ok(WriteSummary::default())
    }
}

/// Values a writer did not write as they are, per column: those that did not
/// match the column's type. Only columns with such values are listed, in
/// column order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteSummary {
    /// One entry per column with a count above zero.
    pub columns: Vec<ColumnSummary>,
}

/// The values of one column that did not match its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSummary {
    /// The column name.
    pub name: String,
    /// The column's declared type.
    pub col_type: ColumnType,
    /// dateTime values with a time, written to a `date` column as the date
    /// part as written (not converted to UTC first).
    pub written_as_date: usize,
    /// Values that could not be converted to the column's type, written as
    /// null.
    pub written_as_null: usize,
}

impl WriteSummary {
    /// Whether every value was written as it is.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Add `other`'s counts to these, by column name: the summaries of several
    /// files written from one view (such as its partitions) as one.
    pub fn merge(&mut self, other: WriteSummary) {
        for c in other.columns {
            match self.columns.iter_mut().find(|m| m.name == c.name) {
                Some(m) => {
                    m.written_as_date += c.written_as_date;
                    m.written_as_null += c.written_as_null;
                }
                None => self.columns.push(c),
            }
        }
    }

    /// One line per count, starting with the column name, e.g.
    /// `value: 3 values did not fit integer and were written as null`.
    pub fn messages(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.columns {
            if c.written_as_date > 0 {
                let n = c.written_as_date;
                out.push(if n == 1 {
                    format!("{}: 1 dateTime value written as its date", c.name)
                } else {
                    format!("{}: {n} dateTime values written as their date", c.name)
                });
            }
            if c.written_as_null > 0 {
                let n = c.written_as_null;
                out.push(if n == 1 {
                    format!(
                        "{}: 1 value did not fit {} and was written as null",
                        c.name, c.col_type
                    )
                } else {
                    format!(
                        "{}: {n} values did not fit {} and were written as null",
                        c.name, c.col_type
                    )
                });
            }
        }
        out
    }
}

/// Get a writer for the specified format.
///
/// # Errors
///
/// Returns an error if the format is not recognized.
pub fn get_writer(format: &str) -> Result<Box<dyn OutputWriter>> {
    match format.to_lowercase().as_str() {
        "csv" => Ok(Box::new(CsvWriter::new())),
        "ndjson" | "jsonl" => Ok(Box::new(NdjsonWriter::new())),
        "json" => Ok(Box::new(JsonArrayWriter::new())),
        #[cfg(feature = "parquet")]
        "parquet" => Ok(Box::new(ParquetWriter::new())),
        _ => Err(Error::Output(format!("Unknown format: {}", format))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnInfo;

    #[test]
    fn write_summary_merges_by_column_and_words_its_counts() {
        let col = |name: &str, col_type, date, null| ColumnSummary {
            name: name.into(),
            col_type,
            written_as_date: date,
            written_as_null: null,
        };
        let mut a = WriteSummary {
            columns: vec![col("start", ColumnType::Date, 2, 0)],
        };
        a.merge(WriteSummary {
            columns: vec![
                col("value", ColumnType::Integer, 0, 1),
                col("start", ColumnType::Date, 1, 1),
            ],
        });
        assert_eq!(
            a.messages(),
            [
                "start: 3 dateTime values written as their date",
                "start: 1 value did not fit date and was written as null",
                "value: 1 value did not fit integer and was written as null",
            ]
        );
        assert!(WriteSummary::default().is_empty());
    }

    #[test]
    fn csv_and_ndjson_summaries_are_empty() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("n", ColumnType::Integer)],
            data: vec![vec![serde_json::json!(5.5)], vec![serde_json::json!("x")]],
        };
        for format in ["csv", "ndjson", "json"] {
            let mut out = Vec::new();
            let summary = get_writer(format)
                .unwrap()
                .write_with_summary(&result, &mut out)
                .unwrap();
            assert!(summary.is_empty(), "{format}");
            // The values are written as they are.
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("5.5") && text.contains('x'),
                "{format}: {text}"
            );
        }
    }

    #[test]
    fn test_get_writer() {
        assert!(get_writer("csv").is_ok());
        assert!(get_writer("ndjson").is_ok());
        assert!(get_writer("json").is_ok());
        assert!(get_writer("unknown").is_err());
    }
}
