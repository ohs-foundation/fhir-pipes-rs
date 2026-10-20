//! Finding the ViewDefinitions and running them in memory with fhir-pipes-sof.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fhir_pipes_sof::{CompiledView, ViewDefinition, ViewResult};
use fhir_pipes_sof_lint::{Severity, validate_structure};
use serde_json::Value;

use crate::views::ndjson;

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
    let mut files = crate::fs::sorted_entries(dir)
        .with_context(|| format!("reading views directory {}", dir.display()))?;
    files.retain(|p| p.extension().is_some_and(|x| x == "json"));
    let mut views = Vec::new();
    for path in files {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some((json, definition)) = parse_view(&text)
            .with_context(|| format!("parsing ViewDefinition {}", path.display()))?
        else {
            continue;
        };
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
        let table = table_name(&name, strip_prefix);
        check_table(&name, &table)?;
        views.push(View {
            table,
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

/// Parse a ViewDefinition from JSON text. An IG build's logical-model form,
/// whose `resourceType` ends with `/ViewDefinition`, is accepted and returned
/// as plain ViewDefinition JSON. `Ok(None)` when the text is not JSON or not a
/// view.
pub fn parse_view(text: &str) -> Result<Option<(Value, ViewDefinition)>> {
    let Ok(mut json) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    let is_view = json
        .get("resourceType")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "ViewDefinition" || t.ends_with("/ViewDefinition"));
    if !is_view {
        return Ok(None);
    }
    json["resourceType"] = Value::String("ViewDefinition".into());
    let definition = ViewDefinition::from_json(&json)?;
    Ok(Some((json, definition)))
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

/// A view's table must be a SQL name, which also keeps it one directory
/// under parquet/, and must not be the table the Location transform writes.
fn check_table(view: &str, table: &str) -> Result<()> {
    let sql_name = table.starts_with(|c: char| c.is_ascii_lowercase())
        && table
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !sql_name {
        bail!(
            "view {view} would write table {table:?}, which is not a SQL name; rename the \
             view or change strip_prefix"
        );
    }
    if table == "locations" {
        bail!(
            "view {view} would write table locations, which the Location transform writes; \
             rename the view"
        );
    }
    Ok(())
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

/// Several views' rows, run a chunk of resources at a time: each chunk's
/// rows are appended to what the chunks before it gave, so the rows come in
/// the order of the resources, as from one run over them all. Each view is
/// compiled once, when it first runs, and reused for every chunk.
pub struct ViewRows<'a> {
    views: Vec<(&'a str, &'a ViewDefinition)>,
    compiled: Vec<Option<CompiledView>>,
    results: Vec<Option<ViewResult>>,
}

impl<'a> ViewRows<'a> {
    /// `views`: each view's name (for messages) and definition.
    pub fn new(views: Vec<(&'a str, &'a ViewDefinition)>) -> Self {
        let compiled = views.iter().map(|_| None).collect();
        let results = views.iter().map(|_| None).collect();
        Self {
            views,
            compiled,
            results,
        }
    }

    /// Run every view on `resources` and keep the rows. A view skips the
    /// resources of other types than its own.
    pub fn add(&mut self, resources: &[Value], threads: usize) -> Result<()> {
        self.run(resources, threads, None)
    }

    /// Run the views that read `rtype` on `resources`, all of that type.
    pub fn add_typed(&mut self, rtype: &str, resources: &[Value], threads: usize) -> Result<()> {
        self.run(resources, threads, Some(rtype))
    }

    fn run(&mut self, resources: &[Value], threads: usize, only: Option<&str>) -> Result<()> {
        for (i, (name, view)) in self.views.iter().enumerate() {
            if only.is_some_and(|t| t != view.resource) {
                continue;
            }
            let r = compiled(&mut self.compiled[i], view, name)?
                .execute_parallel(resources, threads)
                .with_context(|| format!("running view {name}"))?;
            match &mut self.results[i] {
                Some(all) => {
                    all.data.extend(r.data);
                }
                slot => *slot = Some(r),
            }
        }
        Ok(())
    }

    /// Each view's rows, in the order of the views. A view that never ran
    /// (no resources) is run on none, which gives its columns, or its error.
    pub fn finish(self, threads: usize) -> Result<Vec<ViewResult>> {
        self.views
            .iter()
            .zip(self.compiled)
            .zip(self.results)
            .map(|(((name, view), mut c), r)| match r {
                Some(r) => Ok(r),
                None => compiled(&mut c, view, name)?
                    .execute_parallel(&[], threads)
                    .with_context(|| format!("running view {name}")),
            })
            .collect()
    }
}

/// The view compiled in `slot`, compiling it on first use. A view that does
/// not compile is the view's fault: a usage error, whatever the input and
/// the output format.
fn compiled<'c>(
    slot: &'c mut Option<CompiledView>,
    view: &ViewDefinition,
    name: &str,
) -> Result<&'c CompiledView> {
    if slot.is_none() {
        let c = CompiledView::compile_blocking(view)
            .with_context(|| format!("compiling view {name}"))
            .map_err(crate::error::usage)?;
        *slot = Some(c);
    }
    Ok(slot.as_ref().expect("just compiled"))
}

/// Run `views` over the NDJSON file at `path`, a chunk of lines at a time,
/// on `threads` threads. Returns the number of resources read and each
/// view's rows, in file order.
pub fn run_file(views: &[&View], path: &Path, threads: usize) -> Result<(usize, Vec<ViewResult>)> {
    run_lines(views, ndjson::open(path)?, threads)
}

fn run_lines<R: std::io::BufRead>(
    views: &[&View],
    lines: ndjson::Lines<R>,
    threads: usize,
) -> Result<(usize, Vec<ViewResult>)> {
    let mut rows = ViewRows::new(
        views
            .iter()
            .map(|v| (v.name.as_str(), &v.definition))
            .collect(),
    );
    let mut count = 0;
    for chunk in lines {
        let resources = chunk?.values(threads)?;
        count += resources.len();
        rows.add(&resources, threads)?;
    }
    Ok((count, rows.finish(threads)?))
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
    fn rejects_a_table_name_that_is_empty_or_the_locations_table() {
        for (name, why) in [
            ("IcrLocations", "Location transform"),
            ("Icr", "not a SQL name"),
            ("Icr2x", "not a SQL name"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let view = serde_json::json!({"resourceType": "ViewDefinition", "name": name,
                "resource": "Patient", "status": "active",
                "select": [{"column": [{"name": "id", "path": "getResourceKey()"}]}]});
            fs::write(dir.path().join("v.json"), view.to_string()).unwrap();
            let err = discover(dir.path(), &["icr_".into(), "icr".into()]).unwrap_err();
            assert!(err.to_string().contains(why), "{name}: {err}");
        }
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

    fn view(name: &str, resource: &str, select: Value) -> View {
        let json = serde_json::json!({"resourceType": "ViewDefinition", "name": name,
            "resource": resource, "status": "active", "select": select});
        View {
            name: name.into(),
            resource: resource.into(),
            table: table_name(name, &[]),
            definition: ViewDefinition::from_json(&json).unwrap(),
            json,
        }
    }

    /// Rows from a chunk at a time are the rows of one run over the whole
    /// file, in file order, for a view with one row per resource and for one
    /// with a row per repeated element.
    #[test]
    fn chunks_give_the_rows_of_one_run_in_file_order() {
        let ids = view(
            "Ids",
            "Patient",
            serde_json::json!([{"column": [{"name": "id", "path": "id"}]}]),
        );
        let names = view(
            "Names",
            "Patient",
            serde_json::json!([{"column": [{"name": "id", "path": "id"}]},
                {"forEach": "name", "column": [{"name": "family", "path": "family"}]}]),
        );
        let other = view(
            "Obs",
            "Observation",
            serde_json::json!([{"column": [{"name": "id", "path": "id"}]}]),
        );
        let text: String = (0..103)
            .map(|i| {
                let r = if i % 10 == 3 {
                    serde_json::json!({"resourceType": "Observation", "id": format!("o{i}")})
                } else {
                    let names: Vec<Value> = (0..i % 3)
                        .map(|n| serde_json::json!({"family": format!("f{i}-{n}")}))
                        .collect();
                    serde_json::json!({"resourceType": "Patient", "id": format!("p{i}"), "name": names})
                };
                format!("{r}\n\n")
            })
            .collect();
        let views = [&ids, &names, &other];
        let resources: Vec<Value> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let whole: Vec<ViewResult> = views
            .iter()
            .map(|v| fhir_pipes_sof::execute_parallel(&v.definition, &resources, 1).unwrap())
            .collect();
        for (lines, threads) in [(1, 1), (7, 3), (50, 8), (1000, 2)] {
            let source = ndjson::Lines::new(text.as_bytes(), "x").with_limits(lines, usize::MAX);
            let (n, chunked) = run_lines(&views, source, threads).unwrap();
            assert_eq!(n, 103);
            for (a, b) in chunked.iter().zip(&whole) {
                assert_eq!(a.data, b.data, "{lines} lines a chunk");
                assert_eq!(a.columns.len(), b.columns.len());
            }
        }
        assert_eq!(whole[0].data.len(), 93);
        assert_eq!(whole[2].data.len(), 10);
    }

    #[test]
    fn a_view_with_no_resources_has_its_columns_and_a_bad_line_is_named() {
        let ids = view(
            "Ids",
            "Patient",
            serde_json::json!([{"column": [{"name": "id", "path": "id"}, {"name": "n", "path": "1"}]}]),
        );
        let (n, r) = run_lines(&[&ids], ndjson::Lines::new(&b"\n"[..], "x"), 2).unwrap();
        assert_eq!(n, 0);
        assert_eq!(r[0].columns.len(), 2);
        assert!(r[0].data.is_empty());
        let source = ndjson::Lines::new(&b"{}\n{oops\n"[..], "x.ndjson");
        let err = format!("{:#}", run_lines(&[&ids], source, 2).err().unwrap());
        assert!(err.contains("x.ndjson line 2"), "{err}");
    }
}
