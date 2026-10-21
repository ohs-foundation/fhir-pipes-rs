//! NDJSON (Newline Delimited JSON) output writer for view results.

use std::io::Write;

use serde_json::Value;

use super::OutputWriter;
use crate::column::ColumnInfo;
use crate::runner::ViewResult;
use crate::{Error, Result};

/// NDJSON output writer.
///
/// Writes each row as a JSON object on a separate line, leaving out null
/// values.
#[derive(Debug, Clone, Default)]
pub struct NdjsonWriter;

impl NdjsonWriter {
    /// Create a new NDJSON writer.
    pub fn new() -> Self {
        Self
    }

    /// A row as the JSON object of one NDJSON line: each column's value by
    /// its name, nulls left out.
    pub fn row_to_object(columns: &[ColumnInfo], row: impl IntoIterator<Item = Value>) -> Value {
        let mut obj = serde_json::Map::new();
        for (col, value) in columns.iter().zip(row) {
            if !value.is_null() {
                obj.insert(col.name.clone(), value);
            }
        }
        Value::Object(obj)
    }
}

impl OutputWriter for NdjsonWriter {
    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        for row in &result.data {
            let obj = Self::row_to_object(&result.columns, row.iter().cloned());

            let line = serde_json::to_string(&obj).map_err(|e| Error::Output(e.to_string()))?;

            writeln!(output, "{}", line).map_err(|e| Error::Output(e.to_string()))?;
        }

        Ok(())
    }
}

/// JSON array output writer.
///
/// Writes the entire result set as a single JSON array.
#[derive(Debug, Clone, Default)]
pub struct JsonArrayWriter;

impl JsonArrayWriter {
    /// Create a new JSON array writer.
    pub fn new() -> Self {
        Self
    }
}

impl OutputWriter for JsonArrayWriter {
    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        let json_array = result.to_json_array();

        let json_str =
            serde_json::to_string(&json_array).map_err(|e| Error::Output(e.to_string()))?;

        write!(output, "{}", json_str).map_err(|e| Error::Output(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnType;
    use serde_json::json;

    #[test]
    fn test_ndjson_writer() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("name", ColumnType::String),
            ],
            data: vec![
                vec![json!("1"), json!("Alice")],
                vec![json!("2"), json!("Bob")],
            ],
        };

        let writer = NdjsonWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.lines().collect();

        assert_eq!(lines.len(), 2);

        let obj1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(obj1["id"], "1");
        assert_eq!(obj1["name"], "Alice");

        let obj2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(obj2["id"], "2");
        assert_eq!(obj2["name"], "Bob");
    }

    #[test]
    fn test_json_array_writer() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("value", ColumnType::Integer),
            ],
            data: vec![vec![json!("1"), json!(100)], vec![json!("2"), json!(200)]],
        };

        let writer = JsonArrayWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output_str).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "1");
        assert_eq!(parsed[0]["value"], 100);
    }

    #[test]
    fn test_empty_result() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![],
        };

        let writer = NdjsonWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        assert!(output.is_empty());

        let json_writer = JsonArrayWriter::new();
        let mut json_output = Vec::new();
        OutputWriter::write(&json_writer, &result, &mut json_output).unwrap();

        let output_str = String::from_utf8(json_output).unwrap();
        assert_eq!(output_str, "[]");
    }

    #[test]
    fn test_null_values_excluded() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("name", ColumnType::String),
            ],
            data: vec![vec![json!("1"), Value::Null]],
        };

        let writer = NdjsonWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let obj: serde_json::Value = serde_json::from_str(output_str.trim()).unwrap();

        // Null values should not be included in the output
        assert_eq!(obj["id"], "1");
        assert!(obj.get("name").is_none());
    }
}
