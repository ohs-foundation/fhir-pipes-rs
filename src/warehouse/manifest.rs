//! manifest.json: when and from where the warehouse was refreshed, each table's
//! row count, the views, and the Location extract's state.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::config::Settings;
use super::partition::{open_parquet, partition_key, table_files};
use super::stage::Stage;

/// The time now as `refreshed_at` gives it: UTC, to the second.
pub fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// Write manifest.json into the staging directory, describing the tables and
/// views as they will be once the run is published, and return its
/// `refreshed_at`. `server` is where the data came from, if known
/// (`recorded_server`).
pub fn write(s: &Settings, stage: &Stage, server: Option<&str>) -> Result<String> {
    println!("== manifest");
    let refreshed_at = now();

    let mut tables = Map::new();
    for (name, dir) in stage.tables()? {
        tables.insert(name, json!({"rows": rows(&dir, &s.partition_column)?}));
    }

    let mut views = Map::new();
    let mut files = crate::fs::sorted_entries(&stage.read_dir("views")).unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|x| x == "json"));
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
    crate::fs::atomic_write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("   {}", serde_json::to_string(&manifest["tables"])?);
    Ok(refreshed_at)
}

/// The server to record in the manifest and last-refresh.json: the configured
/// one if the run `contacted` it; else, as when `--reuse-raw` downloads
/// nothing, the server named in raw/'s export state, else in the Location
/// snapshot's state; else none. None with a file source. Without a user
/// name, password or query, since the manifest is published.
pub fn recorded_server(s: &Settings, contacted: bool) -> Option<String> {
    configured_server(s, contacted).map(|u| crate::redact::url(&u))
}

fn configured_server(s: &Settings, contacted: bool) -> Option<String> {
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
        n += open_parquet(&file)?.metadata().file_metadata().num_rows();
    }
    Ok(n)
}
