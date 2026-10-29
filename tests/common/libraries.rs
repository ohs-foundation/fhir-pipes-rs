//! SQL on FHIR Libraries for the tests that run them with duckdb: a
//! Library, the views they read, and duckdb itself.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::fixtures;

pub const SQLVIEW: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";
pub const SQLQUERY: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";
pub const READING: &str = "https://example.org/ViewDefinition/IcrReading";

pub fn have_duckdb(skipped: &str) -> bool {
    let found = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
}

/// One value from a duckdb query, run in `dir`.
pub fn duckdb_value(dir: &Path, sql: &str) -> String {
    let out = std::process::Command::new("duckdb")
        .args(["-noheader", "-list", "-c", sql])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A SQLView (`view`) or SQLQuery Library named `name`; `deps` are (label,
/// canonical) pairs.
pub fn library(name: &str, view: bool, deps: &[(&str, &str)], sql: &str) -> Value {
    let related: Vec<Value> = deps
        .iter()
        .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
        .collect();
    json!({
        "resourceType": "Library",
        "meta": {"profile": [if view { SQLVIEW } else { SQLQUERY }]},
        "url": format!("https://example.org/Library/{name}"),
        "name": name,
        "status": "draft",
        "relatedArtifact": related,
        "content": [{"contentType": "application/sql", "data": STANDARD.encode(sql)}],
    })
}

/// The fixture views (without urls, so no Library can name them) and
/// IcrReading, which has one: Observations with their location and value.
pub fn views(root: &Path) -> PathBuf {
    let dir = root.join("my-views");
    fs::create_dir_all(&dir).unwrap();
    for f in fs::read_dir(fixtures().join("views")).unwrap() {
        let f = f.unwrap().path();
        fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
    }
    let reading = json!({"resourceType": "ViewDefinition", "url": READING, "name": "IcrReading",
    "status": "active", "resource": "Observation", "select": [{"column": [
        {"name": "id", "path": "getResourceKey()", "type": "id"},
        {"name": "location_id", "path": "subject.getReferenceKey(Location)", "type": "string"},
        {"name": "value", "path": "value.ofType(integer)", "type": "integer"},
    ]}]});
    fs::write(dir.join("IcrReading.json"), reading.to_string()).unwrap();
    dir
}
