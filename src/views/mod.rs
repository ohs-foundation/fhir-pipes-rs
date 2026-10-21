//! `fhir-pipes views`: run, check and test SQL-on-FHIR ViewDefinitions
//! directly, without a warehouse. Ported from the octofhir-sof command-line
//! tool; the work is done by fhir-pipes-sof and fhir-pipes-sof-lint.
//!
//!   run       run views over FHIR files (or PostgreSQL) and write the rows
//!   sql       generate the SQL for a view, or a CREATE TABLE for its columns
//!   validate  check a view's structure against the spec, offline
//!   lint      check a view's FHIRPath against the FHIR core types, a FHIR package, or the
//!             shareable subset
//!   test      run SQL-on-FHIR test-case files

mod diagnostic;
pub mod run;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fhir_pipes_sof::output::{
    NdjsonWriter, OutputWriter, ParquetWriter, TemporalEncoding, WriteSummary, get_writer,
};
use fhir_pipes_sof::{SqlGenerator, ViewDefinition, ViewResult};
use fhir_pipes_sof_lint::{Finding, Severity, lint_core, lint_shareable, validate_structure};

use crate::error::{Findings, usage};
use crate::fhir::chunks;
use crate::fhir::input::{
    for_each_chunk, is_ndjson, parse_resource_text, push_resource, resource_files,
};
use crate::fhir::ndjson::{Output, Splitter, is_gz, read_text};
use run::{ViewRows, discover, parse_view};

#[derive(clap::Args, Debug)]
pub struct ViewsArgs {
    #[command(subcommand)]
    command: ViewsCommand,
    /// Disable coloured output (also honours the NO_COLOR environment variable).
    #[arg(long, global = true)]
    no_color: bool,
}

#[derive(clap::Subcommand, Debug)]
enum ViewsCommand {
    /// Run a ViewDefinition, or a directory of them, and write the rows. Runs
    /// against FHIR files with --input (no database) or against PostgreSQL
    /// with --db.
    Run {
        /// A ViewDefinition JSON file, or a directory of them (such as an IG
        /// build's fsh-generated/resources). A directory runs every view in one
        /// pass, writing one file per view into the --out directory.
        view: PathBuf,
        /// FHIR resources to run against, with no database: an NDJSON file, a
        /// Bundle, a JSON resource or array, a directory of such files
        /// (subdirectories included), or `-` to read from stdin. A file ending
        /// in `.gz` is read as gzip. `urn:uuid` references within a Bundle are
        /// resolved to Type/id.
        #[arg(long, conflicts_with = "db")]
        input: Option<PathBuf>,
        /// PostgreSQL connection URL, for a database holding FHIR resources as
        /// JSONB.
        #[arg(long, env = "FHIR_PIPES_DATABASE_URL")]
        db: Option<String>,
        /// Output format: csv, ndjson, json or parquet.
        #[arg(long, default_value = "csv")]
        output: String,
        /// Write to this file (a directory for a directory of views) instead
        /// of stdout. A file ending in `.gz` is written as gzip, except for
        /// Parquet, which is refused.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Parquet only: how date, dateTime, instant and time columns are
        /// encoded. `text` keeps the FHIR string (the spec default); `native`
        /// writes Arrow Date32 / Timestamp(µs, UTC) / Time64(µs), flooring
        /// partial dates (`2026`, `2026-03`) to the start of the period and
        /// writing a dateTime in a date column as its date part as written.
        #[arg(long, default_value = "text", value_parser = ["text", "native"])]
        parquet_temporal: String,
        /// Threads for parsing --input and evaluating views on it (default:
        /// the number of CPUs). Rows keep the input order whatever the count.
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Generate SQL from a ViewDefinition, offline. Emits a PostgreSQL SELECT
    /// over FHIR JSONB by default; --dialect duckdb emits DuckDB SQL. With
    /// --ddl, emits a CREATE TABLE for the view's output columns instead.
    Sql {
        /// The ViewDefinition JSON file.
        view: PathBuf,
        /// Emit a CREATE TABLE statement for the output columns instead of the
        /// SELECT query.
        #[arg(long)]
        ddl: bool,
        /// SQL dialect. For --ddl: ansi (the default, spec types), postgres or
        /// duckdb. For the SELECT: postgres (the default) or duckdb; ansi is
        /// treated as postgres.
        #[arg(long, default_value = "ansi")]
        dialect: String,
        /// Table name for --ddl (default: the view's name, or the resource
        /// name with `_view`).
        #[arg(long)]
        table: Option<String>,
    },
    /// Check a ViewDefinition's structure against the spec, offline: SQL-safe
    /// names, one iteration construct per select, unique column names,
    /// consistent unionAll branches. Exits non-zero on an error.
    Validate {
        /// The ViewDefinition JSON file.
        view: PathBuf,
        /// Emit findings as JSON instead of a report.
        #[arg(long)]
        json: bool,
        /// Emit findings as SARIF 2.1.0, for CI code scanning.
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
    },
    /// Check a ViewDefinition's FHIRPath selectors against the FHIR R4 and R5
    /// core types built in (FH01–FH05), or against a FHIR package with
    /// --package (FH01–FH10); with --shareable, also against the
    /// ShareableViewDefinition subset (FH11). Exits non-zero on an error.
    Lint {
        /// The ViewDefinition JSON file.
        view: PathBuf,
        /// FHIR package to check against in place of the built-in core
        /// types: a package.tgz (path or https URL), an unpacked package or
        /// directory of JSON resources, name#version in the local FHIR
        /// package cache (~/.fhir/packages), or a name with --version.
        #[arg(long)]
        package: Option<String>,
        /// Version of the package --package names (e.g. hl7.fhir.r4.core
        /// with 4.0.1): taken from the local FHIR package cache, else
        /// downloaded from the FHIR package registry (packages.fhir.org).
        #[arg(long)]
        version: Option<String>,
        /// Flag any FHIRPath function, operator or literal outside the
        /// portable subset that every SQL-on-FHIR engine must support.
        #[arg(long)]
        shareable: bool,
        /// Exempt an engine-specific FHIRPath function from the --shareable
        /// check. Repeatable.
        #[arg(long = "allow-fn", value_name = "NAME")]
        allow_fn: Vec<String>,
        /// Emit findings as JSON instead of a report.
        #[arg(long)]
        json: bool,
        /// Emit findings as SARIF 2.1.0, for CI code scanning.
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
    },
    /// Run SQL-on-FHIR test-case files (the official content-test format:
    /// `{resources, tests: [{title, view, expect | expectCount | expectColumns
    /// | expectError}]}`) in memory and report each case. Exits non-zero on any
    /// failure.
    Test {
        /// A test-case JSON file, or a directory of them.
        manifest: PathBuf,
    },
}

pub fn run(args: &ViewsArgs) -> Result<()> {
    let color = diagnostic::use_color(args.no_color);
    // Package loading and PostgreSQL are async; view evaluation on files runs
    // on threads of its own inside the runtime.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(run_command(&args.command, color))
}

/// Read one ViewDefinition file, returning its text (for diagnostic spans)
/// and the parsed view.
/// A view that cannot be read or parsed is a usage error.
fn read_view(path: &Path) -> Result<(String, ViewDefinition)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))
        .map_err(usage)?;
    let (_, view) = parse_view(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))
        .map_err(usage)?
        .with_context(|| format!("{} is not a ViewDefinition", path.display()))
        .map_err(usage)?;
    Ok((text, view))
}

/// One view, or every view in a directory keyed by its name. A directory is
/// read as `refresh` reads it: non-view JSON is skipped, an IG build's form is
/// accepted, and a view with a structural error stops the run.
fn load_views(path: &Path) -> Result<Vec<(String, ViewDefinition)>> {
    if path.is_dir() {
        let views = discover(path, &[]).map_err(usage)?;
        if views.is_empty() {
            return Err(usage(anyhow::anyhow!(
                "no ViewDefinitions in {}",
                path.display()
            )));
        }
        Ok(views.into_iter().map(|v| (v.name, v.definition)).collect())
    } else {
        let (_, view) = read_view(path)?;
        let name = view
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "view".to_string());
        Ok(vec![(name, view)])
    }
}

/// Where `run` gets each view's rows from.
enum Source {
    /// Every view's rows, already run over the files, in view order.
    Files(Vec<Option<ViewResult>>),
    #[cfg(feature = "postgres")]
    Db(sqlx_postgres::PgPool),
}

#[cfg(feature = "postgres")]
async fn connect(db: &str) -> Result<Source> {
    crate::http::ensure_crypto_provider();
    let pool = sqlx_postgres::PgPool::connect(db)
        .await
        .context("connecting to the database")?;
    Ok(Source::Db(pool))
}

#[cfg(not(feature = "postgres"))]
async fn connect(_db: &str) -> Result<Source> {
    Err(usage(anyhow::anyhow!(
        "--db needs PostgreSQL support, which this fhir-pipes was built without; \
         build it with `--features postgres`"
    )))
}

/// Findings from checking `view`'s selectors and generated SQL against a
/// FHIR package (FH01–FH10). The package is read as every package is
/// (`fhir::package`); a name with `version` is `name#version` in the local
/// FHIR package cache, or downloaded from the FHIR package registry.
#[cfg(feature = "fhir-packages")]
async fn package_findings(
    view: &ViewDefinition,
    package: &str,
    version: Option<&str>,
) -> Result<Vec<Finding>> {
    let spec = match version {
        Some(v) => crate::fhir::package::by_version(package, v),
        None => package.to_string(),
    };
    // Reading a package may download it, with a blocking client.
    let mut index = tokio::task::block_in_place(|| {
        crate::fhir::package::Index::load(
            std::slice::from_ref(&spec),
            "--package",
            &["StructureDefinition"],
        )
    })?;
    let provider = fhir_pipes_sof_lint::FhirSchemaProvider::from_structure_definitions(
        index
            .take_type("StructureDefinition")
            .into_values()
            .flatten(),
    );
    if provider.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "--package {spec} has no StructureDefinitions of resources"
        )));
    }
    Ok(fhir_pipes_sof_lint::lint(view, &provider))
}

#[cfg(not(feature = "fhir-packages"))]
async fn package_findings(
    _view: &ViewDefinition,
    _package: &str,
    _version: Option<&str>,
) -> Result<Vec<Finding>> {
    Err(usage(anyhow::anyhow!(
        "--package needs FHIR package support, which this fhir-pipes was built without; \
         build it with `--features fhir-packages` (--shareable works without it)"
    )))
}

/// View `i`'s rows: from the run over the files, or from the database.
#[cfg_attr(not(feature = "postgres"), allow(unused_variables))]
async fn view_rows(source: &mut Source, i: usize, view: &ViewDefinition) -> Result<ViewResult> {
    match source {
        Source::Files(results) => Ok(results[i].take().expect("each view's rows are taken once")),
        #[cfg(feature = "postgres")]
        Source::Db(pool) => fhir_pipes_sof::ViewRunner::new(pool.clone())
            .run(view)
            .await
            .context("running view"),
    }
}

fn output_extension(format: &str) -> &'static str {
    match format.to_lowercase().as_str() {
        "ndjson" => "ndjson",
        "json" => "json",
        "parquet" => "parquet",
        _ => "csv",
    }
}

/// The writer for `format`. --parquet-temporal is accepted for every format
/// so scripts can pass it unconditionally, but only affects Parquet.
pub(crate) fn select_writer(format: &str, parquet_temporal: &str) -> Result<Box<dyn OutputWriter>> {
    if format.eq_ignore_ascii_case("parquet") {
        let temporal = TemporalEncoding::parse(parquet_temporal)
            .context("parsing --parquet-temporal")
            .map_err(usage)?;
        return Ok(Box::new(ParquetWriter::new().with_temporal(temporal)));
    }
    get_writer(format)
        .context("choosing the output format")
        .map_err(usage)
}

/// Print, on stderr, the values each view's writer could not write as they
/// are: `view.column: 3 values did not fit integer and were written as null`.
fn report_mismatches(views: &[(&str, WriteSummary)]) {
    let lines: Vec<String> = views
        .iter()
        .flat_map(|(view, s)| s.messages().into_iter().map(move |l| format!("{view}.{l}")))
        .collect();
    if lines.is_empty() {
        return;
    }
    eprintln!("values that did not match their column's type:");
    for l in lines {
        eprintln!("   {l}");
    }
}

/// The default --ddl table name: the view's name, else `<resource>_view`.
fn default_table_name(view: &ViewDefinition) -> String {
    view.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_view", view.resource.to_lowercase()))
}

/// Run `views` over the FHIR resources in a file, a directory of files
/// (searched recursively), or `-` for stdin, or over `given` (stdin, already
/// read). NDJSON (`.ndjson`), single resources, arrays of resources and
/// Bundles are read, an NDJSON file a chunk of lines at a time. Resources of
/// a type no view reads are dropped as they are read. Returns each view's
/// rows, in view order, the rows in the order of the resources.
fn run_on_files(
    views: &[(String, ViewDefinition)],
    path: &Path,
    given: Option<Vec<serde_json::Value>>,
    threads: usize,
) -> Result<Vec<Option<ViewResult>>> {
    let wanted: HashSet<&str> = views.iter().map(|(_, v)| v.resource.as_str()).collect();
    let keep_bundles = wanted.contains("Bundle");
    let mut rows = ViewRows::new(views.iter().map(|(n, v)| (n.as_str(), v)).collect());
    let mut add = |resources: Vec<serde_json::Value>| -> Result<()> {
        let mut by_type: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for r in resources {
            if let Some(t) = r.get("resourceType").and_then(|t| t.as_str())
                && wanted.contains(t)
            {
                by_type.entry(t.to_string()).or_default().push(r);
            }
        }
        for (t, resources) in by_type {
            rows.add_typed(&t, &resources, threads)?;
        }
        Ok(())
    };
    let count = if let Some(resources) = given {
        let n = resources.len();
        add(resources)?;
        n
    } else if path.as_os_str() == "-" {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text)
            .context("reading FHIR resources from stdin")?;
        let mut resources = Vec::new();
        parse_resource_text(&text, &mut resources, keep_bundles)
            .context("parsing stdin")
            .map_err(usage)?;
        if resources.is_empty() {
            return Err(usage(anyhow::anyhow!("no FHIR resources on stdin")));
        }
        let n = resources.len();
        add(resources)?;
        n
    } else {
        // An --input that is not there is a usage error; its message is the
        // read's.
        resource_files(path)
            .and_then(|files| for_each_chunk(&files, threads, keep_bundles, &mut add))
            .map_err(|e| if path.exists() { e } else { usage(e) })?
    };
    if count == 0 {
        return Err(usage(anyhow::anyhow!(
            "no FHIR resources in {}",
            path.display()
        )));
    }
    Ok(rows.finish(threads)?.into_iter().map(Some).collect())
}

/// The next non-blank line of `lines` and its 1-based number. `name` is the
/// input's name in messages.
fn next_line(
    lines: &mut Splitter<Box<dyn io::BufRead>>,
    name: &str,
) -> Result<Option<(usize, String)>> {
    let mut buf = Vec::new();
    let Some(span) = lines.next_into(&mut buf).map_err(|e| {
        chunks::read_error(e, format!("reading {name} line {}", lines.lines_read() + 1))
    })?
    else {
        return Ok(None);
    };
    buf.truncate(span.end);
    buf.drain(..span.start);
    let line = String::from_utf8(buf).map_err(|e| {
        let e = io::Error::new(io::ErrorKind::InvalidData, e.utf8_error());
        chunks::read_error(e, format!("reading {name} line {}", span.number))
    })?;
    Ok(Some((span.number, line)))
}

/// Stream NDJSON through one view, writing NDJSON rows as they are produced,
/// without holding the input or the output in memory. `first` is a line the
/// caller already read from `reader`. Returns the number of rows written.
async fn stream_ndjson(
    view: &ViewDefinition,
    first: Option<(usize, String)>,
    mut lines: Splitter<Box<dyn io::BufRead>>,
    name: &str,
    sink: &mut dyn Write,
) -> Result<usize> {
    let compiled = fhir_pipes_sof::CompiledView::compile(view)
        .await
        .context("compiling view")
        .map_err(usage)?;
    let mut total = 0;
    let mut next = first;
    while let Some((number, line)) = next {
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parsing {name} line {number}"))
            .map_err(usage)?;
        // A Bundle or array on one line is expanded into its resources.
        let mut resources = Vec::new();
        push_resource(value, &mut resources, view.resource == "Bundle");
        for resource in &resources {
            for row in compiled.execute_resource(resource).await? {
                let obj = NdjsonWriter::row_to_object(compiled.columns(), row);
                writeln!(sink, "{obj}").context("writing NDJSON row")?;
                total += 1;
            }
        }
        next = next_line(&mut lines, name)?;
    }
    Ok(total)
}

/// Refuse an --out that is the --input file: the output is opened for
/// writing before (or while) the input is read, which would empty it.
fn check_out_is_not_input(input: Option<&PathBuf>, out: Option<&PathBuf>) -> Result<()> {
    let (Some(input), Some(out)) = (input, out) else {
        return Ok(());
    };
    if input.as_os_str() == "-" || !out.exists() {
        return Ok(());
    }
    let same = match (input.canonicalize(), out.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        return Err(usage(anyhow::anyhow!(
            "--out {} is the --input file; writing it would empty the input",
            out.display()
        )));
    }
    Ok(())
}

/// The file `out` names, written as gzip when its name ends in `.gz`, or
/// stdout. `close_sink` finishes it.
pub(crate) fn open_sink(out: Option<&PathBuf>) -> Result<Output<Box<dyn Write>>> {
    Ok(match out {
        Some(path) => Output::new(
            Box::new(
                fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
            ),
            is_gz(path),
        ),
        None => Output::new(Box::new(io::stdout().lock()), false),
    })
}

/// Finish writing what `open_sink` opened: the end of a gzip stream, then
/// a flush. Both are buffered, so this is where most of a small output is
/// written, and an error here (a full disk, a closed pipe) fails the run.
pub(crate) fn close_sink(sink: Output<Box<dyn Write>>, out: Option<&PathBuf>) -> Result<()> {
    sink.finish().map(drop).with_context(|| match out {
        Some(path) => format!("writing {}", path.display()),
        None => "writing to stdout".to_string(),
    })
}

/// Run one test-case file in memory, printing each case. Returns
/// `(passed, total)`.
fn run_test_file(path: &Path, color: bool) -> Result<(usize, usize)> {
    let doc: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
            .map_err(usage)?,
    )
    .with_context(|| format!("parsing {}", path.display()))
    .map_err(usage)?;

    let resources: Vec<serde_json::Value> = doc
        .get("resources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let empty = Vec::new();
    let tests = doc
        .get("tests")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (mut pass, mut total) = (0usize, 0usize);
    for test in tests {
        total += 1;
        let title = test
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("<untitled>");
        let (ok, detail) = run_one_test(test, &resources);
        let marker = diagnostic::marker(ok, color);
        if ok {
            pass += 1;
            println!("  {marker} [{name}] {title}");
        } else {
            println!("  {marker} [{name}] {title}: {detail}");
        }
    }
    Ok((pass, total))
}

fn run_one_test(test: &serde_json::Value, resources: &[serde_json::Value]) -> (bool, String) {
    use serde_json::Value;
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let view = match ViewDefinition::from_json(&test.get("view").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v,
        Err(e) => return (expect_error, format!("parse error: {e}")),
    };
    let result = match fhir_pipes_sof::execute_blocking(&view, resources) {
        Ok(r) => r,
        Err(e) => return (expect_error, format!("execute error: {e}")),
    };
    if expect_error {
        return (false, "expected an error but execution succeeded".into());
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual = result.to_json_array();
        if multiset_eq(&actual, expect) {
            (true, String::new())
        } else {
            (false, format!("rows differ: got {}", Value::Array(actual)))
        }
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = result.data.len() as u64;
        (got == count, format!("want {count} rows, got {got}"))
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let want: Vec<&str> = cols.iter().filter_map(Value::as_str).collect();
        (got == want, format!("want columns {want:?}, got {got:?}"))
    } else {
        (true, String::new())
    }
}

/// Order-insensitive comparison of two sets of JSON rows.
fn multiset_eq(a: &[serde_json::Value], b: &[serde_json::Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(canonical_json).collect();
    let mut b: Vec<String> = b.iter().map(canonical_json).collect();
    a.sort();
    b.sort();
    a == b
}

/// JSON with object keys sorted, for comparing rows as sets.
fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{k:?}:{}", canonical_json(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// How validate and lint report findings.
#[derive(Clone, Copy, PartialEq)]
enum ReportFormat {
    Human,
    Json,
    Sarif,
}

impl ReportFormat {
    fn from_flags(json: bool, sarif: bool) -> Self {
        if sarif {
            Self::Sarif
        } else if json {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Print findings in the chosen format and return whether any is an error.
/// JSON and SARIF print a valid empty report when there are no findings.
fn report_findings(
    origin: &str,
    source: &str,
    findings: &[Finding],
    fmt: ReportFormat,
    color: bool,
    ok_msg: &str,
) -> bool {
    match fmt {
        ReportFormat::Json => println!("{}", diagnostic::render_findings_json(findings)),
        ReportFormat::Sarif => {
            println!("{}", diagnostic::render_findings_sarif(origin, findings))
        }
        ReportFormat::Human => {
            if findings.is_empty() {
                println!("{}", diagnostic::ok(ok_msg, color));
            } else {
                print!(
                    "{}",
                    diagnostic::render_findings(origin, source, findings, color)
                );
            }
        }
    }
    findings.iter().any(|f| f.severity == Severity::Error)
}

async fn run_command(command: &ViewsCommand, color: bool) -> Result<()> {
    match command {
        ViewsCommand::Sql {
            view,
            ddl,
            dialect,
            table,
        } => {
            let (_, view) = read_view(view)?;
            let dialect: fhir_pipes_sof::Dialect = dialect
                .parse()
                .map_err(|e: String| usage(anyhow::anyhow!(e)))?;
            // A view the generator cannot turn into SQL is the view's fault.
            let generated = SqlGenerator::new()
                .with_dialect(dialect)
                .generate(&view)
                .context("generating SQL")
                .map_err(usage)?;
            if *ddl {
                let table = table.clone().unwrap_or_else(|| default_table_name(&view));
                print!(
                    "{}",
                    fhir_pipes_sof::create_table(&table, &generated.columns, dialect)
                );
            } else {
                println!("{}", generated.sql);
            }
        }
        ViewsCommand::Run {
            view,
            input,
            db,
            output,
            out,
            parquet_temporal,
            threads,
        } => {
            let threads = threads.unwrap_or_else(crate::work::default_threads);
            // A directory of views runs every view in one pass, writing one
            // file per view into the --out directory.
            let multi = view.is_dir();

            if !multi {
                check_out_is_not_input(input.as_ref(), out.as_ref())?;
                // A Parquet file inside gzip is not a Parquet file any reader
                // opens, and Parquet compresses its own pages.
                if output.eq_ignore_ascii_case("parquet") && out.as_ref().is_some_and(|o| is_gz(o))
                {
                    return Err(usage(anyhow::anyhow!(
                        "--out ends in .gz, but Parquet is not written as gzip; \
                         Parquet compresses its own pages, so name the file .parquet"
                    )));
                }
            }

            // One view, NDJSON in and NDJSON out: stream it resource by
            // resource in bounded memory. A .ndjson (or .ndjson.gz) file is
            // taken at its word.
            // Stdin is streamed when its first line is one whole JSON value;
            // otherwise (formatted JSON, a Bundle or an array over several
            // lines) it is read whole, as for the other output formats.
            let mut stdin_resources: Option<Vec<serde_json::Value>> = None;
            let stdin_in = input.as_ref().is_some_and(|p| p.as_os_str() == "-");
            let ndjson_in = stdin_in || input.as_ref().is_some_and(|p| is_ndjson(p));
            if !multi && output.eq_ignore_ascii_case("ndjson") && ndjson_in {
                let (_, view_def) = read_view(view)?;
                let path = input.as_ref().expect("checked above");
                let name = if stdin_in {
                    "stdin".to_string()
                } else {
                    path.display().to_string()
                };
                let reader: Box<dyn io::BufRead> = if stdin_in {
                    Box::new(io::BufReader::new(io::stdin()))
                } else {
                    read_text(
                        fs::File::open(path)
                            .with_context(|| format!("opening {}", path.display()))
                            .map_err(|e| if path.exists() { e } else { usage(e) })?,
                        is_gz(path),
                    )
                };
                let mut lines = Splitter::new(reader);
                let first = next_line(&mut lines, &name)?;
                let one_value_per_line = !stdin_in
                    || first.as_ref().is_none_or(|(_, l)| {
                        serde_json::from_str::<serde::de::IgnoredAny>(l).is_ok()
                    });
                if one_value_per_line {
                    let mut sink = open_sink(out.as_ref())?;
                    let n = stream_ndjson(&view_def, first, lines, &name, &mut sink).await?;
                    close_sink(sink, out.as_ref())?;
                    eprintln!("{n} rows");
                    return Ok(());
                }
                // The first line, trimmed, and the rest as they are.
                let mut text = first.map(|(_, l)| l + "\n").unwrap_or_default();
                lines
                    .into_inner()
                    .read_to_string(&mut text)
                    .context("reading FHIR resources from stdin")?;
                let mut resources = Vec::new();
                parse_resource_text(&text, &mut resources, view_def.resource == "Bundle")
                    .context("parsing stdin")
                    .map_err(usage)?;
                if resources.is_empty() {
                    return Err(usage(anyhow::anyhow!("no FHIR resources on stdin")));
                }
                stdin_resources = Some(resources);
            }

            let views = load_views(view)?;
            let mut source = match (input, db) {
                // Evaluation runs on threads of its own; block_in_place lets
                // this runtime worker wait for them without stalling other
                // tasks.
                (Some(path), _) => Source::Files(tokio::task::block_in_place(|| {
                    run_on_files(&views, path, stdin_resources.take(), threads)
                })?),
                (None, Some(db)) => connect(db).await?,
                (None, None) => {
                    return Err(usage(anyhow::anyhow!(
                        "pass --input <file|dir|-> to run on files, or --db <url>"
                    )));
                }
            };
            let writer = select_writer(output, parquet_temporal)?;

            if multi {
                let out_dir = out
                    .as_ref()
                    .context("--out <dir> is needed to run a directory of views")
                    .map_err(usage)?;
                fs::create_dir_all(out_dir)
                    .with_context(|| format!("creating {}", out_dir.display()))?;
                let ext = output_extension(output);
                let mut mismatches = Vec::new();
                for (i, (name, view)) in views.iter().enumerate() {
                    let result = view_rows(&mut source, i, view).await?;
                    let path = out_dir.join(format!("{name}.{ext}"));
                    let mut sink: Box<dyn Write> = Box::new(
                        fs::File::create(&path)
                            .with_context(|| format!("creating {}", path.display()))?,
                    );
                    let summary = writer
                        .write_with_summary(&result, &mut sink)
                        .with_context(|| format!("writing {}", path.display()))?;
                    sink.flush().ok();
                    eprintln!("{name} → {} ({} rows)", path.display(), result.data.len());
                    mismatches.push((name.as_str(), summary));
                }
                report_mismatches(&mismatches);
            } else {
                let (name, view) = &views[0];
                let result = view_rows(&mut source, 0, view).await?;
                let mut sink = open_sink(out.as_ref())?;
                let summary = writer
                    .write_with_summary(&result, &mut sink)
                    .context("writing output")?;
                close_sink(sink, out.as_ref())?;
                report_mismatches(&[(name.as_str(), summary)]);
            }
        }
        ViewsCommand::Validate { view, json, sarif } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(view)?;
            let findings = validate_structure(&view);
            let fmt = ReportFormat::from_flags(*json, *sarif);
            if report_findings(&origin, &source, &findings, fmt, color, "valid") {
                return Err(Findings.into());
            }
        }
        ViewsCommand::Test { manifest } => {
            let mut files = Vec::new();
            if manifest.is_dir() {
                let mut entries = crate::fs::sorted_entries(manifest)
                    .with_context(|| format!("reading directory {}", manifest.display()))?;
                entries.retain(|p| p.extension().is_some_and(|x| x == "json"));
                files.extend(entries);
            } else {
                files.push(manifest.clone());
            }
            let (mut pass, mut total) = (0usize, 0usize);
            for file in &files {
                let (p, t) = run_test_file(file, color)?;
                pass += p;
                total += t;
            }
            let summary = format!("{pass}/{total} passed");
            if pass == total {
                println!("{}", diagnostic::ok(&summary, color));
            } else {
                println!("{summary}");
                return Err(Findings.into());
            }
        }
        ViewsCommand::Lint {
            view,
            package,
            version,
            shareable,
            allow_fn,
            json,
            sarif,
        } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(view)?;
            let mut findings = match package {
                Some(package) => package_findings(&view, package, version.as_deref()).await?,
                None => lint_core(&view),
            };
            if *shareable {
                findings.extend(lint_shareable(&view, allow_fn));
            }
            let fmt = ReportFormat::from_flags(*json, *sarif);
            if report_findings(&origin, &source, &findings, fmt, color, "no findings") {
                return Err(Findings.into());
            }
        }
    }
    Ok(())
}
