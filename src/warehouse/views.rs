//! Finding the ViewDefinitions and running them in memory with fhir-pipes-sof.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fhir_pipes_sof::{ViewDefinition, ViewResult, execute_parallel};
use fhir_pipes_sof_lint::{Severity, validate_structure};
use serde_json::Value;

/// A ViewDefinition found in the views directory.
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    /// The FHIR resource type it reads.
    pub resource: String,
    /// The warehouse table it writes: parquet/<table>/.
    pub table: String,
    /// The view as plain ViewDefinition JSON.
    pub json: Value,
    pub definition: ViewDefinition,
}

/// Every ViewDefinition in `dir`, in file-name order. A file counts when its
/// `resourceType` is `ViewDefinition` or ends with `/ViewDefinition` (an IG's
/// logical-model instance, as SUSHI writes it into Binary-*.json); other JSON
/// files are skipped. Each view's structure is checked, and errors stop here.
pub fn discover(dir: &Path, strip_prefix: &[String]) -> Result<Vec<View>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading views directory {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut views = Vec::new();
    for path in files {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let Ok(mut json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let is_view = json
            .get("resourceType")
            .and_then(Value::as_str)
            .is_some_and(|t| t == "ViewDefinition" || t.ends_with("/ViewDefinition"));
        if !is_view {
            continue;
        }
        json["resourceType"] = Value::String("ViewDefinition".into());
        let definition = ViewDefinition::from_json(&json)
            .with_context(|| format!("parsing ViewDefinition {}", path.display()))?;
        let errors: Vec<String> = validate_structure(&definition)
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
            .map(|f| format!("{}: {}", f.code, f.message))
            .collect();
        if !errors.is_empty() {
            bail!(
                "{} is not a valid ViewDefinition:\n  {}",
                path.display(),
                errors.join("\n  ")
            );
        }
        let name = definition
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .with_context(|| format!("{} has no name", path.display()))?;
        views.push(View {
            table: table_name(&name, strip_prefix),
            resource: definition.resource.clone(),
            name,
            json,
            definition,
        });
    }
    let mut seen = BTreeMap::new();
    for v in &views {
        if let Some(other) = seen.insert(v.table.clone(), v.name.clone()) {
            bail!("views {other} and {} both write table {}", v.name, v.table);
        }
    }
    Ok(views)
}

/// The table name for a view: snake case, minus the first matching prefix
/// (`IcrIndicatorValue` with `icr_` → `indicator_value`).
pub fn table_name(view_name: &str, strip_prefix: &[String]) -> String {
    let mut snake = String::with_capacity(view_name.len() + 4);
    for (i, c) in view_name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(c.to_ascii_lowercase());
    }
    strip_prefix
        .iter()
        .find_map(|p| snake.strip_prefix(p.as_str()).map(str::to_string))
        .unwrap_or(snake)
}

/// Write each view, as plain ViewDefinition JSON, to `dir/<name>.json`.
pub fn save(views: &[View], dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for v in views {
        let path = dir.join(format!("{}.json", v.name));
        let text = serde_json::to_string_pretty(&v.json)? + "\n";
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Read an NDJSON file on `threads` threads, in line order. Blank lines are
/// skipped; an error names the first bad line.
pub fn read_ndjson(path: &Path, threads: usize) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let chunk = lines.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        let workers: Vec<_> = lines
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(i, line)| {
                            serde_json::from_str(line).with_context(|| {
                                format!("parsing {} line {}", path.display(), i + 1)
                            })
                        })
                        .collect::<Result<Vec<Value>>>()
                })
            })
            .collect();
        let mut values = Vec::with_capacity(lines.len());
        for w in workers {
            values.extend(
                w.join()
                    .map_err(|_| anyhow::anyhow!("parsing thread panicked"))??,
            );
        }
        Ok(values)
    })
}

/// Run every view that reads `resources`' type against them.
pub fn run(views: &[&View], resources: &[Value], threads: usize) -> Result<Vec<ViewResult>> {
    views
        .iter()
        .map(|v| {
            execute_parallel(&v.definition, resources, threads)
                .with_context(|| format!("running view {}", v.name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names() {
        let icr = vec!["icr_".to_string()];
        assert_eq!(table_name("IcrIndicatorValue", &icr), "indicator_value");
        assert_eq!(table_name("IcrLocationStatus", &icr), "location_status");
        assert_eq!(
            table_name("PatientDemographics", &icr),
            "patient_demographics"
        );
        assert_eq!(table_name("IcrIndicatorValue", &[]), "icr_indicator_value");
    }

    #[test]
    fn discovers_only_view_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let view = |rt: &str, name: &str| {
            serde_json::json!({"resourceType": rt, "name": name, "resource": "Patient", "status": "active",
                "select": [{"column": [{"name": "id", "path": "getResourceKey()"}]}]})
            .to_string()
        };
        fs::write(
            dir.path().join("Binary-B.json"),
            view(
                "https://sql-on-fhir.org/ig/StructureDefinition/ViewDefinition",
                "IcrB",
            ),
        )
        .unwrap();
        fs::write(dir.path().join("A.json"), view("ViewDefinition", "IcrA")).unwrap();
        fs::write(
            dir.path().join("Patient-x.json"),
            r#"{"resourceType":"Patient"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let views = discover(dir.path(), &["icr_".into()]).unwrap();
        let names: Vec<_> = views
            .iter()
            .map(|v| (v.name.as_str(), v.table.as_str()))
            .collect();
        assert_eq!(names, [("IcrA", "a"), ("IcrB", "b")]);
        assert_eq!(views[1].json["resourceType"], "ViewDefinition");
    }

    #[test]
    fn rejects_two_views_writing_one_table() {
        let dir = tempfile::tempdir().unwrap();
        for (file, name) in [("a.json", "IcrX"), ("b.json", "X")] {
            let v = serde_json::json!({"resourceType": "ViewDefinition", "name": name, "resource": "Patient",
                "status": "active", "select": [{"column": [{"name": "id", "path": "id"}]}]});
            fs::write(dir.path().join(file), v.to_string()).unwrap();
        }
        assert!(discover(dir.path(), &["icr_".into()]).is_err());
    }

    #[test]
    fn ndjson_keeps_order_and_names_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.ndjson");
        let text: String = (0..50).map(|i| format!("{{\"n\":{i}}}\n\n")).collect();
        fs::write(&p, text).unwrap();
        let values = read_ndjson(&p, 4).unwrap();
        assert_eq!(values.len(), 50);
        assert!(values.iter().enumerate().all(|(i, v)| v["n"] == i));
        fs::write(&p, "{}\n{oops\n").unwrap();
        let err = format!("{:#}", read_ndjson(&p, 2).unwrap_err());
        assert!(err.contains("line 2"), "{err}");
    }
}
