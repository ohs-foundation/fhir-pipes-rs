//! manifest.json: when and from where the warehouse was refreshed, each table's
//! row count, the views, and kiln's extract state.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use parquet::file::reader::{FileReader, SerializedFileReader};
use serde_json::{Map, Value, json};

use crate::config::Settings;
use crate::partition::table_files;

/// Write data/manifest.json and return its `refreshed_at`.
pub fn write(s: &Settings) -> Result<String> {
    println!("== manifest");
    let refreshed_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string();

    let parquet = s.parquet_dir();
    let mut names: Vec<String> = fs::read_dir(&parquet)
        .with_context(|| format!("reading {}", parquet.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // Dot-directories hold catalog state and staging, not tables.
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let mut tables = Map::new();
    for name in &names {
        tables.insert(
            name.clone(),
            json!({"rows": rows(&parquet.join(name), &s.partition_column)?}),
        );
    }

    let mut views = Map::new();
    let mut files: Vec<_> = fs::read_dir(s.views_out_dir())
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.retain(|p: &std::path::PathBuf| p.extension().is_some_and(|x| x == "json"));
    files.sort();
    for f in files {
        let v: Value = serde_json::from_str(&fs::read_to_string(&f)?)
            .with_context(|| format!("parsing {}", f.display()))?;
        if let Some(name) = v.get("name").and_then(Value::as_str) {
            views.insert(
                name.to_string(),
                json!({"url": v.get("url"), "resource": v.get("resource"), "status": v.get("status")}),
            );
        }
    }

    let state: Value = fs::read_to_string(s.raw_dir().join("locations/state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let kiln: Map<String, Value> = [
        "kiln_version",
        "watermark",
        "count",
        "organization_count",
        "completed_at",
    ]
    .iter()
    .map(|k| (k.to_string(), state.get(k).cloned().unwrap_or(Value::Null)))
    .collect();

    let manifest = json!({
        "refreshed_at": refreshed_at,
        "server": s.server,
        "server_name": s.server_name,
        "tables": tables,
        "views": views,
        "kiln": kiln,
    });
    let path = s.data.join("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("   {}", serde_json::to_string(&manifest["tables"])?);
    Ok(refreshed_at)
}

/// A partitioned table's row count, from the parquet footers.
pub fn rows(table_dir: &Path, column: &str) -> Result<i64> {
    let mut n = 0;
    for file in table_files(table_dir, column) {
        let f = fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
        n += SerializedFileReader::new(f)
            .with_context(|| format!("reading {}", file.display()))?
            .metadata()
            .file_metadata()
            .num_rows();
    }
    Ok(n)
}
