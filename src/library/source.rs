//! Build Library JSON from a `.sql` file and its `.toml` metadata.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{SQL_TEXT_EXTENSION, SQLQUERY_PROFILE, SQLVIEW_PROFILE, check_label};
use crate::error::usage;

/// Dialects with a `<stem>.<dialect>.sql` sibling file.
const DIALECTS: [&str; 2] = ["duckdb", "postgresql"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    url: String,
    name: String,
    version: Option<String>,
    status: String,
    kind: String,
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    depends_on: Vec<MetaDependency>,
    #[serde(default)]
    parameter: Vec<MetaParameter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaDependency {
    label: Option<String>,
    resource: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaParameter {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    documentation: Option<String>,
}

/// Read `<stem>.sql`, its dialect siblings and `<stem>.toml`, and build the
/// Library JSON.
pub fn build_source(sql_path: &Path) -> Result<Value> {
    let read = |p: &Path| {
        fs::read_to_string(p)
            .with_context(|| format!("reading {}", p.display()))
            .map_err(usage)
    };
    let sql = read(sql_path)?;
    let meta = read(&sql_path.with_extension("toml"))?;
    let stem = sql_path.with_extension("");
    let mut dialects = Vec::new();
    for dialect in DIALECTS {
        let p = stem.with_extension(format!("{dialect}.sql"));
        if p.exists() {
            dialects.push((dialect.to_string(), read(&p)?));
        }
    }
    build_from(&sql, &dialects, &meta)
        .with_context(|| format!("building a Library from {}", sql_path.display()))
        .map_err(usage)
}

/// Build the Library JSON from the portable SQL, (dialect, SQL) pairs and the
/// metadata TOML text.
pub fn build_from(sql: &str, dialects: &[(String, String)], meta: &str) -> Result<Value> {
    let meta: Meta = toml::from_str(meta).context("parsing the metadata")?;
    let (profile, is_view) = match meta.kind.as_str() {
        "SQLView" => (SQLVIEW_PROFILE, true),
        "SQLQuery" => (SQLQUERY_PROFILE, false),
        other => bail!("kind must be SQLQuery or SQLView, not {other:?}"),
    };
    if is_view && !meta.parameter.is_empty() {
        bail!("a SQLView cannot declare parameters");
    }
    if !matches!(meta.status.as_str(), "draft" | "active") {
        bail!("status must be draft or active, not {:?}", meta.status);
    }

    let mut related = Vec::new();
    for dep in &meta.depends_on {
        let label = dep
            .label
            .as_deref()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "depends_on {} needs a label (the table name in the SQL)",
                    dep.resource
                )
            })?;
        check_label(label).with_context(|| format!("Library {}", meta.name))?;
        related.push(json!({"type": "depends-on", "label": label, "resource": dep.resource}));
    }

    let mut content = vec![content_entry("application/sql", sql)];
    for (dialect, text) in dialects {
        content.push(content_entry(
            &format!("application/sql;dialect={dialect}"),
            text,
        ));
    }

    let mut lib = json!({
        "resourceType": "Library",
        "meta": {"profile": [profile]},
        "url": meta.url,
        "name": meta.name,
        "status": meta.status,
        "type": {"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/library-type",
            "code": "logic-library",
        }]},
    });
    let obj = lib.as_object_mut().expect("a JSON object");
    for (key, value) in [
        ("version", &meta.version),
        ("title", &meta.title),
        ("description", &meta.description),
    ] {
        if let Some(v) = value {
            obj.insert(key.into(), json!(v));
        }
    }
    if !related.is_empty() {
        obj.insert("relatedArtifact".into(), Value::Array(related));
    }
    if !meta.parameter.is_empty() {
        let params: Vec<Value> = meta
            .parameter
            .iter()
            .map(|p| {
                let mut o = json!({"name": p.name, "use": "in", "type": p.type_});
                if let Some(d) = &p.documentation {
                    o["documentation"] = json!(d);
                }
                o
            })
            .collect();
        obj.insert("parameter".into(), Value::Array(params));
    }
    obj.insert("content".into(), Value::Array(content));
    Ok(lib)
}

/// One `content` entry: the SQL as base64 `data`, and as the sql-text extension.
fn content_entry(content_type: &str, sql: &str) -> Value {
    let sql = sql.trim_end_matches(['\n', '\r']);
    json!({
        "contentType": content_type,
        "data": STANDARD.encode(sql),
        "extension": [{"url": SQL_TEXT_EXTENSION, "valueString": sql}],
    })
}

/// Build every `*.sql` in `src` that has a sibling `.toml`, writing
/// `out/Library-<name>.json`. Returns how many were written.
pub fn build_dir(src: &Path, out: &Path) -> Result<usize> {
    let entries = fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))
        .map_err(usage)?;
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql") && p.with_extension("toml").exists())
        .collect();
    paths.sort();
    fs::create_dir_all(out)
        .with_context(|| format!("creating {}", out.display()))
        .map_err(usage)?;
    for path in &paths {
        let lib = build_source(path)?;
        let name = lib["name"].as_str().unwrap_or_default();
        let dest = out.join(format!("Library-{name}.json"));
        let text = serde_json::to_string_pretty(&lib)? + "\n";
        fs::write(&dest, text).with_context(|| format!("writing {}", dest.display()))?;
    }
    Ok(paths.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;

    const META: &str = r#"
url = "https://icr.healthcampaigns.org/uganda/Library/UgandaTasks"
name = "UgandaTasks"
version = "0.1.0"
status = "draft"
kind = "SQLQuery"

[[depends_on]]
label = "task"
resource = "https://icr.healthcampaigns.org/uganda/Library/IcrCampaignTaskByArea"
"#;

    /// A temp dir holding t.sql and t.toml (`meta`).
    fn source(meta: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("t.sql"), "SELECT 1 AS x\n").unwrap();
        fs::write(dir.path().join("t.toml"), meta).unwrap();
        dir
    }

    #[test]
    fn build_source_writes_a_sqlquery_library() {
        let dir = source(META);
        let v = build_source(&dir.path().join("t.sql")).unwrap();
        assert_eq!(v["resourceType"], "Library");
        assert_eq!(v["meta"]["profile"], json!([SQLQUERY_PROFILE]));
        assert_eq!(v["type"]["coding"][0]["code"], "logic-library");
        assert_eq!(
            v["relatedArtifact"][0],
            json!({"type": "depends-on", "label": "task",
                "resource": "https://icr.healthcampaigns.org/uganda/Library/IcrCampaignTaskByArea"})
        );
        let c = &v["content"][0];
        assert_eq!(c["contentType"], "application/sql");
        let data = STANDARD.decode(c["data"].as_str().unwrap()).unwrap();
        assert_eq!(data, b"SELECT 1 AS x");
        assert_eq!(
            c["extension"][0],
            json!({"url": SQL_TEXT_EXTENSION, "valueString": "SELECT 1 AS x"})
        );
    }

    #[test]
    fn a_dialect_file_adds_dialect_content() {
        let dir = source(META);
        fs::write(dir.path().join("t.duckdb.sql"), "SELECT 2 AS x").unwrap();
        let v = build_source(&dir.path().join("t.sql")).unwrap();
        assert_eq!(v["content"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["content"][1]["contentType"],
            "application/sql;dialect=duckdb"
        );
    }

    #[test]
    fn from_json_round_trips_and_picks_dialect() {
        let dir = source(META);
        fs::write(dir.path().join("t.duckdb.sql"), "SELECT 2 AS x").unwrap();
        let lib = Library::from_json(&build_source(&dir.path().join("t.sql")).unwrap()).unwrap();
        assert_eq!(lib.name, "UgandaTasks");
        assert_eq!(lib.version.as_deref(), Some("0.1.0"));
        assert_eq!(lib.depends_on[0].label, "task");
        assert_eq!(lib.sql("duckdb"), Some("SELECT 2 AS x"));
        assert_eq!(lib.sql("postgresql"), Some("SELECT 1 AS x"));
        assert_eq!(lib.table(&[]), "uganda_tasks");
    }

    #[test]
    fn a_sqlview_with_parameters_is_refused() {
        let meta = META.replace("SQLQuery", "SQLView")
            + "\n[[parameter]]\nname = \"p\"\ntype = \"string\"\n";
        let dir = source(&meta);
        let err = build_source(&dir.path().join("t.sql")).unwrap_err();
        assert!(
            format!("{err:#}").contains("SQLView cannot declare parameters"),
            "{err:#}"
        );
    }

    #[test]
    fn a_dependency_without_label_is_refused() {
        let meta = META.replace("label = \"task\"\n", "");
        let dir = source(&meta);
        let err = build_source(&dir.path().join("t.sql")).unwrap_err();
        assert!(format!("{err:#}").contains("needs a label"), "{err:#}");
    }

    #[test]
    fn a_label_must_be_a_lowercase_sql_name() {
        for bad in ["Tasks", "my-table"] {
            let meta = META.replace("label = \"task\"", &format!("label = \"{bad}\""));
            let err = build_from("SELECT 1", &[], &meta).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("UgandaTasks") && msg.contains(bad) && msg.contains("lowercase"),
                "{msg}"
            );
        }
        let meta = META.replace("label = \"task\"", "label = \"group\"");
        assert!(build_from("SELECT 1", &[], &meta).is_ok());
    }

    #[test]
    fn build_from_parses_a_dependency_version() {
        let meta = META.replace("IcrCampaignTaskByArea", "IcrCampaignTaskByArea|1.2");
        let v = build_from("SELECT 1", &[], &meta).unwrap();
        let lib = Library::from_json(&v).unwrap();
        assert_eq!(lib.depends_on[0].version.as_deref(), Some("1.2"));
        assert!(
            lib.depends_on[0]
                .canonical
                .ends_with("IcrCampaignTaskByArea")
        );
    }
}
