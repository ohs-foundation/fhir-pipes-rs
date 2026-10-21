//! `fhir-pipes inspect`: describe every parquet file under OUT.

use std::path::{Path, PathBuf};

use parquet::file::reader::{FileReader, SerializedFileReader};

use crate::cli::InspectArgs;
use crate::error::{PipesError, Result};

#[derive(Debug)]
pub struct PartitionSummary {
    pub path: String,
    pub rows: i64,
    pub row_groups: usize,
    pub min_row_group_rows: i64,
    pub avg_row_group_rows: i64,
    pub max_row_group_rows: i64,
    pub size_bytes: u64,
    pub geometry_types: Vec<String>,
    pub geo_version: Option<String>,
    pub has_covering: bool,
}

#[derive(Debug)]
pub struct Summary {
    pub partitions: Vec<PartitionSummary>,
    pub total_rows: i64,
    pub total_size_bytes: u64,
}

fn parquet_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| PipesError::io(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    // `PathBuf`'s `Ord` compares component-by-component, not the raw string,
    // so this already orders `part-2` before `part-10`; do not "simplify" it
    // to a string sort.
    entries.sort();
    for path in entries {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| PipesError::io(&path, e))?;
        if meta.file_type().is_symlink() {
            // Never follow a symlinked directory (e.g. a stray link left
            // behind by a crashed swap) into someone else's files.
            continue;
        }
        if meta.is_dir() {
            parquet_files(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "parquet") {
            out.push(path);
        }
    }
    Ok(())
}

/// Returns `(min, avg, max)` row-group sizes. `avg` rounds half-away-from-
/// zero (e.g. 2.5 rows/group -> 3); the Python implementation used `round()`,
/// which rounds half-to-even, so the two can disagree on exact ties.
fn group_stats(groups: &[i64], rows: i64) -> (i64, i64, i64) {
    if groups.is_empty() {
        return (0, 0, 0);
    }
    let min = groups.iter().copied().min().unwrap();
    let max = groups.iter().copied().max().unwrap();
    let avg = (rows as f64 / groups.len() as f64).round() as i64;
    (min, avg, max)
}

pub fn summarize(out_dir: &Path) -> Result<Summary> {
    if !out_dir.is_dir() {
        return Err(PipesError::Usage(format!(
            "{}: no such directory",
            out_dir.display()
        )));
    }
    let dataset_dir = out_dir.join(crate::write::dataset::DATASET_DIR);
    let mut files = Vec::new();
    parquet_files(&dataset_dir, &mut files)?;
    let mut partitions = Vec::new();
    let mut total_rows = 0i64;
    let mut total_size_bytes = 0u64;
    for path in files {
        let file = std::fs::File::open(&path).map_err(|e| PipesError::io(&path, e))?;
        let size_bytes = file.metadata().map_err(|e| PipesError::io(&path, e))?.len();
        let reader =
            SerializedFileReader::new(file).map_err(|e| PipesError::parquet_at(&path, e))?;
        let meta = reader.metadata();
        let rows = meta.file_metadata().num_rows();
        let groups: Vec<i64> = meta.row_groups().iter().map(|rg| rg.num_rows()).collect();
        let geo: serde_json::Value = meta
            .file_metadata()
            .key_value_metadata()
            .and_then(|kvs| kvs.iter().find(|kv| kv.key == "geo"))
            .and_then(|kv| kv.value.as_deref())
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or(serde_json::Value::Null);
        let primary = geo["primary_column"]
            .as_str()
            .unwrap_or("geometry")
            .to_string();
        let column = &geo["columns"][&primary];
        total_rows += rows;
        total_size_bytes += size_bytes;
        let (min_row_group_rows, avg_row_group_rows, max_row_group_rows) =
            group_stats(&groups, rows);
        partitions.push(PartitionSummary {
            path: path
                .strip_prefix(out_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"),
            rows,
            row_groups: groups.len(),
            min_row_group_rows,
            avg_row_group_rows,
            max_row_group_rows,
            size_bytes,
            geometry_types: column["geometry_types"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            geo_version: geo["version"].as_str().map(str::to_string),
            has_covering: column.get("covering").is_some(),
        });
    }
    Ok(Summary {
        partitions,
        total_rows,
        total_size_bytes,
    })
}

fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1}GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1}MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1}KB", b / 1e3)
    } else {
        format!("{bytes}B")
    }
}

pub fn format_summary(s: &Summary) -> String {
    if s.partitions.is_empty() {
        return "No parquet files found.".to_string();
    }
    let mut lines = vec![
        format!(
            "{} rows across {} partitions ({})",
            s.total_rows,
            s.partitions.len(),
            human_size(s.total_size_bytes)
        ),
        String::new(),
    ];
    for p in &s.partitions {
        // `covering` and `geo_version` render with Rust's default `bool`/
        // `Option` formatting ("true"/"false", "none") rather than mimicking
        // Python's "True"/"None" — this is a Rust CLI, not a port of its
        // output byte-for-byte.
        lines.push(format!(
            "  {}\n    rows={} row_groups={} (min={} avg={} max={}) size={}\n    geo={} covering={} types={}",
            p.path,
            p.rows,
            p.row_groups,
            p.min_row_group_rows,
            p.avg_row_group_rows,
            p.max_row_group_rows,
            human_size(p.size_bytes),
            p.geo_version.as_deref().unwrap_or("none"),
            p.has_covering,
            p.geometry_types.join(",")
        ));
    }
    lines.join("\n")
}

pub fn run_inspect(args: &InspectArgs) -> anyhow::Result<()> {
    let summary = summarize(&args.out)?;
    println!("{}", format_summary(&summary));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_bytes_kb_mb_gb() {
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(1500), "1.5KB");
        assert_eq!(human_size(2_500_000), "2.5MB");
        assert_eq!(human_size(3_000_000_000), "3.0GB");
    }

    #[test]
    fn group_stats_rounds_uneven_groups_half_away_from_zero() {
        // 5 rows over 2 groups of [3, 2] averages 2.5, which rounds up to 3
        // (Rust's `f64::round` rounds half away from zero).
        assert_eq!(group_stats(&[3, 2], 5), (2, 3, 3));
    }

    #[test]
    fn group_stats_of_no_groups_is_all_zero() {
        assert_eq!(group_stats(&[], 0), (0, 0, 0));
    }

    #[test]
    fn format_summary_of_empty_summary_says_so() {
        let summary = Summary {
            partitions: Vec::new(),
            total_rows: 0,
            total_size_bytes: 0,
        };
        assert_eq!(format_summary(&summary), "No parquet files found.");
    }
}
