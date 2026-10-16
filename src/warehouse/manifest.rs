//! manifest.json: when and from where the warehouse was refreshed, each table's
//! row count, the views, and the Location extract's state.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use parquet::file::reader::{FileReader, SerializedFileReader};
use serde_json::{Map, Value, json};

use super::config::Settings;
use super::partition::{partition_key, table_files};
use super::stage::Stage;

/// Write manifest.json into the staging directory, describing the tables and
/// views as they will be once the run is published, and return its
/// `refreshed_at`. `server` is where the data came from, if known
/// (`recorded_server`).
pub fn write(s: &Settings, stage: &Stage, server: Option<&str>) -> Result<String> {
    println!("== manifest");
    let refreshed_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string();

    let mut tables = Map::new();
    for (name, dir) in stage.tables()? {
        tables.insert(name, json!({"rows": rows(&dir, &s.partition_column)?}));
    }

    let mut views = Map::new();
    let mut files: Vec<_> = fs::read_dir(stage.read_dir("views"))
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
    // A state.json written by kiln has kiln_version instead.
    let version = state
        .get("fhir_pipes_version")
        .or_else(|| state.get("kiln_version"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut extract: Map<String, Value> =
        ["watermark", "count", "organization_count", "completed_at"]
            .iter()
            .map(|k| (k.to_string(), state.get(k).cloned().unwrap_or(Value::Null)))
            .collect();
    extract.insert("fhir_pipes_version".into(), version);

    let mut manifest = json!({
        "refreshed_at": refreshed_at,
        "server": server,
        "server_name": s.server_name,
        "tables": tables,
        "views": views,
        "extract": extract,
    });
    if s.reads_files() {
        manifest["files"] = json!(s.inputs());
    }
    let path = stage.file("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("   {}", serde_json::to_string(&manifest["tables"])?);
    Ok(refreshed_at)
}

/// The server to record in the manifest and last-refresh.json: the configured
/// one if the run `contacted` it; else, as when `--reuse-raw` downloads
/// nothing, the server named in raw/'s export state, else in the Location
/// snapshot's state; else none. None with a file source.
pub fn recorded_server(s: &Settings, contacted: bool) -> Option<String> {
    if s.reads_files() {
        return None;
    }
    if contacted {
        return Some(s.server.clone());
    }
    let server_in = |path: &Path| {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("server")?.as_str().map(str::to_string))
            .filter(|v| !v.is_empty() && v != super::files::FILES)
    };
    let raw = s.raw_dir();
    server_in(&raw.join(crate::export::STATE_FILE))
        .or_else(|| server_in(&raw.join("locations").join(crate::snapshot::STATE_FILE)))
}

/// A partitioned table's row count, from the parquet footers. `column` is the
/// partition column the table has unless its directories name another.
pub fn rows(table_dir: &Path, column: &str) -> Result<i64> {
    let mut n = 0;
    for file in table_files(table_dir, &partition_key(table_dir, column)) {
        let f = fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
        n += SerializedFileReader::new(f)
            .with_context(|| format!("reading {}", file.display()))?
            .metadata()
            .file_metadata()
            .num_rows();
    }
    Ok(n)
}
