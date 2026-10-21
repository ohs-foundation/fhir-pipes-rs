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
//!
//! `rows` runs views and writes their rows, `check` reports validate's and
//! lint's findings, `test_cases` runs test-case files, and `run` finds
//! views and runs them in memory for `views run` and `refresh`.

mod check;
mod diagnostic;
mod rows;
pub mod run;
mod test_cases;

pub(crate) use rows::{close_sink, open_sink, select_writer};

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fhir_pipes_sof::{SqlGenerator, ViewDefinition};
use fhir_pipes_sof_lint::{lint_core, lint_shareable, validate_structure};

use crate::error::{Findings, usage};

use check::{ReportFormat, package_findings, report_findings};
use run::{discover, parse_view};

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

/// The default --ddl table name: the view's name, else `<resource>_view`.
fn default_table_name(view: &ViewDefinition) -> String {
    view.name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_view", view.resource.to_lowercase()))
}

async fn run_command(command: &ViewsCommand, color: bool) -> Result<()> {
    match command {
        ViewsCommand::Sql {
            view,
            ddl,
            dialect,
            table,
        } => sql(view, *ddl, dialect, table.as_deref()),
        ViewsCommand::Run {
            view,
            input,
            db,
            output,
            out,
            parquet_temporal,
            threads,
        } => {
            rows::run(
                view,
                input.as_ref(),
                db.as_deref(),
                output,
                out.as_ref(),
                parquet_temporal,
                *threads,
            )
            .await
        }
        ViewsCommand::Validate { view, json, sarif } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(view)?;
            let findings = validate_structure(&view);
            let fmt = ReportFormat::from_flags(*json, *sarif);
            if report_findings(&origin, &source, &findings, fmt, color, "valid") {
                return Err(Findings.into());
            }
            Ok(())
        }
        ViewsCommand::Test { manifest } => test_cases::run(manifest, color),
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
            Ok(())
        }
    }
}

/// `views sql`: the SELECT for `view`, or with `ddl` a CREATE TABLE for its
/// columns.
fn sql(view: &Path, ddl: bool, dialect: &str, table: Option<&str>) -> Result<()> {
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
    if ddl {
        let table = table.map_or_else(|| default_table_name(&view), str::to_string);
        print!(
            "{}",
            fhir_pipes_sof::create_table(&table, &generated.columns, dialect)
        );
    } else {
        println!("{}", generated.sql);
    }
    Ok(())
}
