//! CSV output writer for view results.

use std::io::Write;

use serde_json::Value;

use super::OutputWriter;
use crate::runner::ViewResult;
use crate::{Error, Result};

/// CSV output writer: a header row of column names, then one record per row,
/// comma-separated, with fields quoted where needed.
#[derive(Debug, Clone, Default)]
pub struct CsvWriter;

impl CsvWriter {
    /// Create a new CSV writer.
    pub fn new() -> Self {
        Self
    }
}

/// Convert a JSON value to a CSV-appropriate string.
fn json_value_to_csv_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            // For arrays, join with semicolons
            arr.iter()
                .map(json_value_to_csv_string)
                .collect::<Vec<_>>()
                .join(";")
        }
        Value::Object(_) => {
            // For objects, serialize as JSON
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

impl OutputWriter for CsvWriter {
    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false) // the header is written here
            .from_writer(output);

        let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        writer
            .write_record(&headers)
            .map_err(|e| Error::Output(e.to_string()))?;

        // Write data rows
        for row in &result.data {
            let values: Vec<String> = row.iter().map(json_value_to_csv_string).collect();
            writer
                .write_record(&values)
                .map_err(|e| Error::Output(e.to_string()))?;
        }

        writer.flush().map_err(|e| Error::Output(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::{ColumnInfo, ColumnType};
    use serde_json::json;

    #[test]
    fn test_csv_writer() {
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

        let writer = CsvWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        let csv_str = String::from_utf8(output).unwrap();
        assert!(csv_str.contains("id,name"));
        assert!(csv_str.contains("1,Alice"));
        assert!(csv_str.contains("2,Bob"));
    }

    #[test]
    fn test_json_value_to_csv_string() {
        assert_eq!(json_value_to_csv_string(&Value::Null), "");
        assert_eq!(json_value_to_csv_string(&json!(true)), "true");
        assert_eq!(json_value_to_csv_string(&json!(42)), "42");
        assert_eq!(json_value_to_csv_string(&json!("hello")), "hello");
        assert_eq!(json_value_to_csv_string(&json!(["a", "b", "c"])), "a;b;c");
    }

    #[test]
    fn test_csv_quoting() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("v", ColumnType::String)],
            data: vec![
                vec![json!("hello,world")],
                vec![json!("say \"hi\"")],
                vec![json!("line1\nline2")],
                vec![json!("hello")],
            ],
        };
        let mut output = Vec::new();
        CsvWriter::new().write(&result, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "v\n\"hello,world\"\n\"say \"\"hi\"\"\"\n\"line1\nline2\"\nhello\n"
        );
    }
}
