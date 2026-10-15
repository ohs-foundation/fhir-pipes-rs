//! `fhir-pipes views`: run, check and test SQL-on-FHIR ViewDefinitions
//! directly, without a warehouse. Ported from the octofhir-sof command-line
//! tool; the work is done by fhir-pipes-sof and fhir-pipes-sof-lint.
//!
//!   run       run views over FHIR files (or PostgreSQL) and write the rows
//!   sql       generate the SQL for a view, or a CREATE TABLE for its columns
//!   validate  check a view's structure against the spec, offline
//!   lint      check a view's FHIRPath against a FHIR package, or the
//!             shareable subset
//!   test      run SQL-on-FHIR test-case files

mod diagnostic;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fhir_pipes_sof::output::{OutputWriter, ParquetWriter, TemporalEncoding, get_writer};
use fhir_pipes_sof::{SqlGenerator, ViewDefinition, ViewResult};
use fhir_pipes_sof_lint::{Finding, Severity, lint_shareable, validate_structure};

use crate::warehouse::views::{discover, parse_view};

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
        /// Bundle, a JSON resource or array, a directory of such files, or `-`
        /// to read from stdin.
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
        /// of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Parquet only: how date, dateTime, instant and time columns are
        /// encoded. `text` keeps the FHIR string (the spec default); `native`
        /// writes Arrow Date32 / Timestamp(µs, UTC) / Time64(µs), flooring
        /// partial dates (`2026`, `2026-03`) to the start of the period.
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
    /// Check a ViewDefinition's FHIRPath selectors against a FHIR package
    /// (FH01–FH10), and/or against the ShareableViewDefinition subset (FH11).
    /// Exits non-zero on an error.
    Lint {
        /// The ViewDefinition JSON file.
        view: PathBuf,
        /// FHIR package name (e.g. hl7.fhir.r4.core). Optional with --shareable.
        #[arg(long)]
        package: Option<String>,
        /// Package version. When given, the package is installed if missing;
        /// otherwise only an already-present package is used (offline).
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
fn read_view(path: &Path) -> Result<(String, ViewDefinition)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))?;
    let (_, view) = parse_view(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))?
        .with_context(|| format!("{} is not a ViewDefinition", path.display()))?;
    Ok((text, view))
}

/// One view, or every view in a directory keyed by its name. A directory is
/// read as `refresh` reads it: non-view JSON is skipped, an IG build's form is
/// accepted, and a view with a structural error stops the run.
fn load_views(path: &Path) -> Result<Vec<(String, ViewDefinition)>> {
    if path.is_dir() {
        let views = discover(path, &[])?;
        if views.is_empty() {
            bail!("no ViewDefinitions in {}", path.display());
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

/// Where `run` reads resources from, shared by every view in one run.
enum Source {
    Files(Vec<serde_json::Value>),
    #[cfg(feature = "postgres")]
    Db(sqlx_postgres::PgPool),
}

#[cfg(feature = "postgres")]
async fn connect(db: &str) -> Result<Source> {
    let pool = sqlx_postgres::PgPool::connect(db)
        .await
        .context("connecting to the database")?;
    Ok(Source::Db(pool))
}

#[cfg(not(feature = "postgres"))]
async fn connect(_db: &str) -> Result<Source> {
    bail!(
        "--db needs PostgreSQL support, which this fhir-pipes was built without; \
         build it with `--features postgres`"
    )
}

/// Findings from checking `view`'s selectors and generated SQL against a
/// FHIR package (FH01–FH10).
#[cfg(feature = "fhir-packages")]
async fn package_findings(
    view: &ViewDefinition,
    package: &str,
    version: Option<&str>,
) -> Result<Vec<Finding>> {
    let provider = fhir_pipes_sof_lint::FhirSchemaProvider::load(package, version)
        .await
        .with_context(|| format!("loading package {package}"))?;
    if provider.is_empty() {
        bail!(
            "package `{package}` has no StructureDefinitions in the store; \
             pass --version to install it"
        );
    }
    Ok(fhir_pipes_sof_lint::lint(view, &provider))
}

#[cfg(not(feature = "fhir-packages"))]
async fn package_findings(
    _view: &ViewDefinition,
    _package: &str,
    _version: Option<&str>,
) -> Result<Vec<Finding>> {
    bail!(
        "--package needs FHIR package support, which this fhir-pipes was built without; \
         build it with `--features fhir-packages` (--shareable works without it)"
    )
}

async fn run_view(source: &Source, view: &ViewDefinition, threads: usize) -> Result<ViewResult> {
    match source {
        // Evaluation runs on threads of its own; block_in_place lets this
        // runtime worker wait for them without stalling other tasks.
        Source::Files(resources) => tokio::task::block_in_place(|| {
            fhir_pipes_sof::execute_parallel(view, resources, threads)
        })
        .context("running view"),
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
fn select_writer(format: &str, parquet_temporal: &str) -> Result<Box<dyn OutputWriter>> {
    if format.eq_ignore_ascii_case("parquet") {
        let temporal =
            TemporalEncoding::parse(parquet_temporal).context("parsing --parquet-temporal")?;
        return Ok(Box::new(ParquetWriter::new().with_temporal(temporal)));
    }
    get_writer(format).context("choosing the output format")
}

/// The default --ddl table name: the view's name, else `<resource>_view`.
fn default_table_name(view: &ViewDefinition) -> String {
    view.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_view", view.resource.to_lowercase()))
}

/// FHIR resources from a file, a directory of files, or `-` for stdin. NDJSON
/// (`.ndjson`), single resources, arrays of resources and Bundles are read.
fn load_resources(path: &Path, threads: usize) -> Result<Vec<serde_json::Value>> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text)
            .context("reading FHIR resources from stdin")?;
        let mut resources = Vec::new();
        parse_resource_text(&text, &mut resources).context("parsing stdin")?;
        if resources.is_empty() {
            bail!("no FHIR resources on stdin");
        }
        return Ok(resources);
    }
    let mut resources = Vec::new();
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("reading directory {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json" || x == "ndjson"))
            .collect();
        entries.sort();
        for entry in entries {
            load_file(&entry, &mut resources, threads)?;
        }
    } else {
        load_file(path, &mut resources, threads)?;
    }
    if resources.is_empty() {
        bail!("no FHIR resources in {}", path.display());
    }
    Ok(resources)
}

fn load_file(path: &Path, out: &mut Vec<serde_json::Value>, threads: usize) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().is_some_and(|x| x == "ndjson") {
        for value in
            parse_ndjson(&text, threads).with_context(|| format!("parsing {}", path.display()))?
        {
            push_resource(value, out);
        }
    } else {
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        push_resource(value, out);
    }
    Ok(())
}

/// Parse NDJSON on up to `threads` threads, keeping line order. Blank lines
/// are skipped; an error names the first bad line.
fn parse_ndjson(text: &str, threads: usize) -> Result<Vec<serde_json::Value>> {
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
                            serde_json::from_str(line).with_context(|| format!("line {}", i + 1))
                        })
                        .collect::<Result<Vec<serde_json::Value>>>()
                })
            })
            .collect();
        let mut values = Vec::with_capacity(lines.len());
        for worker in workers {
            values.extend(
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("parsing thread panicked"))??,
            );
        }
        Ok(values)
    })
}

/// The next non-blank line of `reader` and its 1-based number, counting from
/// `*line_no`.
fn next_line(reader: &mut dyn io::BufRead, line_no: &mut usize) -> Result<Option<(usize, String)>> {
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .with_context(|| format!("reading line {}", *line_no + 1))?;
        if n == 0 {
            return Ok(None);
        }
        *line_no += 1;
        if !line.trim().is_empty() {
            return Ok(Some((*line_no, line)));
        }
    }
}

/// Stream NDJSON through one view, writing NDJSON rows as they are produced,
/// without holding the input or the output in memory. `first` is a line the
/// caller already read from `reader`. Returns the number of rows written.
async fn stream_ndjson(
    view: &ViewDefinition,
    first: Option<(usize, String)>,
    mut reader: Box<dyn io::BufRead>,
    mut line_no: usize,
    sink: &mut dyn Write,
) -> Result<usize> {
    let compiled = fhir_pipes_sof::CompiledView::compile(view)
        .await
        .context("compiling view")?;
    let names: Vec<String> = compiled.columns().iter().map(|c| c.name.clone()).collect();
    let mut total = 0;
    let mut next = first;
    while let Some((number, line)) = next {
        let value: serde_json::Value =
            serde_json::from_str(&line).with_context(|| format!("parsing line {number}"))?;
        // A Bundle or array on one line is expanded into its resources.
        let mut resources = Vec::new();
        push_resource(value, &mut resources);
        for resource in &resources {
            for row in compiled.execute_resource(resource).await? {
                let obj: serde_json::Map<String, serde_json::Value> = names
                    .iter()
                    .cloned()
                    .zip(row)
                    .filter(|(_, v)| !v.is_null())
                    .collect();
                writeln!(sink, "{}", serde_json::Value::Object(obj))
                    .context("writing NDJSON row")?;
                total += 1;
            }
        }
        next = next_line(reader.as_mut(), &mut line_no)?;
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
        bail!(
            "--out {} is the --input file; writing it would empty the input",
            out.display()
        );
    }
    Ok(())
}

fn open_sink(out: Option<&PathBuf>) -> Result<Box<dyn Write>> {
    Ok(match out {
        Some(path) => Box::new(
            fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        ),
        None => Box::new(io::stdout().lock()),
    })
}

/// Resource text of unknown shape (stdin has no extension to go by): one JSON
/// value if it parses whole, otherwise NDJSON.
fn parse_resource_text(text: &str, out: &mut Vec<serde_json::Value>) -> Result<()> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        push_resource(value, out);
        return Ok(());
    }
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("parsing line {}", i + 1))?;
        push_resource(value, out);
    }
    Ok(())
}

/// A Bundle becomes its entries' resources, an array its elements, anything
/// else one resource.
fn push_resource(value: serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                push_resource(item, out);
            }
        }
        serde_json::Value::Object(ref map)
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") =>
        {
            if let Some(entries) = map.get("entry").and_then(|e| e.as_array()) {
                for entry in entries {
                    if let Some(resource) = entry.get("resource") {
                        push_resource(resource.clone(), out);
                    }
                }
            }
        }
        other => out.push(other),
    }
}

/// Run one test-case file in memory, printing each case. Returns
/// `(passed, total)`.
fn run_test_file(path: &Path, color: bool) -> Result<(usize, usize)> {
    let doc: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;

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
        let got = result.row_count as u64;
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

/// A run whose findings or test cases failed: reported already, so the
/// caller only sets the exit status.
#[derive(Debug)]
pub struct Failed;

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed")
    }
}

impl std::error::Error for Failed {}

async fn run_command(command: &ViewsCommand, color: bool) -> Result<()> {
    match command {
        ViewsCommand::Sql {
            view,
            ddl,
            dialect,
            table,
        } => {
            let (_, view) = read_view(view)?;
            let dialect: fhir_pipes_sof::Dialect =
                dialect.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let generated = SqlGenerator::new()
                .with_dialect(dialect)
                .generate(&view)
                .context("generating SQL")?;
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
            let threads = threads
                .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
            // A directory of views runs every view in one pass, writing one
            // file per view into the --out directory.
            let multi = view.is_dir();

            if !multi {
                check_out_is_not_input(input.as_ref(), out.as_ref())?;
            }

            // One view, NDJSON in and NDJSON out: stream it resource by
            // resource in bounded memory. A .ndjson file is taken at its word.
            // Stdin is streamed when its first line is one whole JSON value;
            // otherwise (formatted JSON, a Bundle or an array over several
            // lines) it is read whole, as for the other output formats.
            let mut stdin_resources: Option<Vec<serde_json::Value>> = None;
            let stdin_in = input.as_ref().is_some_and(|p| p.as_os_str() == "-");
            let ndjson_in = stdin_in
                || input
                    .as_ref()
                    .is_some_and(|p| p.extension().is_some_and(|x| x == "ndjson"));
            if !multi && output.eq_ignore_ascii_case("ndjson") && ndjson_in {
                let (_, view_def) = read_view(view)?;
                let path = input.as_ref().expect("checked above");
                let mut reader: Box<dyn io::BufRead> = if stdin_in {
                    Box::new(io::BufReader::new(io::stdin()))
                } else {
                    Box::new(io::BufReader::new(
                        fs::File::open(path)
                            .with_context(|| format!("opening {}", path.display()))?,
                    ))
                };
                let mut line_no = 0;
                let first = next_line(reader.as_mut(), &mut line_no)?;
                let one_value_per_line = !stdin_in
                    || first.as_ref().is_none_or(|(_, l)| {
                        serde_json::from_str::<serde::de::IgnoredAny>(l).is_ok()
                    });
                if one_value_per_line {
                    let mut sink = open_sink(out.as_ref())?;
                    let n = stream_ndjson(&view_def, first, reader, line_no, &mut sink).await?;
                    sink.flush().ok();
                    eprintln!("{n} rows");
                    return Ok(());
                }
                let mut text = first.map(|(_, l)| l).unwrap_or_default();
                reader
                    .read_to_string(&mut text)
                    .context("reading FHIR resources from stdin")?;
                let mut resources = Vec::new();
                parse_resource_text(&text, &mut resources).context("parsing stdin")?;
                if resources.is_empty() {
                    bail!("no FHIR resources on stdin");
                }
                stdin_resources = Some(resources);
            }

            let views = load_views(view)?;
            let source = match (input, db) {
                (Some(_), _) if stdin_resources.is_some() => {
                    Source::Files(stdin_resources.take().expect("checked"))
                }
                (Some(path), _) => Source::Files(load_resources(path, threads)?),
                (None, Some(db)) => connect(db).await?,
                (None, None) => {
                    bail!("pass --input <file|dir|-> to run on files, or --db <url>")
                }
            };
            let writer = select_writer(output, parquet_temporal)?;

            if multi {
                let out_dir = out
                    .as_ref()
                    .context("--out <dir> is needed to run a directory of views")?;
                fs::create_dir_all(out_dir)
                    .with_context(|| format!("creating {}", out_dir.display()))?;
                let ext = output_extension(output);
                for (name, view) in &views {
                    let result = run_view(&source, view, threads).await?;
                    let path = out_dir.join(format!("{name}.{ext}"));
                    let mut sink: Box<dyn Write> = Box::new(
                        fs::File::create(&path)
                            .with_context(|| format!("creating {}", path.display()))?,
                    );
                    writer
                        .write(&result, &mut sink)
                        .with_context(|| format!("writing {}", path.display()))?;
                    sink.flush().ok();
                    eprintln!("{name} → {} ({} rows)", path.display(), result.row_count);
                }
            } else {
                let (_, view) = &views[0];
                let result = run_view(&source, view, threads).await?;
                let mut sink = open_sink(out.as_ref())?;
                writer.write(&result, &mut sink).context("writing output")?;
                sink.flush().ok();
            }
        }
        ViewsCommand::Validate { view, json, sarif } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(view)?;
            let findings = validate_structure(&view);
            let fmt = ReportFormat::from_flags(*json, *sarif);
            if report_findings(&origin, &source, &findings, fmt, color, "valid") {
                return Err(Failed.into());
            }
        }
        ViewsCommand::Test { manifest } => {
            let mut files = Vec::new();
            if manifest.is_dir() {
                let mut entries: Vec<PathBuf> = fs::read_dir(manifest)
                    .with_context(|| format!("reading directory {}", manifest.display()))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect();
                entries.sort();
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
                return Err(Failed.into());
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
            if package.is_none() && !shareable {
                bail!(
                    "nothing to lint: pass --package <name> to check selectors against \
                     a FHIR package, and/or --shareable for the portable subset"
                );
            }
            let origin = view.display().to_string();
            let (source, view) = read_view(view)?;
            let mut findings = Vec::new();
            if let Some(package) = package {
                findings.extend(package_findings(&view, package, version.as_deref()).await?);
            }
            if *shareable {
                findings.extend(lint_shareable(&view, allow_fn));
            }
            let fmt = ReportFormat::from_flags(*json, *sarif);
            if report_findings(&origin, &source, &findings, fmt, color, "no findings") {
                return Err(Failed.into());
            }
        }
    }
    Ok(())
}
