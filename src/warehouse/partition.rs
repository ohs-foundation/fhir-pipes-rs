//! Writing a view's rows as a hive-partitioned table: parquet/<table>/<col>=<value>/part-0.parquet,
//! with the partition value looked up through another warehouse table.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;
use fhir_pipes_sof::output::{OutputWriter, ParquetWriter, TemporalEncoding, WriteSummary};
use fhir_pipes_sof::{ColumnInfo, ColumnType, ViewResult};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::Value;

use super::config::Lookup;

/// The directory name hive (and DuckDB) give the null partition.
pub const NULL_PARTITION: &str = "__HIVE_DEFAULT_PARTITION__";

/// The column a table is partitioned by at its first level, from its directory
/// names: `country` for `country=SLE/`. Each table is read by its own layout,
/// since they can differ: the views step partitions by `[partition] column`,
/// the Location transform always by `country`. `default` when the table has
/// no `<column>=` directory (yet).
pub fn partition_key(table_dir: &Path, default: &str) -> String {
    let Ok(entries) = fs::read_dir(table_dir) else {
        return default.to_string();
    };
    let mut keys: Vec<String> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let (key, _) = name.split_once('=')?;
            (!key.is_empty() && !key.starts_with('.')).then(|| key.to_string())
        })
        .collect();
    keys.sort();
    keys.dedup();
    if keys.iter().any(|k| k == default) {
        return default.to_string();
    }
    keys.into_iter()
        .next()
        .unwrap_or_else(|| default.to_string())
}

/// The parquet files of a partitioned table: those under a `<column>=` directory,
/// which leaves out metadata kept beside the data (e.g. a catalog's items.parquet).
/// A missing or unreadable directory counts as empty.
pub fn table_files(table_dir: &Path, column: &str) -> Vec<PathBuf> {
    list_table_files(table_dir, column, |dir| {
        Ok(crate::fs::sorted_entries(dir).unwrap_or_default())
    })
    .unwrap_or_default()
}

/// As `table_files`, but a missing or unreadable directory is an error, not
/// an empty table.
#[cfg(feature = "postgres")]
pub fn table_files_checked(table_dir: &Path, column: &str) -> Result<Vec<PathBuf>> {
    list_table_files(table_dir, column, |dir| {
        crate::fs::sorted_entries_strict(dir).with_context(|| format!("reading {}", dir.display()))
    })
}

type Entries = fn(&Path) -> Result<Vec<PathBuf>>;

fn list_table_files(table_dir: &Path, column: &str, entries: Entries) -> Result<Vec<PathBuf>> {
    let prefix = format!("{column}=");
    let mut files = Vec::new();
    for part in entries(table_dir)? {
        if part.is_dir()
            && part
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        {
            collect_parquet(&part, entries, &mut files)?;
        }
    }
    Ok(files)
}

fn collect_parquet(dir: &Path, entries: Entries, out: &mut Vec<PathBuf>) -> Result<()> {
    for p in entries(dir)? {
        if p.is_dir() {
            collect_parquet(&p, entries, out)?;
        } else if p.extension().is_some_and(|x| x == "parquet") {
            out.push(p);
        }
    }
    Ok(())
}

/// key → partition value, read from a warehouse table. Where a key has several
/// values the first one read wins. The value is the table's own first-level
/// partition (`partition_key`, `column` by default), which need not be named
/// `column`: Locations are partitioned by `country` whatever the views use. A
/// file without that column takes the value from its `<key>=<value>`
/// directory.
pub fn read_lookup(table_dir: &Path, key: &str, column: &str) -> Result<HashMap<String, String>> {
    let column = &partition_key(table_dir, column);
    let mut map = HashMap::new();
    for file in table_files(table_dir, column) {
        let from_path = partition_value_in_path(&file, table_dir, column);
        let f = fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(f)
            .with_context(|| format!("reading {}", file.display()))?;
        let has = |c: &str| builder.schema().column_with_name(c).is_some();
        if !has(key) {
            continue;
        }
        let wanted: Vec<&str> = if has(column) {
            vec![key, column]
        } else {
            vec![key]
        };
        let mask = ProjectionMask::columns(builder.parquet_schema(), wanted.iter().copied());
        for batch in builder.with_projection(mask).build()? {
            let batch = batch?;
            let keys = strings(batch.column_by_name(key).expect("projected"))?;
            let values = match batch.column_by_name(column) {
                Some(c) => strings(c)?,
                None => vec![from_path.clone(); keys.len()],
            };
            for (k, v) in keys.into_iter().zip(values) {
                if let (Some(k), Some(v)) = (k, v) {
                    map.entry(k).or_insert(v);
                }
            }
        }
    }
    Ok(map)
}

fn partition_value_in_path(file: &Path, table_dir: &Path, column: &str) -> Option<String> {
    let prefix = format!("{column}=");
    file.strip_prefix(table_dir)
        .ok()?
        .components()
        .find_map(|c| {
            c.as_os_str()
                .to_string_lossy()
                .strip_prefix(&prefix)
                .map(str::to_string)
        })
        .filter(|v| v != NULL_PARTITION)
        .map(|v| unescape(&v))
}

fn strings(col: &dyn Array) -> Result<Vec<Option<String>>> {
    let col = arrow::compute::cast(col, &DataType::Utf8)?;
    let col = col.as_string::<i32>();
    Ok((0..col.len())
        .map(|i| col.is_valid(i).then(|| col.value(i).to_string()))
        .collect())
}

/// The lookup rule a view's rows use: the first whose key column the view has.
pub fn rule_for<'a>(result: &ViewResult, lookups: &'a [Lookup]) -> Option<(&'a Lookup, usize)> {
    lookups.iter().find_map(|l| {
        result
            .columns
            .iter()
            .position(|c| c.name == l.key)
            .map(|i| (l, i))
    })
}

/// Split `result` by partition value and write it to `table_dir`, replacing
/// what was there only once every file is written. Returns the row count and
/// the values that did not match their column's type.
pub fn write(
    result: ViewResult,
    table_dir: &Path,
    column: &str,
    partition_of: impl Fn(&[Value]) -> Option<String>,
) -> Result<(usize, WriteSummary)> {
    let total = result.data.len();
    let mut groups: BTreeMap<Option<String>, Vec<Vec<Value>>> = BTreeMap::new();
    for row in result.data {
        groups.entry(partition_of(&row)).or_default().push(row);
    }
    // No rows: one empty file in the null partition, so the table still has
    // its columns for whatever reads it (the catalog, the database step).
    if groups.is_empty() {
        groups.insert(None, Vec::new());
    }
    let parent = table_dir
        .parent()
        .context("table directory has no parent")?;
    fs::create_dir_all(parent)?;
    let name = table_dir
        .file_name()
        .context("table directory has no name")?
        .to_string_lossy();
    let staging = parent.join(format!(".{name}.new"));
    let _ = fs::remove_dir_all(&staging);
    let writer = ParquetWriter::new().with_temporal(TemporalEncoding::Native);
    let mut columns = result.columns;
    columns.push(ColumnInfo::new(column, ColumnType::String));
    let mut summary = WriteSummary::default();
    let mut segments = Segments::default();
    for (value, rows) in groups {
        let segment = match value.as_deref() {
            None => NULL_PARTITION.to_string(),
            Some(v) => segments.claim(v)?,
        };
        let dir = staging.join(format!("{column}={segment}"));
        fs::create_dir_all(&dir)?;
        let data: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|mut r| {
                r.push(value.clone().map_or(Value::Null, Value::String));
                r
            })
            .collect();
        let part = ViewResult {
            columns: columns.clone(),
            data,
        };
        let path = dir.join("part-0.parquet");
        let mut file =
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        summary.merge(
            writer
                .write_with_summary(&part, &mut file)
                .with_context(|| format!("writing {}", path.display()))?,
        );
        // On disk before the rename that publishes it, so a crash after the
        // rename cannot leave an empty or cut-short file in the table.
        file.sync_all()
            .with_context(|| format!("writing {}", path.display()))?;
    }
    fs::create_dir_all(&staging)?;
    replace(&staging, table_dir)?;
    Ok((total, summary))
}

/// Put the table at `staged` in place of `table_dir`: the old one is renamed
/// to `.<table>.old`, the new one renamed in, then the old one removed.
pub fn replace(staged: &Path, table_dir: &Path) -> Result<()> {
    let parent = table_dir
        .parent()
        .context("table directory has no parent")?;
    let name = table_dir
        .file_name()
        .context("table directory has no name")?
        .to_string_lossy();
    let old = parent.join(format!(".{name}.old"));
    let _ = fs::remove_dir_all(&old);
    if table_dir.exists() {
        fs::rename(table_dir, &old)
            .with_context(|| format!("moving {} aside", table_dir.display()))?;
    }
    fs::rename(staged, table_dir)
        .with_context(|| format!("renaming {} into place", staged.display()))?;
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// The directory names given to one table's partition values. A
/// case-insensitive disk (default macOS, Windows) holds `SLE` and `sle` in
/// one directory, where one file would replace the other; the second value
/// gets every letter escaped (`%73%6C%65`), which DuckDB reads back as `sle`.
#[derive(Default)]
struct Segments(HashMap<String, String>);

impl Segments {
    fn claim(&mut self, value: &str) -> Result<String> {
        for segment in [escape(value, false), escape(value, true)] {
            match self.0.get(&segment.to_lowercase()) {
                Some(owner) if owner != value => continue,
                _ => {
                    self.0.insert(segment.to_lowercase(), value.to_string());
                    return Ok(segment);
                }
            }
        }
        anyhow::bail!("partition value {value:?} has no directory name of its own")
    }
}

/// A partition value as one path segment, in the `%XX` escapes hive uses and
/// DuckDB decodes when it reads the value from the directory name. Escaped:
/// what a path or a Windows file name cannot hold, control characters, `%`
/// itself, a trailing dot or space (Windows drops them), the first letter of
/// a Windows device name (`CON`), and with `letters` every ASCII letter.
fn escape(v: &str, letters: bool) -> String {
    let base = v.split('.').next().unwrap_or(v);
    let device = crate::index::partition::RESERVED_WINDOWS_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(base));
    let trailing = v.len() - v.trim_end_matches(['.', ' ']).len();
    let mut out = String::with_capacity(v.len());
    for (i, c) in v.char_indices() {
        let escaped = matches!(
            c,
            '/' | '\\' | '=' | '%' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || c.is_control()
            || i >= v.len() - trailing
            || (device && i == 0)
            || (letters && c.is_ascii_alphabetic());
        if escaped {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A directory name's partition value: `escape` undone.
fn unescape(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tables_partition_key_comes_from_its_directories() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        // No directories yet: the configured column.
        assert_eq!(partition_key(&t.join("missing"), "part"), "part");
        fs::create_dir_all(t.join("country=NG/geom_type=point")).unwrap();
        fs::create_dir_all(t.join(".staging")).unwrap();
        assert_eq!(partition_key(t, "part"), "country");
        // The configured column wins where both are there.
        fs::create_dir_all(t.join("part=x")).unwrap();
        assert_eq!(partition_key(t, "part"), "part");
    }
    use serde_json::json;

    fn result(cols: &[&str], rows: Vec<Vec<Value>>) -> ViewResult {
        ViewResult {
            columns: cols
                .iter()
                .map(|c| ColumnInfo::new(*c, ColumnType::String))
                .collect(),
            data: rows,
        }
    }

    #[test]
    fn writes_partitions_and_reads_them_back_as_a_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("places");
        let r = result(
            &["id", "where"],
            vec![
                vec![json!("a"), json!("SLE")],
                vec![json!("b"), json!("NGA")],
                vec![json!("c"), Value::Null],
            ],
        );
        let (n, summary) = write(r, &table, "country", |row| {
            row[1].as_str().map(str::to_string)
        })
        .unwrap();
        assert!(summary.is_empty());
        assert_eq!(n, 3);
        for p in [
            "country=SLE",
            "country=NGA",
            "country=__HIVE_DEFAULT_PARTITION__",
        ] {
            assert!(table.join(p).join("part-0.parquet").exists(), "{p}");
        }
        let lookup = read_lookup(&table, "id", "country").unwrap();
        assert_eq!(lookup.get("a").map(String::as_str), Some("SLE"));
        assert_eq!(lookup.get("b").map(String::as_str), Some("NGA"));
        assert!(!lookup.contains_key("c"));
        assert!(!dir.path().join(".places.new").exists());
    }

    #[test]
    fn the_summary_adds_up_every_partition() {
        let dir = tempfile::tempdir().unwrap();
        let r = ViewResult {
            columns: vec![
                ColumnInfo::new("where", ColumnType::String),
                ColumnInfo::new("start", ColumnType::Date),
            ],
            data: vec![
                vec![json!("SLE"), json!("2026-06-15T08:30:00+01:00")],
                vec![json!("NGA"), json!("2026-06-15T08:30:00+01:00")],
                vec![json!("NGA"), json!("June")],
            ],
        };
        let (_, summary) = write(r, &dir.path().join("t"), "country", |row| {
            row[0].as_str().map(str::to_string)
        })
        .unwrap();
        assert_eq!(
            summary.messages(),
            [
                "start: 2 dateTime values written as their date",
                "start: 1 value did not fit date and was written as null",
            ]
        );
    }

    #[test]
    fn values_that_differ_only_in_case_keep_all_their_rows() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        let r = result(
            &["id", "where"],
            vec![
                vec![json!("a"), json!("Kano")],
                vec![json!("b"), json!("KANO")],
            ],
        );
        write(r, &table, "region", |row| {
            row[1].as_str().map(str::to_string)
        })
        .unwrap();
        // Each value is in a file of its own, on any disk.
        assert_eq!(table_files(&table, "region").len(), 2);
        let lookup = read_lookup(&table, "id", "region").unwrap();
        assert_eq!(lookup.get("a").map(String::as_str), Some("Kano"));
        assert_eq!(lookup.get("b").map(String::as_str), Some("KANO"));
    }

    #[test]
    fn rewriting_replaces_old_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("t");
        let r = result(&["id"], vec![vec![json!("a")]]);
        write(r.clone(), &table, "country", |_| Some("NGA".into())).unwrap();
        write(r, &table, "country", |_| Some("SLE".into())).unwrap();
        assert!(!table.join("country=NGA").exists());
        assert!(table.join("country=SLE").exists());
    }

    #[test]
    fn picks_the_first_rule_the_view_has() {
        let lookups = vec![
            Lookup {
                key: "location_id".into(),
                table: "locations".into(),
                table_key: Some("id".into()),
            },
            Lookup {
                key: "campaign_id".into(),
                table: "campaign_calendar".into(),
                table_key: None,
            },
        ];
        let both = result(&["campaign_id", "location_id"], vec![]);
        assert_eq!(
            rule_for(&both, &lookups).map(|(l, i)| (l.key.as_str(), i)),
            Some(("location_id", 1))
        );
        let campaign = result(&["campaign_id"], vec![]);
        assert_eq!(
            rule_for(&campaign, &lookups).map(|(l, _)| l.table.as_str()),
            Some("campaign_calendar")
        );
        assert!(rule_for(&result(&["x"], vec![]), &lookups).is_none());
    }

    #[test]
    fn values_are_escaped_as_path_segments() {
        assert_eq!(escape("SLE", false), "SLE");
        assert_eq!(escape("a/b=c", false), "a%2Fb%3Dc");
        assert_eq!(escape("a*b?", false), "a%2Ab%3F");
        assert_eq!(escape("x. ", false), "x%2E%20");
        assert_eq!(escape("con.txt", false), "%63on.txt");
        assert_eq!(escape("é\n", false), "é%0A");
        assert_eq!(escape("Ab1", true), "%41%621");
        for v in ["SLE", "a/b=c", "a*b?", "x. ", "con.txt", "é\n", "50%"] {
            assert_eq!(unescape(&escape(v, false)), v);
            assert_eq!(unescape(&escape(v, true)), v);
        }
    }

    #[test]
    fn values_that_differ_only_in_case_get_directories_of_their_own() {
        let mut s = Segments::default();
        let got: Vec<String> = ["SLE", "Sle", "sle", "SLE"]
            .iter()
            .map(|v| s.claim(v).unwrap())
            .collect();
        assert_eq!(got, ["SLE", "%53%6C%65", "%73%6C%65", "SLE"]);
        let lower: std::collections::HashSet<String> =
            got.iter().map(|g| g.to_lowercase()).collect();
        assert_eq!(lower.len(), 3);
    }
}
