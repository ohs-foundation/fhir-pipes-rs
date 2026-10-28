//! The `location_hierarchy` table: what the hierarchy walk resolved for every
//! Location, written beside `locations` as a hive-partitioned table. It has
//! the columns of the built-in `LocationHierarchy` SQLView (views/location/),
//! which it stands in for, partitioned by `country` as the views step lays out
//! its tables.

use std::path::Path;

use fhir_pipes_sof::{ColumnInfo, ColumnType, ViewResult};
use serde_json::Value;

use crate::error::{PipesError, Result};
use crate::index::{ADMIN_COLUMNS, Index};
use crate::warehouse::partition;

const PARTITION_COLUMN: &str = "country";
/// Position of `admin0_code` in a row: the country of the Location.
const ADMIN0_CODE: usize = 6 + ADMIN_COLUMNS;

fn columns() -> Vec<ColumnInfo> {
    let mut cols = vec![
        ColumnInfo::new("id", ColumnType::String),
        ColumnInfo::new("name", ColumnType::String),
        ColumnInfo::new("part_of", ColumnType::String),
        ColumnInfo::new("is_admin", ColumnType::Boolean),
        ColumnInfo::new("depth", ColumnType::Integer),
        ColumnInfo::new("admin_level", ColumnType::Integer),
    ];
    for kind in ["name", "code"] {
        for level in 0..ADMIN_COLUMNS {
            cols.push(ColumnInfo::new(
                format!("admin{level}_{kind}"),
                ColumnType::String,
            ));
        }
    }
    cols.push(ColumnInfo::new("path", ColumnType::String));
    cols
}

fn text(s: Option<String>) -> Value {
    s.map_or(Value::Null, Value::String)
}

/// Write one row per Location the walk resolved (with or without geometry)
/// to `out_dir`, replacing what was there. Returns the row count.
pub fn write_hierarchy(index: &Index, out_dir: &Path) -> Result<usize> {
    let records = &index.records;
    let mut data = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let Some(info) = index.hierarchy.get(i) else {
            continue;
        };
        let mut row = vec![
            Value::String(rec.id.clone()),
            text(rec.name.clone()),
            text(rec.part_of.clone()),
            Value::Bool(rec.is_admin),
            Value::from(info.depth),
            info.admin_level.map_or(Value::Null, Value::from),
        ];
        row.extend(
            index
                .hierarchy
                .admin_names(records, i)
                .into_iter()
                .map(text),
        );
        row.extend(
            index
                .hierarchy
                .admin_codes(records, i)
                .into_iter()
                .map(text),
        );
        row.push(Value::String(index.hierarchy.path(records, i)));
        data.push(row);
    }
    let result = ViewResult {
        columns: columns(),
        data,
    };
    let (rows, _) = partition::write(result, out_dir, PARTITION_COLUMN, |row| {
        row[ADMIN0_CODE].as_str().map(str::to_string)
    })
    .map_err(|e| PipesError::Environment(format!("{}: {e:#}", out_dir.display())))?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexRecord;
    use crate::index::hierarchy::resolve_hierarchy;
    use crate::report::Report;
    use arrow::array::{Array, AsArray};
    use std::collections::BTreeMap;

    fn rec(id: &str, parent: Option<&str>, admin: bool, code: Option<&str>) -> IndexRecord {
        IndexRecord {
            id: id.into(),
            part_of: parent.map(str::to_string),
            name: Some(id.to_uppercase()),
            admin_code: code.map(str::to_string),
            is_admin: admin,
            ..Default::default()
        }
    }

    fn index(records: Vec<IndexRecord>) -> Index {
        let hierarchy = resolve_hierarchy(&records, &mut Report::default());
        Index {
            records,
            hierarchy,
            order: Vec::new(),
            report: Report::default(),
            read: 0,
            keep_without_geometry: false,
        }
    }

    /// id -> (column -> value as text, "" for null), across every partition.
    fn read(dir: &Path) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for file in partition::table_files(dir, "country") {
            let reader = partition::open_parquet(&file).unwrap().build().unwrap();
            for batch in reader {
                let batch = batch.unwrap();
                let schema = batch.schema();
                for r in 0..batch.num_rows() {
                    let mut row = BTreeMap::new();
                    for (c, f) in schema.fields().iter().enumerate() {
                        let col = batch.column(c);
                        let v = if col.is_null(r) {
                            String::new()
                        } else {
                            let s = arrow::compute::cast(col, &arrow::datatypes::DataType::Utf8)
                                .unwrap();
                            s.as_string::<i32>().value(r).to_string()
                        };
                        row.insert(f.name().clone(), v);
                    }
                    out.insert(row["id"].clone(), row);
                }
            }
        }
        out
    }

    #[test]
    fn writes_every_resolved_location_including_ones_without_geometry() {
        let idx = index(vec![
            rec("ng", None, true, Some("NG")),
            rec("clinic", Some("ng"), false, None),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("location_hierarchy");
        assert_eq!(write_hierarchy(&idx, &table).unwrap(), 2);
        let rows = read(&table);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows["clinic"]["part_of"], "ng");
        assert_eq!(rows["clinic"]["depth"], "1");
        assert_eq!(rows["ng"]["part_of"], "");
        assert_eq!(rows["ng"]["is_admin"], "true");
    }

    #[test]
    fn admin_level_is_null_for_non_admin_and_counts_through_them() {
        let idx = index(vec![
            rec("c", None, true, Some("C")),
            rec("zone", Some("c"), false, None),
            rec("d", Some("zone"), true, Some("D1")),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        write_hierarchy(&idx, &table).unwrap();
        let rows = read(&table);
        assert_eq!(rows["d"]["admin_level"], "1");
        assert_eq!(rows["zone"]["admin_level"], "");
        assert_eq!(rows["c"]["admin_level"], "0");
        assert_eq!(rows["d"]["admin1_name"], "D");
        assert_eq!(rows["d"]["admin1_code"], "D1");
        assert_eq!(rows["d"]["admin0_name"], "C");
    }

    #[test]
    fn country_comes_from_the_admin0_code_for_every_row() {
        let idx = index(vec![
            rec("ng", None, true, Some("NG")),
            rec("kano", Some("ng"), true, Some("NG001")),
            rec("stray", None, false, None),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        write_hierarchy(&idx, &table).unwrap();
        for p in ["country=NG", "country=__HIVE_DEFAULT_PARTITION__"] {
            assert!(table.join(p).join("part-0.parquet").exists(), "{p}");
        }
        let rows = read(&table);
        assert_eq!(rows["kano"]["country"], "NG");
        assert_eq!(rows["ng"]["country"], "NG");
        assert_eq!(rows["stray"]["country"], "");
    }

    #[test]
    fn an_orphan_keeps_its_dangling_part_of() {
        let idx = index(vec![rec("o", Some("gone"), false, None)]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        write_hierarchy(&idx, &table).unwrap();
        assert_eq!(read(&table)["o"]["part_of"], "gone");
    }

    #[test]
    fn rows_under_a_cycle_are_left_out() {
        let idx = index(vec![
            rec("ng", None, true, Some("NG")),
            rec("a", Some("b"), true, None),
            rec("b", Some("a"), true, None),
            rec("victim", Some("a"), false, None),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        assert_eq!(write_hierarchy(&idx, &table).unwrap(), 1);
        assert_eq!(read(&table).keys().collect::<Vec<_>>(), ["ng"]);
    }

    #[test]
    fn path_matches_locations_parquet() {
        let idx = index(vec![
            rec("ng", None, true, Some("NG")),
            rec("kano", Some("ng"), true, Some("NG001")),
            rec("clinic", Some("kano"), false, None),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        write_hierarchy(&idx, &table).unwrap();
        let rows = read(&table);
        for (i, r) in idx.records.iter().enumerate() {
            assert_eq!(rows[&r.id]["path"], idx.hierarchy.path(&idx.records, i));
        }
        assert_eq!(rows["clinic"]["path"], "/ng/kano/clinic");
    }

    #[test]
    fn the_columns_are_in_the_views_order_with_country_last() {
        let idx = index(vec![rec("ng", None, true, Some("NG"))]);
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        write_hierarchy(&idx, &table).unwrap();
        let file = &partition::table_files(&table, "country")[0];
        let builder = partition::open_parquet(file).unwrap();
        let names: Vec<String> = builder
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        assert_eq!(names.first().map(String::as_str), Some("id"));
        assert_eq!(names.last().map(String::as_str), Some("country"));
        assert_eq!(names.len(), 18);
    }
}
