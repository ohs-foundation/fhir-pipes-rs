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

#[cfg(test)]
mod tests {
    use super::*;

    use fhir_pipes_sof::{ColumnInfo, ColumnType, ViewResult};

    /// Settings for a warehouse in `dir`, with `extra` added to the config.
    fn settings(dir: &Path, extra: &str) -> Settings {
        let file = toml::from_str(&format!(
            "data = 'data'\n[server]\nbase = 'https://fhir.example/fhir?key=k'\n{extra}"
        ))
        .unwrap();
        super::super::config::settings(file, dir.to_path_buf(), Default::default()).unwrap()
    }

    /// Write `n` rows to `table_dir`, partitioned by `column`: the first
    /// half in SLE, the rest in NGA.
    fn table(table_dir: &Path, column: &str, n: usize) {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: (0..n).map(|i| vec![json!(format!("r{i}"))]).collect(),
        };
        super::super::partition::write(result, table_dir, column, |row| {
            let i: usize = row[0].as_str()?[1..].parse().ok()?;
            Some(if i < n / 2 { "SLE" } else { "NGA" }.to_string())
        })
        .unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_manifest_counts_every_table_and_lists_the_staged_views() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path(), "");
        let stage = Stage::new(&s).unwrap();
        // A live table the run did not write, and one it did.
        table(&s.parquet_dir().join("old"), "country", 3);
        table(&stage.table("obs"), "country", 5);
        let views = stage.dir("views");
        fs::create_dir_all(&views).unwrap();
        fs::write(
            views.join("obs.json"),
            r#"{"name": "obs", "resource": "Observation", "status": "active", "url": "u"}"#,
        )
        .unwrap();
        fs::write(views.join("notes.txt"), "not a view").unwrap();
        // A Location state written by kiln.
        let loc = s.raw_dir().join("locations");
        fs::create_dir_all(&loc).unwrap();
        fs::write(
            loc.join("state.json"),
            r#"{"watermark": "2026-01-01T00:00:00Z", "count": 7, "kiln_version": "0.9"}"#,
        )
        .unwrap();

        let at = write(&s, &stage, Some("https://fhir.example/fhir")).unwrap();
        let m = read(&stage.file("manifest.json"));
        assert_eq!(m["refreshed_at"], at);
        assert_eq!(m["server"], "https://fhir.example/fhir");
        assert_eq!(m["server_name"], "hapi-local");
        assert_eq!(m["tables"], json!({"obs": {"rows": 5}, "old": {"rows": 3}}));
        assert_eq!(
            m["views"],
            json!({"obs": {"url": "u", "resource": "Observation", "status": "active"}})
        );
        assert_eq!(
            m["extract"],
            json!({
                "watermark": "2026-01-01T00:00:00Z",
                "count": 7,
                "organization_count": null,
                "completed_at": null,
                "fhir_pipes_version": "0.9",
            })
        );
        assert!(m.get("files").is_none());
        // Nothing is published yet.
        assert!(!s.data.join("manifest.json").exists());
    }

    #[test]
    fn a_file_source_lists_its_inputs_and_records_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path(), "[source]\nfiles = ['in/a.ndjson']\n");
        let stage = Stage::new(&s).unwrap();
        write(&s, &stage, recorded_server(&s, true).as_deref()).unwrap();
        let m = read(&stage.file("manifest.json"));
        assert_eq!(m["server"], Value::Null);
        assert_eq!(m["server_name"], "files");
        assert_eq!(m["files"], json!(s.inputs()));
        assert_eq!(m["tables"], json!({}));
        assert_eq!(m["extract"]["fhir_pipes_version"], Value::Null);
    }

    #[test]
    fn the_recorded_server_is_the_one_contacted_else_the_one_in_raw() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path(), "");
        // The query is not published, nor credentials in a state file.
        assert_eq!(
            recorded_server(&s, true).as_deref(),
            Some("https://fhir.example/fhir")
        );
        assert_eq!(recorded_server(&s, false), None);

        let raw = s.raw_dir();
        let loc = raw.join("locations");
        fs::create_dir_all(&loc).unwrap();
        let snapshot_state = loc.join(crate::snapshot::STATE_FILE);
        fs::write(
            &snapshot_state,
            r#"{"server": "https://a@snap.example/fhir"}"#,
        )
        .unwrap();
        assert_eq!(
            recorded_server(&s, false).as_deref(),
            Some("https://snap.example/fhir")
        );
        // The export state comes first, unless it names the file source.
        let export_state = raw.join(crate::export::STATE_FILE);
        fs::write(
            &export_state,
            r#"{"server": "https://export.example/fhir"}"#,
        )
        .unwrap();
        assert_eq!(
            recorded_server(&s, false).as_deref(),
            Some("https://export.example/fhir")
        );
        fs::write(&export_state, r#"{"server": "files"}"#).unwrap();
        assert_eq!(
            recorded_server(&s, false).as_deref(),
            Some("https://snap.example/fhir")
        );
        fs::write(&snapshot_state, r#"{"server": ""}"#).unwrap();
        assert_eq!(recorded_server(&s, false), None);
    }

    #[test]
    fn rows_are_counted_under_the_tables_own_partition_column() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("locations");
        table(&t, "country", 4);
        // Beside the partitions, not part of the table.
        fs::write(t.join("items.parquet"), "not read").unwrap();
        assert_eq!(rows(&t, "country").unwrap(), 4);
        assert_eq!(rows(&t, "district").unwrap(), 4);
        assert_eq!(rows(&dir.path().join("missing"), "country").unwrap(), 0);
    }
}
