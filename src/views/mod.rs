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
pub(crate) mod ndjson;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fhir_pipes_sof::output::{
    OutputWriter, ParquetWriter, TemporalEncoding, WriteSummary, get_writer,
};
use fhir_pipes_sof::{SqlGenerator, ViewDefinition, ViewResult};
use fhir_pipes_sof_lint::{Finding, Severity, lint_core, lint_shareable, validate_structure};

use crate::error::usage;
use crate::fhir::ndjson::{Output, is_gz, read_text};
use crate::warehouse::views::{ViewRows, discover, parse_view};

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
        /// FHIR package name (e.g. hl7.fhir.r4.core), to check against in
        /// place of the built-in core types.
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
    crate::extract::client::ensure_crypto_provider();
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
        return Err(usage(anyhow::anyhow!(
            "package `{package}` has no StructureDefinitions in the store; \
             pass --version to install it"
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
fn select_writer(format: &str, parquet_temporal: &str) -> Result<Box<dyn OutputWriter>> {
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
        parse_resource_text(&text, &mut resources)
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
            .and_then(|files| for_each_chunk(&files, threads, &mut add))
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

/// Read `files` in order and hand their resources to `f` a chunk at a time:
/// an NDJSON file (`is_ndjson`) a chunk of lines at a time,
/// parsed on `threads` threads, other files whole, several small ones
/// together, up to a chunk's worth of resources. A Bundle or an array is
/// handed on as its resources. Returns the number of resources.
fn for_each_chunk(
    files: &[PathBuf],
    threads: usize,
    mut f: impl FnMut(Vec<serde_json::Value>) -> Result<()>,
) -> Result<usize> {
    let mut count = 0;
    // Resources of JSON files not yet handed on.
    let mut pending = Vec::new();
    for file in files {
        if is_ndjson(file) {
            if !pending.is_empty() {
                f(std::mem::take(&mut pending))?;
            }
            for chunk in ndjson::open(file)? {
                let mut resources = Vec::new();
                // A line that is not JSON is the input's fault.
                for value in chunk?.values(threads).map_err(usage)? {
                    push_resource(value, &mut resources);
                }
                count += resources.len();
                f(resources)?;
            }
        } else {
            let before = pending.len();
            load_json_file(file, &mut pending)?;
            count += pending.len() - before;
            if pending.len() >= ndjson::CHUNK_LINES {
                f(std::mem::take(&mut pending))?;
            }
        }
    }
    if !pending.is_empty() {
        f(pending)?;
    }
    Ok(count)
}

/// The files `path` names: itself if it is a file, else the resource files
/// under it (`collect_resource_files`), sorted by path. `refresh` reads its
/// file source through this.
pub(crate) fn resource_files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files = Vec::new();
    collect_resource_files(path, &mut files)?;
    files.sort();
    check_no_gz_copies(&files)?;
    Ok(files)
}

/// The extension that says what a resource file holds, past a `.gz`:
/// `ndjson` for both `a.ndjson` and `a.ndjson.gz`.
fn content_extension(path: &Path) -> Option<&OsStr> {
    if is_gz(path) {
        Path::new(path.file_stem()?).extension()
    } else {
        path.extension()
    }
}

/// Whether `path` is read as NDJSON: `.ndjson` or `.ndjson.gz`. Any other
/// file is read as one JSON value.
pub(crate) fn is_ndjson(path: &Path) -> bool {
    content_extension(path).is_some_and(|x| x == "ndjson")
}

/// Whether a directory's or a pattern's file is read as resources:
/// `.json`, `.ndjson`, `.json.gz` or `.ndjson.gz`.
pub(crate) fn is_resource_file(path: &Path) -> bool {
    content_extension(path).is_some_and(|x| x == "json" || x == "ndjson")
}

/// An error when `files` holds both `X` and `X.gz`. Both would be read, and
/// they most likely hold the same resources (`gzip -k` leaves the original),
/// which would give every row twice.
pub(crate) fn check_no_gz_copies(files: &[PathBuf]) -> Result<()> {
    let plain: HashSet<&Path> = files
        .iter()
        .filter(|f| !is_gz(f))
        .map(PathBuf::as_path)
        .collect();
    for gz in files.iter().filter(|f| is_gz(f)) {
        let original = gz.with_extension("");
        if plain.contains(original.as_path()) {
            return Err(usage(anyhow::anyhow!(
                "{} and {} are both inputs, and would both be read; remove one",
                original.display(),
                gz.display()
            )));
        }
    }
    Ok(())
}

/// The `.json` and `.ndjson` files under `dir`, and the same compressed
/// (`.json.gz`, `.ndjson.gz`), in subdirectories too.
/// Hidden files and directories (`.export-state.json`) are skipped, and so
/// are symlinked directories, which could lead back up the tree and read the
/// same files again. Symlinked files are read.
fn collect_resource_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading directory {}", dir.display()))?;
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        // file_type() does not follow a symlink; is_dir() and is_file() do.
        let kind = entry
            .file_type()
            .with_context(|| format!("reading {}", path.display()))?;
        if kind.is_dir() {
            collect_resource_files(&path, out)?;
        } else if (kind.is_file() || (kind.is_symlink() && path.is_file()))
            && is_resource_file(&path)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The resources in one JSON file, a resource, an array or a Bundle
/// (`push_resource`), appended to `out`. A `.gz` file is read decompressed.
pub(crate) fn load_json_file(path: &Path, out: &mut Vec<serde_json::Value>) -> Result<()> {
    let read = || -> io::Result<String> {
        let mut text = String::new();
        read_text(fs::File::open(path)?, is_gz(path)).read_to_string(&mut text)?;
        Ok(text)
    };
    let text = read().map_err(|e| ndjson::read_error(e, format!("reading {}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .map_err(usage)?;
    push_resource(value, out);
    Ok(())
}

/// The next non-blank line of `reader` and its 1-based number, counting from
/// `*line_no`. `name` is the input's name in messages.
fn next_line(
    reader: &mut dyn io::BufRead,
    name: &str,
    line_no: &mut usize,
) -> Result<Option<(usize, String)>> {
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ndjson::read_error(e, format!("reading {name} line {}", *line_no + 1)))?;
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
    name: &str,
    mut line_no: usize,
    sink: &mut dyn Write,
) -> Result<usize> {
    let compiled = fhir_pipes_sof::CompiledView::compile(view)
        .await
        .context("compiling view")
        .map_err(usage)?;
    let names: Vec<String> = compiled.columns().iter().map(|c| c.name.clone()).collect();
    let mut total = 0;
    let mut next = first;
    while let Some((number, line)) = next {
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parsing {name} line {number}"))
            .map_err(usage)?;
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
        next = next_line(reader.as_mut(), name, &mut line_no)?;
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
fn open_sink(out: Option<&PathBuf>) -> Result<Output<Box<dyn Write>>> {
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
fn close_sink(sink: Output<Box<dyn Write>>, out: Option<&PathBuf>) -> Result<()> {
    sink.finish().map(drop).with_context(|| match out {
        Some(path) => format!("writing {}", path.display()),
        None => "writing to stdout".to_string(),
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
///
/// A reference to another entry of the same Bundle by its `urn:` fullUrl
/// (`urn:uuid:…`, as in a transaction Bundle) is rewritten to that entry's
/// `Type/id`, as a server would store it, so `getReferenceKey()` and joins
/// work on Bundle files. An entry without an id is left as is.
pub(crate) fn push_resource(value: serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                push_resource(item, out);
            }
        }
        serde_json::Value::Object(mut map)
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") =>
        {
            let Some(serde_json::Value::Array(entries)) = map.remove("entry") else {
                return;
            };
            let urns = bundle_urns(&entries);
            for entry in entries {
                if let serde_json::Value::Object(mut entry) = entry
                    && let Some(mut resource) = entry.remove("resource")
                {
                    if !urns.is_empty() {
                        resolve_urns(&mut resource, &urns);
                    }
                    push_resource(resource, out);
                }
            }
        }
        other => out.push(other),
    }
}

/// `urn:` fullUrl → `Type/id` for the entries of a Bundle that have both.
fn bundle_urns(entries: &[serde_json::Value]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let full_url = entry.get("fullUrl")?.as_str()?;
            if !full_url.starts_with("urn:") {
                return None;
            }
            let resource = entry.get("resource")?;
            let ty = resource.get("resourceType")?.as_str()?;
            let id = resource.get("id")?.as_str()?;
            Some((full_url.to_string(), format!("{ty}/{id}")))
        })
        .collect()
}

/// Rewrite every `reference` in `value` that is a key of `urns`. A Bundle
/// inside `value` is left alone: it resolves its own entries when it is read.
fn resolve_urns(value: &mut serde_json::Value, urns: &HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") {
                return;
            }
            if let Some(serde_json::Value::String(reference)) = map.get_mut("reference")
                && let Some(resolved) = urns.get(reference.as_str())
            {
                *reference = resolved.clone();
            }
            for child in map.values_mut() {
                resolve_urns(child, urns);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_urns(item, urns);
            }
        }
        _ => {}
    }
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
                let mut reader: Box<dyn io::BufRead> = if stdin_in {
                    Box::new(io::BufReader::new(io::stdin()))
                } else {
                    read_text(
                        fs::File::open(path)
                            .with_context(|| format!("opening {}", path.display()))
                            .map_err(|e| if path.exists() { e } else { usage(e) })?,
                        is_gz(path),
                    )
                };
                let mut line_no = 0;
                let first = next_line(reader.as_mut(), &name, &mut line_no)?;
                let one_value_per_line = !stdin_in
                    || first.as_ref().is_none_or(|(_, l)| {
                        serde_json::from_str::<serde::de::IgnoredAny>(l).is_ok()
                    });
                if one_value_per_line {
                    let mut sink = open_sink(out.as_ref())?;
                    let n =
                        stream_ndjson(&view_def, first, reader, &name, line_no, &mut sink).await?;
                    close_sink(sink, out.as_ref())?;
                    eprintln!("{n} rows");
                    return Ok(());
                }
                let mut text = first.map(|(_, l)| l).unwrap_or_default();
                reader
                    .read_to_string(&mut text)
                    .context("reading FHIR resources from stdin")?;
                let mut resources = Vec::new();
                parse_resource_text(&text, &mut resources)
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
                return Err(Failed.into());
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
                return Err(Failed.into());
            }
        }
    }
    Ok(())
}
