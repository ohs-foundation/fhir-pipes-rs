//! `fhir-pipes import check|run|test`: run an import package over a folder
//! of record files and write FHIR NDJSON for `fhir-pipes load`.
//!
//! A package holds the logical models of the records (`models/`), the FHIR
//! Mapping Language maps and ConceptMaps that turn them into resources
//! (`maps/`), and, where records have to be joined or grouped first,
//! ViewDefinitions (`views/`) and SQLQuery Libraries (`libraries/`) that run
//! over resources staged by earlier maps, or, for a record view (a view
//! whose `resource` is a record type), over the record files themselves.
//! `import.toml` lists the steps.
//! See docs/import.md.
//!
//! - `manifest`: `import.toml`;
//! - `records`: the record files and the check of a record against its model;
//! - `plan`: the package loaded, and its steps resolved;
//! - `run`: the steps run in a work folder;
//! - `tables`: views and Libraries over Parquet in duckdb, and the data tests;
//! - `compare`: a run's output against a test case's expected files;
//! - `previous`: a run's output against an earlier run's (`--previous`).

mod compare;
mod manifest;
mod parameters;
mod plan;
mod previous;
mod records;
mod run;
mod tables;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::json;

use crate::error::{Findings, usage};
use parameters::Given;
use plan::{Package, Plan};
use previous::{Ignores, Previous};
use run::{Options, Report};

/// The record problems printed per file by `import check --in`.
const PROBLEMS_PER_FILE: usize = 20;

/// The ignored members named per file by `import check --in`.
const IGNORED_PER_FILE: usize = 20;

/// A test case's parameter values: `tests/<case>/parameters.toml`, one
/// `NAME = value` per line.
const CASE_PARAMETERS: &str = "parameters.toml";

/// The file in `--out` that lists what the last run wrote there, so a run
/// with `--overwrite` removes only its own files.
const WRITTEN: &str = ".fhir-pipes-import.json";

#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// The package folder: import.toml, models/, maps/, views/, libraries/
    #[arg(long, value_name = "DIR")]
    pub package: PathBuf,
    /// Also check every record of every file in this folder against its logical model
    #[arg(long = "in", value_name = "DIR")]
    pub input: Option<PathBuf>,
    /// Set a parameter of the package, overriding its default: NAME=VALUE (repeatable)
    #[arg(long = "param", value_name = "NAME=VALUE", value_parser = parameters::parse_assignment)]
    pub params: Vec<(String, String)>,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// The package folder: import.toml, models/, maps/, views/, libraries/
    #[arg(long, value_name = "DIR")]
    pub package: PathBuf,
    /// The record files: one <Type>.ndjson (or .ndjson.gz) per record type
    #[arg(long = "in", value_name = "DIR")]
    pub input: PathBuf,
    /// Folder for the output, one <ResourceType>.ndjson per type. Must be empty or not exist, unless --overwrite
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Replace the files an earlier run wrote in --out, and remove those this run does not write
    #[arg(long)]
    pub overwrite: bool,
    /// Threads that run the maps and views (default: the number of CPUs). The output is the same at any count
    #[arg(long)]
    pub threads: Option<usize>,
    /// Write the run report, JSON, to this file
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Keep the staged resources, view tables and Library results in this folder (empty or new) instead of removing them
    #[arg(long, value_name = "DIR")]
    pub keep_work: Option<PathBuf>,
    /// The --out folder of an earlier run of the same package: compare this run's resources with it, by resourceType and id. May be --out itself with --overwrite
    #[arg(long, value_name = "DIR")]
    pub previous: Option<PathBuf>,
    /// With --previous: write the new and changed resources here, one <ResourceType>.ndjson per type. Must be empty or not exist, unless --overwrite
    #[arg(long, value_name = "DIR", requires = "previous")]
    pub changed: Option<PathBuf>,
    /// With --previous: write here, sorted, one ResourceType/id per line for every resource of the previous run that this run did not write
    #[arg(long, value_name = "FILE", requires = "previous")]
    pub removed: Option<PathBuf>,
    /// Set a parameter of the package, overriding its default: NAME=VALUE (repeatable)
    #[arg(long = "param", value_name = "NAME=VALUE", value_parser = parameters::parse_assignment)]
    pub params: Vec<(String, String)>,
}

#[derive(clap::Args, Debug)]
pub struct TestArgs {
    /// The package folder; its test cases are tests/<case>/in/ and tests/<case>/expected/
    #[arg(long, value_name = "DIR")]
    pub package: PathBuf,
    /// Run only this test case
    #[arg(long, value_name = "NAME")]
    pub case: Option<String>,
    /// Write each case's output to its expected/ folder instead of comparing
    #[arg(long)]
    pub update: bool,
    /// Threads that run the maps and views (default: the number of CPUs)
    #[arg(long)]
    pub threads: Option<usize>,
    /// Set a parameter of the package for every case, overriding its default: NAME=VALUE (repeatable). A case's parameters.toml overrides it
    #[arg(long = "param", value_name = "NAME=VALUE", value_parser = parameters::parse_assignment)]
    pub params: Vec<(String, String)>,
}

/// `NAME=VALUE`, as `--param` takes it, for the commands outside `import`
/// that run a package's maps.
pub fn parse_param(text: &str) -> std::result::Result<(String, String), String> {
    parameters::parse_assignment(text)
}

/// A package's maps, with the values of a run's parameters, for the
/// commands that map records one at a time as they read them instead of
/// running `import run` over a folder (`dhis2 locations|measures|reports`).
pub struct PackageMaps {
    /// The package folder.
    pub dir: PathBuf,
    pub name: String,
    pub maps: crate::mapping::MapPackage,
    /// The value of each parameter: its default, or `--param`.
    pub values: BTreeMap<String, serde_json::Value>,
}

/// Loads the package in `dir`, or the package whose `maps/` folder `dir` is,
/// and sets its parameters: the defaults, then `params`. Only a package
/// without steps can be run this way, since each record goes through the
/// one map that reads its type.
pub fn package_maps(dir: &Path, params: &[(String, String)]) -> Result<PackageMaps> {
    let root = if !dir.join(manifest::FILE).is_file()
        && dir.file_name().is_some_and(|n| n == "maps")
        && dir.join("..").join(manifest::FILE).is_file()
    {
        dir.join("..")
    } else {
        dir.to_path_buf()
    };
    let mut package = Package::load(&root)?;
    if package
        .manifest
        .as_ref()
        .is_some_and(|m| !m.steps.is_empty())
    {
        return Err(usage(anyhow!(
            "{}: the package has [[step]]s, so its records cannot be mapped one at a time; \
             write the records with `fhir-pipes dhis2 extract`, then run `fhir-pipes import \
             run --package {}`",
            root.display(),
            root.display()
        )));
    }
    package.set_values(&[Given::Text(params)], true)?;
    let values = package
        .values
        .iter()
        .map(|(n, v)| (n.clone(), v.json()))
        .collect();
    Ok(PackageMaps {
        dir: root,
        name: package.name,
        maps: package.maps,
        values,
    })
}

/// The parameter defaults of the package each folder of maps belongs to,
/// for the commands that run a package's maps outside `import` (`map run`):
/// the `[parameters]` of `DIR/import.toml` or, for a
/// package's `maps/` folder, `DIR/../import.toml`. Each value is (name, JSON,
/// FHIR type) for `MapPackage::set_parameters`, and the manifests read are
/// returned too. A parameter with no default is an error: those commands
/// have no `--param`.
pub fn map_parameter_defaults(
    dirs: &[PathBuf],
) -> crate::error::Result<(Vec<crate::mapping::MapParameter>, Vec<PathBuf>)> {
    let mut values: BTreeMap<String, (serde_json::Value, &'static str)> = BTreeMap::new();
    let mut read = Vec::new();
    for dir in dirs {
        let found = [
            dir.join(manifest::FILE),
            dir.join("..").join(manifest::FILE),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let Some(path) = found else { continue };
        let text =
            std::fs::read_to_string(&path).map_err(|e| crate::error::PipesError::io(&path, e))?;
        let m = manifest::parse(&text, &path.display().to_string())?;
        for p in &m.parameters {
            let Some(v) = &p.default else {
                return Err(crate::error::PipesError::Usage(format!(
                    "{}: parameter {} has no default, and these maps are run without \
                     `import run`, which is where a value is given (--param {}=VALUE)",
                    path.display(),
                    p.name,
                    p.name
                )));
            };
            values
                .entry(p.name.clone())
                .or_insert((v.json(), v.kind().name()));
        }
        if !m.parameters.is_empty() {
            read.push(path);
        }
    }
    Ok((
        values.into_iter().map(|(n, (v, t))| (n, v, t)).collect(),
        read,
    ))
}

fn threads(n: Option<usize>) -> usize {
    n.unwrap_or_else(crate::work::default_threads).max(1)
}

/// duckdb runs the views' Libraries and the data tests.
const DUCKDB: &str = "duckdb";

fn require_duckdb(plan: &Plan) -> Result<()> {
    if plan.runs_libraries() {
        crate::warehouse::tools::require(&[(DUCKDB, "the library steps")])?;
    }
    Ok(())
}

/// Every record file is read by a step.
fn check_files(plan: &Plan, files: &BTreeMap<String, PathBuf>) -> Result<()> {
    let read = plan.record_types();
    for (t, path) in files {
        if !read.contains(t.as_str()) {
            return Err(usage(anyhow!(
                "{}: no step reads record type {t}",
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn check(args: &CheckArgs) -> Result<()> {
    let mut package = Package::load(&args.package)?;
    package.set_values(&[Given::Text(&args.params)], false)?;
    let (mut errors, mut warnings) = package.maps.check();
    errors.extend(package.record_view_problems());
    warnings.extend(package.record_view_warnings());
    let (e, w) = package.parameter_findings();
    errors.extend(e);
    warnings.extend(w);
    for line in &warnings {
        println!("warning: {line}");
    }
    for line in &errors {
        println!("error: {line}");
    }
    let record_views: Vec<String> = package
        .record_views()
        .map(|v| format!("{} ({})", v.name, v.resource))
        .collect();
    if !record_views.is_empty() {
        println!(
            "record views, which read record files directly, beyond the SQL on FHIR \
             specification: {}",
            record_views.join(", ")
        );
    }
    if !package.parameters.is_empty() {
        println!("parameters:");
        for p in &package.parameters {
            match package.values.get(&p.name) {
                Some(v) => println!("  {} = {}", p.name, v.json()),
                None => println!(
                    "  {} has no value; a run must give --param {}=VALUE",
                    p.name, p.name
                ),
            }
        }
    }
    println!(
        "{}: {} maps, {} logical models: {} errors, {} warnings",
        package.name,
        package.maps.map_count(),
        package.models.types().count(),
        errors.len(),
        warnings.len()
    );
    let files = args.input.as_deref().map(records::scan).transpose()?;
    let plan = Plan::new(&package, files.as_ref())?;
    let mut problems = 0usize;
    if let Some(files) = &files {
        let read = plan.record_types();
        let mut records_read = 0usize;
        for (t, path) in files {
            if !read.contains(t.as_str()) {
                println!("{}: no step reads record type {t}", path.display());
                problems += 1;
                continue;
            }
            let (n, found, ignored) = check_file(&package, t, path);
            records_read += n;
            if !ignored.is_empty() {
                println!("{}", ignored_line(path, &ignored));
            }
            for line in found.iter().take(PROBLEMS_PER_FILE) {
                println!("{line}");
            }
            if found.len() > PROBLEMS_PER_FILE {
                println!(
                    "{}: {} more problems",
                    path.display(),
                    found.len() - PROBLEMS_PER_FILE
                );
            }
            problems += found.len();
        }
        println!(
            "{records_read} records in {} files: {problems} problems",
            files.len()
        );
    }
    if errors.is_empty() && problems == 0 {
        Ok(())
    } else {
        Err(Findings.into())
    }
}

/// The records read, the problems as `file:line: message`, and with
/// `[records] unknown = "ignore"` how many records had each member the
/// model does not describe.
fn check_file(
    package: &Package,
    t: &str,
    path: &Path,
) -> (usize, Vec<String>, BTreeMap<String, usize>) {
    let mut found = Vec::new();
    let mut ignored: BTreeMap<String, usize> = BTreeMap::new();
    let mut n = 0;
    let records = match records::Records::open(path, t) {
        Ok(r) => r,
        Err(e) => return (0, vec![e.to_string()], ignored),
    };
    let rules = package.record_rules();
    for record in records {
        match record {
            Ok((line, mut r)) => {
                n += 1;
                let left_out: BTreeSet<String> = package
                    .models
                    .prepare(t, &mut r, rules)
                    .into_iter()
                    .collect();
                for p in left_out {
                    *ignored.entry(p).or_default() += 1;
                }
                for p in package.models.check(t, &r) {
                    found.push(format!("{}:{line}: {p}", path.display()));
                }
            }
            Err(e) => found.push(e.to_string()),
        }
    }
    (n, found, ignored)
}

/// `file: ignored N members the model does not describe: a (in 3 records), ...`,
/// naming the first `IGNORED_PER_FILE`.
fn ignored_line(path: &Path, ignored: &BTreeMap<String, usize>) -> String {
    let mut named: Vec<String> = ignored
        .iter()
        .take(IGNORED_PER_FILE)
        .map(|(p, n)| format!("{p} ({n})"))
        .collect();
    if ignored.len() > IGNORED_PER_FILE {
        named.push(format!("and {} more", ignored.len() - IGNORED_PER_FILE));
    }
    format!(
        "{}: ignored {} members the model does not describe (records with each): {}",
        path.display(),
        ignored.len(),
        named.join(", ")
    )
}

/// Where a run writes its intermediate files: a temporary folder removed
/// when dropped, or the folder `--keep-work` names.
enum Work {
    Temp(tempfile::TempDir),
    Kept(PathBuf),
}

impl Work {
    /// A new temporary folder in `near`, or `keep`, which must be empty or
    /// not exist. Either is readable by its owner only.
    fn new(keep: Option<&Path>, near: &Path) -> Result<Self> {
        match keep {
            None => Ok(Work::Temp(
                tempfile::Builder::new()
                    .prefix(".fhir-pipes-import-")
                    .tempdir_in(near)
                    .with_context(|| format!("creating a work folder in {}", near.display()))?,
            )),
            Some(dir) => {
                if dir.exists() && !is_empty(dir)? {
                    return Err(usage(anyhow!(
                        "--keep-work {}: not empty; give an empty or new folder",
                        dir.display()
                    )));
                }
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                        .with_context(|| format!("setting the mode of {}", dir.display()))?;
                }
                Ok(Work::Kept(dir.to_path_buf()))
            }
        }
    }

    fn path(&self) -> &Path {
        match self {
            Work::Temp(t) => t.path(),
            Work::Kept(p) => p,
        }
    }
}

fn is_empty(dir: &Path) -> Result<bool> {
    if !dir.is_dir() {
        return Err(usage(anyhow!("{} is not a folder", dir.display())));
    }
    Ok(std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .next()
        .is_none())
}

/// The folder `path` is in: `.` for a bare name.
fn parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

pub fn run_command(args: &RunArgs) -> Result<()> {
    let mut package = Package::load(&args.package)?;
    package.set_values(&[Given::Text(&args.params)], true)?;
    let files = records::scan(&args.input)?;
    let plan = Plan::new(&package, Some(&files))?;
    check_files(&plan, &files)?;
    // First, so --previous given as --out without --overwrite is named as such.
    check_comparison(args)?;
    check_folder("--out", &args.out, args.overwrite)?;
    require_duckdb(&plan)?;
    // Read before anything is written: --previous may be --out.
    let mut previous = match &args.previous {
        Some(dir) => {
            let ignore = package.manifest.as_ref().map_or(&[][..], |m| &m.ignore);
            let p = Previous::load(dir, Ignores::new(ignore))?;
            eprintln!("{}: {} previous resources", dir.display(), p.len());
            Some(p)
        }
        None => None,
    };
    crate::stop::install();
    let near = parent(&args.out);
    std::fs::create_dir_all(&near).with_context(|| format!("creating {}", near.display()))?;
    let work = Work::new(args.keep_work.as_deref(), &near)?;
    let options = Options {
        files: &files,
        work: work.path(),
        threads: threads(args.threads),
        duckdb: DUCKDB,
        echo: true,
        require_ids: previous.is_some(),
    };
    let mut report = Report::default();
    let mut result = run::run(&package, &plan, &options, &mut report);
    let mut compared = None;
    if result.is_ok()
        && report.status == "ok"
        && let Some(prev) = previous.as_mut()
    {
        let types: Vec<&String> = report.output.keys().collect();
        let changed = args.changed.as_ref().map(|_| work.path().join("changed"));
        match previous::compare(prev, &work.path().join("out"), &types, changed.as_deref()) {
            Ok(c) => {
                report.comparison = Some(c.comparison);
                compared = Some((c.changed, c.removed));
            }
            Err(e) => result = Err(e),
        }
    }
    if let Err(e) = &result {
        report.status = "error".into();
        report.error = Some(crate::error::message(e));
    }
    let failed = report.tests.iter().filter(|t| !t.passed).count();
    let published = match (&result, report.status.as_str()) {
        (Ok(()), "ok") => Some(publish_all(work.path(), args, &report, compared.as_ref())),
        _ => None,
    };
    if let Some(Err(e)) = &published {
        report.status = "error".into();
        report.error = Some(crate::error::message(e));
    }
    if let Some(path) = &args.report {
        crate::report::write_json(path, &serde_json::to_value(&report)?)?;
    }
    if let Work::Kept(dir) = &work {
        println!("work files kept in {}", dir.display());
    }
    result?;
    if failed > 0 {
        eprintln!(
            "fhir-pipes: {failed} of {} data tests failed; nothing was written to {}",
            report.tests.len(),
            args.out.display()
        );
        return Err(Findings.into());
    }
    published.expect("published when the run passed")?;
    let total: usize = report.output.values().sum();
    let warnings: usize = report.steps.iter().filter_map(|s| s.warnings).sum();
    println!(
        "Wrote {total} resources ({}) to {}",
        run::describe(&report.output),
        args.out.display()
    );
    if let (Some(c), Some((changed, removed))) = (&report.comparison, &compared) {
        println!("Compared with {}: {}", c.previous, c.total.describe());
        for (t, counts) in &c.types {
            println!("  {t}: {}", counts.describe());
        }
        if let Some(dir) = &args.changed {
            println!(
                "Wrote {} new or changed resources ({}) to {}",
                changed.values().sum::<usize>(),
                run::describe(changed),
                dir.display()
            );
        }
        if let Some(path) = &args.removed {
            println!("Wrote {} removed ids to {}", removed.len(), path.display());
        }
    }
    if warnings > 0 {
        println!("{warnings} engine warnings");
    }
    Ok(())
}

/// An output folder (`--out`, `--changed`) must be a folder, and empty or
/// new unless `--overwrite`.
fn check_folder(flag: &str, dir: &Path, overwrite: bool) -> Result<()> {
    if dir.exists() && !dir.is_dir() {
        return Err(usage(anyhow!("{flag} {}: not a folder", dir.display())));
    }
    if dir.exists() && !overwrite && !is_empty(dir)? {
        return Err(usage(anyhow!(
            "{flag} {}: not empty; give an empty or new folder, or --overwrite to replace \
             the files an earlier run wrote there",
            dir.display()
        )));
    }
    Ok(())
}

/// The checks of `--previous`, `--changed` and `--removed`.
fn check_comparison(args: &RunArgs) -> Result<()> {
    let Some(prev) = &args.previous else {
        return Ok(());
    };
    if !prev.exists() {
        return Err(usage(anyhow!(
            "--previous {}: no such folder; give the --out folder of an earlier run",
            prev.display()
        )));
    }
    if !prev.is_dir() {
        return Err(usage(anyhow!(
            "--previous {}: not a folder",
            prev.display()
        )));
    }
    if same_place(prev, &args.out) && !args.overwrite {
        return Err(usage(anyhow!(
            "--previous {}: is the --out folder; give --overwrite to replace the previous \
             run's files there with this run's (the previous ones are read first)",
            prev.display()
        )));
    }
    if let Some(changed) = &args.changed {
        for (flag, other) in [("--out", &args.out), ("--previous", prev)] {
            if same_place(changed, other) {
                return Err(usage(anyhow!(
                    "--changed {}: is the {flag} folder; give --changed a folder of its own",
                    changed.display()
                )));
            }
        }
        check_folder("--changed", changed, args.overwrite)?;
    }
    if let Some(removed) = &args.removed
        && removed.is_dir()
    {
        return Err(usage(anyhow!(
            "--removed {}: is a folder; give a file",
            removed.display()
        )));
    }
    Ok(())
}

/// Whether `a` and `b` name one place: the same path once resolved, or for
/// a path that does not exist yet, the same name in the same resolved
/// folder.
fn same_place(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        p.canonicalize()
            .ok()
            .or_else(|| Some(parent(p).canonicalize().ok()?.join(p.file_name()?)))
            .unwrap_or_else(|| p.to_path_buf())
    };
    key(a) == key(b)
}

/// Puts the run's files in place: `--changed` and `--removed` first, then
/// `--out`. Should a later one fail, `--out` still holds the previous run,
/// so the next run finds the same changes again rather than miss them.
fn publish_all(
    work: &Path,
    args: &RunArgs,
    report: &Report,
    compared: Option<&(BTreeMap<String, usize>, Vec<String>)>,
) -> Result<()> {
    if let Some((changed, removed)) = compared {
        if let Some(dir) = &args.changed {
            publish(&work.join("changed"), dir, changed.keys(), &report.package)?;
        }
        if let Some(path) = &args.removed {
            previous::write_lines(path, removed)?;
        }
    }
    publish(
        &work.join("out"),
        &args.out,
        report.output.keys(),
        &report.package,
    )
}

/// Moves the `<type>.ndjson` files of `types` from `from` into `out`,
/// replacing files of the same name, and removes the files the last run
/// listed in `out/.fhir-pipes-import.json` that this run did not write.
/// Nothing else in `out` is touched.
fn publish<'a>(
    from: &Path,
    out: &Path,
    types: impl Iterator<Item = &'a String>,
    package: &str,
) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let list = out.join(WRITTEN);
    let before: BTreeSet<String> = match std::fs::read_to_string(&list) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v["files"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default(),
        Err(_) => BTreeSet::new(),
    };
    let mut written = BTreeSet::new();
    for t in types {
        let name = format!("{t}.ndjson");
        let (src, dest) = (from.join(&name), out.join(&name));
        if std::fs::rename(&src, &dest).is_err() {
            // Another file system: copy (mode and all), then remove.
            std::fs::copy(&src, &dest).with_context(|| format!("writing {}", dest.display()))?;
            let _ = std::fs::remove_file(&src);
        }
        written.insert(name);
    }
    for stale in before.difference(&written) {
        let is_ours = stale.ends_with(".ndjson") && !stale.contains(['/', '\\']);
        if is_ours {
            let _ = std::fs::remove_file(out.join(stale));
        }
    }
    let record = json!({"package": package, "files": written});
    crate::fs::atomic_write(&list, serde_json::to_string_pretty(&record)? + "\n")?;
    Ok(())
}

pub fn test_command(args: &TestArgs) -> Result<()> {
    let mut package = Package::load(&args.package)?;
    let tests = package.dir.join("tests");
    let mut cases: Vec<(String, PathBuf)> = if tests.is_dir() {
        crate::fs::sorted_entries(&tests)
            .with_context(|| format!("reading {}", tests.display()))?
            .into_iter()
            .filter(|p| p.join("in").is_dir())
            .filter_map(|p| Some((p.file_name()?.to_str()?.to_string(), p)))
            .collect()
    } else {
        Vec::new()
    };
    if cases.is_empty() {
        return Err(usage(anyhow!(
            "{}: no test cases; a case is a folder tests/<case>/ with in/ and expected/",
            package.dir.display()
        )));
    }
    if let Some(name) = &args.case {
        let names: Vec<String> = cases.iter().map(|(n, _)| n.clone()).collect();
        cases.retain(|(n, _)| n == name);
        if cases.is_empty() {
            return Err(usage(anyhow!(
                "no test case {name}; the cases are: {}",
                names.join(", ")
            )));
        }
    }
    let mut failed = 0;
    for (name, dir) in &cases {
        let case_file = dir.join(CASE_PARAMETERS);
        let case_values = if case_file.is_file() {
            let text = std::fs::read_to_string(&case_file)
                .with_context(|| format!("reading {}", case_file.display()))?;
            toml::from_str::<toml::Table>(&text)
                .map_err(|e| usage(anyhow!("{}: {e}", case_file.display())))?
        } else {
            toml::Table::new()
        };
        let shown = case_file.display().to_string();
        package.set_values(
            &[Given::Text(&args.params), Given::Toml(&case_values, &shown)],
            true,
        )?;
        let package = &package;
        let files = records::scan(&dir.join("in"))?;
        let plan = Plan::new(package, Some(&files))?;
        check_files(&plan, &files)?;
        require_duckdb(&plan)?;
        let work = tempfile::tempdir().context("creating a work folder")?;
        let options = Options {
            files: &files,
            work: work.path(),
            threads: threads(args.threads),
            duckdb: DUCKDB,
            echo: false,
            require_ids: false,
        };
        let mut report = Report::default();
        let result = run::run(package, &plan, &options, &mut report);
        let out = work.path().join("out");
        let expected = dir.join("expected");
        let problems: Vec<String> = match result {
            Err(e) => vec![format!("the run failed: {}", crate::error::message(&e))],
            Ok(()) if report.status == "failed" => report
                .tests
                .iter()
                .filter(|t| !t.passed)
                .map(|t| {
                    format!(
                        "data test {} ({}) failed: {} rows",
                        t.name, t.check, t.failures
                    )
                })
                .collect(),
            Ok(()) if args.update => {
                update(&out, &expected)?;
                println!(
                    "case {name}: wrote {} ({})",
                    expected.display(),
                    run::describe(&report.output)
                );
                continue;
            }
            Ok(()) => compare::compare(&out, &expected)?,
        };
        if problems.is_empty() {
            let total: usize = report.output.values().sum();
            println!(
                "case {name}: passed ({total} resources: {})",
                run::describe(&report.output)
            );
        } else {
            failed += 1;
            println!("case {name}: FAILED");
            for p in problems {
                println!("  {p}");
            }
        }
    }
    if args.update {
        println!(
            "{} cases: {} updated, {failed} failed",
            cases.len(),
            cases.len() - failed
        );
    } else {
        println!(
            "{} cases: {} passed, {failed} failed",
            cases.len(),
            cases.len() - failed
        );
    }
    if failed > 0 {
        Err(Findings.into())
    } else {
        Ok(())
    }
}

/// Replaces the `*.ndjson` files in `expected` with those in `out`.
fn update(out: &Path, expected: &Path) -> Result<()> {
    std::fs::create_dir_all(expected)
        .with_context(|| format!("creating {}", expected.display()))?;
    for old in compare::ndjson_files(expected)? {
        std::fs::remove_file(expected.join(&old))
            .with_context(|| format!("removing {}", expected.join(&old).display()))?;
    }
    // Written as new files, not copied, so they get the usual mode for files
    // kept in a repository rather than the work files' owner-only one.
    for name in compare::ndjson_files(out)? {
        let bytes = std::fs::read(out.join(&name))
            .with_context(|| format!("reading {}", out.join(&name).display()))?;
        std::fs::write(expected.join(&name), bytes)
            .with_context(|| format!("writing {}", expected.join(&name).display()))?;
    }
    Ok(())
}
