//! Running a plan over a folder of record files. Everything the run writes
//! goes in its work folder:
//!
//! - `staged/<ResourceType>.ndjson`: the resources of the stage steps;
//! - `tables/<table>.parquet`: the views' rows and the Libraries' results;
//! - `rows/<table>.ndjson`: a Library's result as records, for a map;
//! - `out/<ResourceType>.ndjson`: the output, which the caller moves into
//!   place or compares.
//!
//! The files are created readable by their owner only. Maps run on several
//! threads a batch at a time, and their resources are written in record
//! order, so the files are the same at any thread count.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;

use super::manifest::RecordRules;
use super::plan::{Input, Package, Plan, PlanStep};
use super::previous::Comparison;
use super::records::{Records, with_type};
use super::tables::Tables;
use crate::error::{PipesError, usage};
use crate::mapping::BATCH;
use crate::views::run::{View, ViewRows};

/// The engine warnings a step keeps in the report; the count stays exact.
const KEPT_WARNINGS: usize = 100;

/// The engine warnings a step prints.
const PRINTED_WARNINGS: usize = 10;

pub struct Options<'a> {
    /// The record files, by type.
    pub files: &'a BTreeMap<String, PathBuf>,
    pub work: &'a Path,
    pub threads: usize,
    pub duckdb: &'a str,
    /// Print each step and its warnings.
    pub echo: bool,
    /// Every resource written to the output must have an id, as `--previous`
    /// compares resources by `resourceType` and `id`.
    pub require_ids: bool,
}

/// What a run did, for the report.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub package: String,
    /// `ok`, `failed` (a data test failed) or `error`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepReport>,
    pub tests: Vec<TestReport>,
    /// The resources written to the output, by type.
    pub output: BTreeMap<String, usize>,
    /// With `--previous`: how the output compares with the previous run's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
}

#[derive(Debug, Default, Serialize)]
pub struct StepReport {
    pub step: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stage: bool,
    /// The record file read, when the step reads one; `null` when the
    /// folder has none for the type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warning_list: Vec<Warning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<TableReport>,
}

#[derive(Debug, Serialize)]
pub struct Warning {
    /// The type of the record the map read.
    pub record_type: String,
    /// `file:line`, or `Library <name> row <n>`.
    pub at: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct TableReport {
    pub table: String,
    /// `view <name>` or `Library <name>`.
    pub from: String,
    pub rows: u64,
}

#[derive(Debug, Serialize)]
pub struct TestReport {
    pub name: String,
    pub check: String,
    pub passed: bool,
    /// The rows that fail.
    pub failures: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Value>,
}

/// Resources written one file per type.
struct TypeFiles {
    dir: PathBuf,
    files: BTreeMap<String, (BufWriter<File>, usize)>,
}

impl TypeFiles {
    fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            files: BTreeMap::new(),
        })
    }

    fn path(&self, t: &str) -> PathBuf {
        self.dir.join(format!("{t}.ndjson"))
    }

    fn write(&mut self, resource: &Value) -> Result<String> {
        let t = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && !t.contains(['/', '\\', '.']))
            .ok_or_else(|| anyhow!("the map wrote a resource with no resourceType"))?
            .to_string();
        if !self.files.contains_key(&t) {
            let path = self.path(&t);
            let file = crate::fs::create_private(&path).map_err(|e| PipesError::io(&path, e))?;
            self.files
                .insert(t.clone(), (BufWriter::with_capacity(1 << 20, file), 0));
        }
        let path = self.path(&t);
        let (w, n) = self.files.get_mut(&t).expect("just inserted");
        serde_json::to_writer(&mut *w, resource)?;
        w.write_all(b"\n").map_err(|e| PipesError::io(&path, e))?;
        *n += 1;
        Ok(t)
    }

    fn flush(&mut self) -> Result<()> {
        for (t, (w, _)) in &mut self.files {
            let path = self.dir.join(format!("{t}.ndjson"));
            w.flush().map_err(|e| PipesError::io(&path, e))?;
        }
        Ok(())
    }

    fn counts(&self) -> BTreeMap<String, usize> {
        self.files
            .iter()
            .map(|(t, (_, n))| (t.clone(), *n))
            .collect()
    }
}

/// Runs `plan`. Returns the report: `ok` with the output in
/// `<work>/out/`, or `failed` when a data test failed, in which case the
/// steps after the tests did not run. A step that cannot run is an error.
pub fn run(package: &Package, plan: &Plan, o: &Options, report: &mut Report) -> Result<()> {
    report.package = package.name.clone();
    let mut staged = TypeFiles::new(o.work.join("staged"))?;
    let mut output = TypeFiles::new(o.work.join("out"))?;
    let mut tables = None;
    let rows_dir = o.work.join("rows");
    for (i, step) in plan.steps.iter().enumerate() {
        crate::stop::check()?;
        let mut r = StepReport {
            step: i + 1,
            ..Default::default()
        };
        match step {
            PlanStep::Map {
                name,
                map,
                model,
                input,
                stage,
            } => {
                r.map = Some(name.clone());
                r.stage = *stage;
                let sink = if *stage { &mut staged } else { &mut output };
                // `[records]` loosens the check of the record files only; a
                // Library's rows are the package's own and are checked as they are.
                let rules = match input {
                    Input::File(_) => package.record_rules(),
                    Input::Rows(_) => RecordRules::default(),
                };
                let records: Box<dyn Iterator<Item = Result<(String, Value)>>> = match input {
                    Input::File(t) => {
                        r.from = Some(t.clone());
                        match o.files.get(t) {
                            Some(path) => {
                                r.file = Some(Some(path.display().to_string()));
                                let shown = path.display().to_string();
                                Box::new(Records::open(path, t)?.map(move |rec| {
                                    rec.map(|(n, v)| (format!("{shown}:{n}"), v))
                                        .map_err(anyhow::Error::from)
                                }))
                            }
                            None => {
                                r.file = Some(None);
                                Box::new(std::iter::empty())
                            }
                        }
                    }
                    Input::Rows(lib) => {
                        r.from = Some(format!("Library {}", lib.name));
                        let table = lib.table(&[]);
                        std::fs::create_dir_all(&rows_dir)?;
                        let path = rows_dir.join(format!("{table}.ndjson"));
                        let t: &mut Tables = tables.as_mut().context("no Library has run")?;
                        if !path.exists() {
                            t.export_rows(&table, &path)?;
                        }
                        Box::new(rows(&path, &lib.name, &model.type_name)?)
                    }
                };
                map_step(
                    package,
                    map,
                    &model.type_name,
                    rules,
                    records,
                    sink,
                    o,
                    &mut r,
                )?;
                if o.echo {
                    let to = if *stage { "staged" } else { "output" };
                    println!(
                        "step {} {name}: {} {} records → {} ({})",
                        i + 1,
                        r.records.unwrap_or(0),
                        model.type_name,
                        to,
                        describe(&r.resources)
                    );
                    if r.file == Some(None) {
                        println!("   no {}.ndjson in the input folder", model.type_name);
                    }
                }
            }
            PlanStep::Library {
                library,
                views,
                queries,
            } => {
                r.library = Some(library.name.clone());
                staged.flush()?;
                if tables.is_none() {
                    tables = Some(Tables::new(o.work, o.duckdb)?);
                }
                let t = tables.as_mut().expect("just made");
                for (view, rows) in run_views(views, &staged, o.threads)? {
                    let n = t.write_view(view, rows)?;
                    r.tables.push(TableReport {
                        table: view.table.clone(),
                        from: format!("view {}", view.name),
                        rows: n as u64,
                    });
                }
                for q in queries {
                    crate::stop::check()?;
                    let n = t.run_query(q, &package.catalog)?;
                    r.tables.push(TableReport {
                        table: q.table(&[]),
                        from: format!("Library {}", q.name),
                        rows: n,
                    });
                }
                if o.echo {
                    let tables: Vec<String> = r
                        .tables
                        .iter()
                        .map(|t| format!("{} {} rows", t.table, t.rows))
                        .collect();
                    println!(
                        "step {} {}: {}",
                        i + 1,
                        library.name,
                        if tables.is_empty() {
                            "already run".to_string()
                        } else {
                            tables.join(", ")
                        }
                    );
                }
            }
        }
        report.steps.push(r);
        if plan.tests_after == Some(i) && !plan.tests.is_empty() {
            let t = tables.as_ref().context("no Library has run")?;
            for (n, test) in plan.tests.iter().enumerate() {
                let found = t
                    .test(&test.check, n)
                    .with_context(|| format!("data test {}", test.label))?;
                report.tests.push(TestReport {
                    name: test.label.clone(),
                    check: test.check.describe(),
                    passed: found.count == 0,
                    failures: found.count,
                    examples: found.examples,
                });
            }
            if o.echo {
                for t in &report.tests {
                    if t.passed {
                        println!("test {} ({}): passed", t.name, t.check);
                    } else {
                        println!("test {} ({}): FAILED, {} rows", t.name, t.check, t.failures);
                        for e in &t.examples {
                            println!("   {e}");
                        }
                    }
                }
            }
            if report.tests.iter().any(|t| !t.passed) {
                report.status = "failed".into();
                return Ok(());
            }
        }
    }
    staged.flush()?;
    output.flush()?;
    report.output = output.counts();
    report.status = "ok".into();
    Ok(())
}

/// "Task 3, MedicationAdministration 4", or "no resources".
pub fn describe(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "no resources".into();
    }
    counts
        .iter()
        .map(|(t, n)| format!("{t} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs `map` over `records` a batch at a time and writes the resources to
/// `sink` in record order. Each record is prepared by `rules` and checked
/// against its model first; a record that does not fit, or that the map
/// fails on, stops the run.
#[allow(clippy::too_many_arguments)]
fn map_step(
    package: &Package,
    map: &fml_model::StructureMap,
    type_name: &str,
    rules: RecordRules,
    mut records: Box<dyn Iterator<Item = Result<(String, Value)>> + '_>,
    sink: &mut TypeFiles,
    o: &Options,
    r: &mut StepReport,
) -> Result<()> {
    let mut count = 0usize;
    let mut warnings = 0usize;
    let need_ids = o.require_ids && !r.stage;
    loop {
        crate::stop::check()?;
        let mut batch: Vec<(String, Value)> = Vec::with_capacity(BATCH);
        for record in records.by_ref() {
            let (at, mut record) = record?;
            package.models.prepare(type_name, &mut record, rules);
            let problems = package.models.check(type_name, &record);
            if let Some(first) = problems.first() {
                let more = match problems.len() - 1 {
                    0 => String::new(),
                    n => format!(" (and {n} more problems in this record)"),
                };
                return Err(usage(anyhow!(
                    "{at}: {first}{more}; `fhir-pipes import check --in` lists every problem"
                )));
            }
            batch.push((at, record));
            if batch.len() == BATCH {
                break;
            }
        }
        let sources: Vec<&Value> = batch.iter().map(|(_, v)| v).collect();
        let outputs = package.maps.transform_all(map, &sources, o.threads);
        for ((at, _), output) in batch.iter().zip(outputs) {
            let output = output.map_err(|e| usage(anyhow!("{at}: {e}")))?;
            for w in output.warnings {
                warnings += 1;
                if o.echo && warnings <= PRINTED_WARNINGS {
                    eprintln!("warning: {at}: {w}");
                }
                if r.warning_list.len() < KEPT_WARNINGS {
                    r.warning_list.push(Warning {
                        record_type: type_name.to_string(),
                        at: at.clone(),
                        message: w,
                    });
                }
            }
            for resource in &output.resources {
                if need_ids && !has_id(resource) {
                    let t = resource["resourceType"].as_str().unwrap_or("resource");
                    return Err(usage(anyhow!(
                        "{at}: map {} wrote a {t} with no id; --previous compares resources \
                         by resourceType and id",
                        r.map.as_deref().unwrap_or_default()
                    )));
                }
                let t = sink.write(resource).with_context(|| at.clone())?;
                *r.resources.entry(t).or_default() += 1;
            }
            count += 1;
        }
        if batch.len() < BATCH {
            break;
        }
    }
    if o.echo && warnings > PRINTED_WARNINGS {
        eprintln!(
            "warning: {} more engine warnings in step {}; the run report lists the first \
             {KEPT_WARNINGS}",
            warnings - PRINTED_WARNINGS,
            r.step
        );
    }
    r.records = Some(count);
    r.warnings = Some(warnings);
    Ok(())
}

/// Whether a resource has an `id`: a non-empty string.
fn has_id(resource: &Value) -> bool {
    resource
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// The rows of a Library's result as records of `type_name`, numbered from
/// 1: SQL NULLs left out, and `resourceType` set.
fn rows(
    path: &Path,
    library: &str,
    type_name: &str,
) -> Result<impl Iterator<Item = Result<(String, Value)>> + use<>> {
    let lines = crate::fhir::ndjson::NdjsonReader::open(path)?;
    let (library, type_name) = (library.to_string(), type_name.to_string());
    Ok(lines.map(move |line| {
        let line = line?;
        let at = format!("Library {library} row {}", line.number);
        let mut row: Value = serde_json::from_str(&line.text)
            .with_context(|| format!("{at}: duckdb wrote a line that is not JSON"))?;
        drop_nulls(&mut row);
        let record = with_type(row, &type_name).map_err(|m| usage(anyhow!("{at}: {m}")))?;
        Ok((at, record))
    }))
}

/// Removes every member whose value is null, at any depth: FHIR JSON has no
/// nulls, and a SQL NULL means the element is absent.
fn drop_nulls(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.retain(|_, v| !v.is_null());
            o.values_mut().for_each(drop_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

/// Runs `views` over the staged resources, each staged file read once for
/// the views of its type. A type nothing staged gives empty tables. The
/// results come in the order of `views`.
fn run_views<'v>(
    views: &[&'v View],
    staged: &TypeFiles,
    threads: usize,
) -> Result<Vec<(&'v View, fhir_pipes_sof::ViewResult)>> {
    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, v) in views.iter().enumerate() {
        by_type.entry(v.resource.as_str()).or_default().push(i);
    }
    let mut results: Vec<Option<fhir_pipes_sof::ViewResult>> = views.iter().map(|_| None).collect();
    for (t, idx) in by_type {
        let vs: Vec<&View> = idx.iter().map(|&i| views[i]).collect();
        let path = staged.path(t);
        let out = if path.exists() {
            crate::views::run::run_file(&vs, &path, threads)?.1
        } else {
            ViewRows::new(
                vs.iter()
                    .map(|v| (v.name.as_str(), &v.definition))
                    .collect(),
            )
            .finish(threads)?
        };
        for (i, r) in idx.into_iter().zip(out) {
            results[i] = Some(r);
        }
    }
    Ok(views
        .iter()
        .copied()
        .zip(results.into_iter().map(|r| r.expect("every view ran")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nulls_are_dropped_at_any_depth() {
        let mut v = json!({"a": null, "b": [{"c": null, "d": 1}, null], "e": {"f": null}});
        drop_nulls(&mut v);
        assert_eq!(v, json!({"b": [{"d": 1}, null], "e": {}}));
    }
}
