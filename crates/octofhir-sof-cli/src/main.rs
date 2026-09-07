//! `octofhir-sof` — command-line interface over the `octofhir-sof` library.
//!
//! The CLI is a thin shell: it parses arguments and delegates to the library's
//! `SqlGenerator`, `ViewRunner` and output writers. All real work lives in the
//! library so it stays embeddable.

mod diagnostic;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use octofhir_sof::output::{OutputWriter, get_writer};
use octofhir_sof::{SqlGenerator, ViewDefinition, ViewResult, ViewRunner};
use octofhir_sof_lint::{FhirSchemaProvider, Severity, lint, lint_shareable, validate_structure};
use sqlx_postgres::PgPool;

#[derive(Parser)]
#[command(name = "octofhir-sof", version, about = "SQL on FHIR toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Disable coloured output (also honours the NO_COLOR environment variable).
    #[arg(long, global = true)]
    no_color: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Generate SQL from a ViewDefinition (offline, no database). Emits a
    /// PostgreSQL SELECT by default; `--dialect duckdb` emits DuckDB SQL. With
    /// `--ddl`, emit a CREATE TABLE for the view's output columns instead.
    Generate {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// Emit a CREATE TABLE statement for the output columns instead of the
        /// SELECT query.
        #[arg(long)]
        ddl: bool,

        /// SQL dialect. For `--ddl`: ansi (default, spec types), postgres or
        /// duckdb. For the SELECT query: postgres (the default) or duckdb (ansi
        /// is treated as postgres).
        #[arg(long, default_value = "ansi")]
        dialect: String,

        /// Table name for `--ddl` (defaults to the ViewDefinition name, or the
        /// resource name suffixed with `_view`).
        #[arg(long)]
        table: Option<String>,
    },

    /// Execute a ViewDefinition and write the rows. Runs against FHIR files with
    /// `--input` (no database) or against PostgreSQL with `--db`.
    Run {
        /// Path to a ViewDefinition JSON file, or a directory of them. A
        /// directory runs every view in one pass, writing one file per view
        /// into the --out directory.
        view: PathBuf,

        /// FHIR resources to run against, with no database: an NDJSON file, a
        /// Bundle, a JSON resource or array, a directory of such files, or `-`
        /// to read from stdin.
        #[arg(long, conflicts_with = "db")]
        input: Option<PathBuf>,

        /// PostgreSQL connection URL (or set DATABASE_URL).
        #[arg(long, env = "DATABASE_URL")]
        db: Option<String>,

        /// Output format: csv, ndjson, json (parquet with the parquet feature).
        #[arg(long, default_value = "csv")]
        output: String,

        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Parquet only: how date, dateTime, instant and time columns are
        /// encoded. `text` keeps the FHIR string (spec default); `native`
        /// writes Arrow Date32 / Timestamp(µs, UTC) / Time64(µs), flooring
        /// partial dates (`2026`, `2026-03`) to the start of the period.
        #[arg(long, default_value = "text", value_parser = ["text", "native"])]
        parquet_temporal: String,
    },

    /// Validate a ViewDefinition's structure against the spec, offline (no FHIR
    /// package and no database): SQL-safe names, one iteration construct per
    /// select, unique column names, consistent unionAll branches.
    Validate {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// Emit findings as machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,

        /// Emit findings as SARIF 2.1.0 (for CI code scanning).
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
    },

    /// Run a SQL-on-FHIR test-case file (the official content-test format:
    /// `{resources, tests:[{title, view, expect|expectCount|expectColumns|
    /// expectError}]}`) in memory and report pass/fail. Exits non-zero on any
    /// failure.
    Test {
        /// Path to the test-case JSON file (or a directory of them).
        manifest: PathBuf,
    },

    /// Validate a ViewDefinition's FHIRPath selectors and generated SQL against
    /// a FHIR package.
    Lint {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// FHIR package name (e.g. hl7.fhir.r4.core). Enables schema-driven
        /// selector linting (FH01–FH10). Optional when `--shareable` is used.
        #[arg(long)]
        package: Option<String>,

        /// Package version. When given, the package is installed if missing;
        /// otherwise only an already-present package is used (offline).
        #[arg(long)]
        version: Option<String>,

        /// Enforce the ShareableViewDefinition FHIRPath subset (FH11): flag any
        /// function/operator/literal outside the portable required subset.
        #[arg(long)]
        shareable: bool,

        /// Exempt an engine-registered custom FHIRPath function from the
        /// `--shareable` allow-list. Repeatable.
        #[arg(long = "allow-fn", value_name = "NAME")]
        allow_fn: Vec<String>,

        /// Emit findings as machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,

        /// Emit findings as SARIF 2.1.0 (for CI code scanning).
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
    },
}

/// Read a ViewDefinition file, returning both its raw text (for diagnostics
/// spans) and the parsed view.
fn read_view(path: &PathBuf) -> Result<(String, ViewDefinition)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))?;
    let view = ViewDefinition::parse(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))?;
    Ok((text, view))
}

fn load_view(path: &PathBuf) -> Result<ViewDefinition> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))?;
    ViewDefinition::parse(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))
}

/// A resolved data source for `run`, shared across one or many views.
enum Source {
    Files(Vec<serde_json::Value>),
    Db(PgPool),
}

/// Execute a single view against the resolved source.
async fn run_view(source: &Source, view: &ViewDefinition) -> Result<ViewResult> {
    match source {
        Source::Files(resources) => octofhir_sof::execute(view, resources)
            .await
            .context("executing view"),
        Source::Db(pool) => ViewRunner::new(pool.clone())
            .run(view)
            .await
            .context("executing view"),
    }
}

/// Load one or many ViewDefinitions. A directory yields every `*.json` view
/// (sorted), keyed by view name (falling back to the file stem); a file yields
/// a single entry. The key is used as the per-view output filename in multi mode.
fn load_views(path: &PathBuf) -> Result<Vec<(String, ViewDefinition)>> {
    if path.is_dir() {
        let mut files: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("reading views directory {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        if files.is_empty() {
            anyhow::bail!("no ViewDefinition (*.json) files in {}", path.display());
        }
        let mut views = Vec::new();
        for file in files {
            let view = load_view(&file)?;
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "view".to_string());
            let name = view.name.clone().filter(|n| !n.is_empty()).unwrap_or(stem);
            views.push((name, view));
        }
        Ok(views)
    } else {
        let view = load_view(path)?;
        let name = view
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "view".to_string());
        Ok(vec![(name, view)])
    }
}

/// File extension for an output format (used for per-view filenames).
fn output_extension(format: &str) -> &'static str {
    match format.to_lowercase().as_str() {
        "ndjson" => "ndjson",
        "json" => "json",
        "parquet" => "parquet",
        _ => "csv",
    }
}

/// Select the output writer for `format`, applying Parquet-only options.
///
/// `--parquet-temporal` is accepted (and validated by clap) for every format
/// so scripts can pass it unconditionally, but it only affects Parquet output.
fn select_writer(format: &str, parquet_temporal: &str) -> Result<Box<dyn OutputWriter>> {
    #[cfg(feature = "parquet")]
    if format.eq_ignore_ascii_case("parquet") {
        use octofhir_sof::output::{ParquetWriter, TemporalEncoding};
        let temporal =
            TemporalEncoding::parse(parquet_temporal).context("parsing --parquet-temporal")?;
        return Ok(Box::new(ParquetWriter::new().with_temporal(temporal)));
    }
    #[cfg(not(feature = "parquet"))]
    let _ = parquet_temporal;
    get_writer(format).context("selecting output format")
}

/// The default DDL table name for a view: its `name`, else `<resource>_view`.
fn default_table_name(view: &ViewDefinition) -> String {
    view.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_view", view.resource.to_lowercase()))
}

/// Load FHIR resources from a file or a directory of files. Supports NDJSON
/// (`.ndjson`), single resources, resource arrays, and Bundles.
fn load_resources(path: &PathBuf) -> Result<Vec<serde_json::Value>> {
    // `-` reads FHIR resources from stdin (NDJSON or a single JSON value).
    if path.as_os_str() == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .context("reading FHIR resources from stdin")?;
        let mut resources = Vec::new();
        parse_resource_text(&text, &mut resources).context("parsing stdin")?;
        if resources.is_empty() {
            anyhow::bail!("no FHIR resources found on stdin");
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
            load_file(&entry, &mut resources)?;
        }
    } else {
        load_file(path, &mut resources)?;
    }
    if resources.is_empty() {
        anyhow::bail!("no FHIR resources found in {}", path.display());
    }
    Ok(resources)
}

fn load_file(path: &PathBuf, out: &mut Vec<serde_json::Value>) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().is_some_and(|x| x == "ndjson") {
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parsing {} line {}", path.display(), i + 1))?;
            push_resource(value, out);
        }
    } else {
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        push_resource(value, out);
    }
    Ok(())
}

/// Stream NDJSON input through a compiled view, writing NDJSON rows as they are
/// produced. Reads one line at a time and never holds the whole input or output
/// in memory. Returns the number of rows written.
async fn stream_ndjson(
    view: &ViewDefinition,
    input: &PathBuf,
    sink: &mut dyn Write,
) -> Result<usize> {
    use std::io::BufRead;

    let compiled = octofhir_sof::CompiledView::compile(view)
        .await
        .context("compiling view")?;
    let reader: Box<dyn BufRead> = if input.as_os_str() == "-" {
        Box::new(io::BufReader::new(io::stdin()))
    } else {
        Box::new(io::BufReader::new(
            fs::File::open(input).with_context(|| format!("opening {}", input.display()))?,
        ))
    };

    let names: Vec<String> = compiled.columns().iter().map(|c| c.name.clone()).collect();
    let mut total = 0;
    for (i, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading line {}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(&line).with_context(|| format!("parsing line {}", i + 1))?;
        // Expand a Bundle/array line into its resources.
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
    }
    Ok(total)
}

/// Parse resource text of unknown shape: a single JSON value (object, array or
/// Bundle) if it parses whole, otherwise NDJSON (one JSON value per line). Used
/// for stdin, where there is no filename extension to disambiguate.
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

/// Expand a JSON value into resources: a Bundle becomes its entries, an array
/// becomes its elements, anything else is a single resource.
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

/// Run one SQL-on-FHIR test-case file in memory, printing per-test results.
/// Returns `(passed, total)`.
fn run_test_file(path: &PathBuf, color: bool) -> Result<(usize, usize)> {
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
    let result = match octofhir_sof::execute_blocking(&view, resources) {
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

/// Order-insensitive comparison of two JSON row sets.
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

/// Canonical JSON string with object keys sorted, for set comparison.
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

/// Print findings (JSON or rustc-style) and report whether any are errors.
/// Output format for validate/lint findings.
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

/// Report findings in the chosen format. Returns true if any finding is an
/// error (so the caller can set a non-zero exit code). For machine formats
/// (JSON/SARIF) an empty finding list still emits a valid empty report.
fn report_findings(
    origin: &str,
    source: &str,
    findings: &[octofhir_sof_lint::Finding],
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let color = diagnostic::use_color(cli.no_color);
    if let Err(e) = run(cli, color).await {
        eprintln!("{} {e:#}", diagnostic::error_prefix(color));
        std::process::exit(1);
    }
}

async fn run(cli: Cli, color: bool) -> Result<()> {
    match cli.command {
        Command::Generate {
            view,
            ddl,
            dialect,
            table,
        } => {
            let view = load_view(&view)?;
            let dialect: octofhir_sof::Dialect =
                dialect.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let generated = SqlGenerator::new()
                .with_dialect(dialect)
                .generate(&view)
                .context("generating SQL")?;
            if ddl {
                let table = table.unwrap_or_else(|| default_table_name(&view));
                print!(
                    "{}",
                    octofhir_sof::create_table(&table, &generated.columns, dialect)
                );
            } else {
                println!("{}", generated.sql);
            }
        }
        Command::Run {
            view,
            input,
            db,
            output,
            out,
            parquet_temporal,
        } => {
            // A directory of ViewDefinitions runs every view in one pass, writing
            // one output file per view into the --out directory.
            let multi = view.is_dir();

            // Fast path: a single view, NDJSON input (file or stdin) and NDJSON
            // output stream resource-by-resource in bounded memory.
            let ndjson_in = input.as_ref().is_some_and(|p| {
                p.as_os_str() == "-" || p.extension().is_some_and(|x| x == "ndjson")
            });
            if !multi && output.eq_ignore_ascii_case("ndjson") && ndjson_in {
                let view = load_view(&view)?;
                let path = input.expect("checked above");
                let mut sink: Box<dyn Write> = match &out {
                    Some(path) => Box::new(
                        fs::File::create(path)
                            .with_context(|| format!("creating {}", path.display()))?,
                    ),
                    None => Box::new(io::stdout().lock()),
                };
                let n = stream_ndjson(&view, &path, &mut sink).await?;
                sink.flush().ok();
                eprintln!("{n} rows");
                return Ok(());
            }

            let views = load_views(&view)?;

            // Acquire the data source once, shared across all views.
            let source = match (input, db) {
                (Some(path), _) => Source::Files(load_resources(&path)?),
                (None, Some(db)) => Source::Db(
                    PgPool::connect(&db)
                        .await
                        .with_context(|| format!("connecting to {db}"))?,
                ),
                (None, None) => {
                    anyhow::bail!("provide --input <file|dir|-> to run on files, or --db <url>")
                }
            };

            let writer = select_writer(&output, &parquet_temporal)?;

            if multi {
                let out_dir =
                    out.context("--out <dir> is required when running a directory of views")?;
                fs::create_dir_all(&out_dir)
                    .with_context(|| format!("creating output directory {}", out_dir.display()))?;
                let ext = output_extension(&output);
                for (name, view) in &views {
                    let result = run_view(&source, view).await?;
                    let path = out_dir.join(format!("{name}.{ext}"));
                    let mut sink: Box<dyn Write> = Box::new(
                        fs::File::create(&path)
                            .with_context(|| format!("creating {}", path.display()))?,
                    );
                    writer
                        .write(&result, &mut sink)
                        .with_context(|| format!("writing {}", path.display()))?;
                    sink.flush().ok();
                    eprintln!("{} -> {}", name, path.display());
                }
            } else {
                let (_, view) = &views[0];
                let result = run_view(&source, view).await?;
                let mut sink: Box<dyn Write> = match out {
                    Some(path) => Box::new(
                        fs::File::create(&path)
                            .with_context(|| format!("creating {}", path.display()))?,
                    ),
                    None => Box::new(io::stdout().lock()),
                };
                writer.write(&result, &mut sink).context("writing output")?;
                sink.flush().ok();
            }
        }
        Command::Validate { view, json, sarif } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(&view)?;
            let findings = validate_structure(&view);
            let fmt = ReportFormat::from_flags(json, sarif);
            let has_error = report_findings(&origin, &source, &findings, fmt, color, "valid");
            if has_error {
                std::process::exit(1);
            }
        }
        Command::Test { manifest } => {
            let mut files = Vec::new();
            if manifest.is_dir() {
                let mut entries: Vec<PathBuf> = fs::read_dir(&manifest)
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
            println!(
                "{}",
                if pass == total {
                    diagnostic::ok(&summary, color)
                } else {
                    summary
                }
            );
            if pass < total {
                std::process::exit(1);
            }
        }
        Command::Lint {
            view,
            package,
            version,
            shareable,
            allow_fn,
            json,
            sarif,
        } => {
            if package.is_none() && !shareable {
                anyhow::bail!(
                    "nothing to lint: pass --package <name> for schema-driven \
                     linting and/or --shareable for the portable-subset check"
                );
            }
            let origin = view.display().to_string();
            let (source, view) = read_view(&view)?;

            let mut findings = Vec::new();
            if let Some(package) = &package {
                let provider = FhirSchemaProvider::load(package, version.as_deref())
                    .await
                    .with_context(|| format!("loading package {package}"))?;
                if provider.is_empty() {
                    anyhow::bail!(
                        "package `{package}` has no StructureDefinitions in the store; \
                         pass --version to install it"
                    );
                }
                findings.extend(lint(&view, &provider));
            }
            if shareable {
                findings.extend(lint_shareable(&view, &allow_fn));
            }
            let fmt = ReportFormat::from_flags(json, sarif);
            let has_error = report_findings(&origin, &source, &findings, fmt, color, "no findings");
            if has_error {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
