//! `fhir-pipes refresh`: refresh a SQL-on-FHIR Parquet warehouse from a FHIR
//! server. This was fhir-warehouse, which ran kiln as a separate program; the
//! FHIR side now runs in process.
//!
//! With a file source (`[source] files`, `--input`), the resources are read
//! from files instead of the server (files.rs), and the locations and views
//! steps read them; the server is never contacted.
//!
//! Steps, in order:
//!   locations  extract and transform the Location registry → parquet/locations
//!   views      export each resource type the views read (only what changed
//!              since the last refresh), run the views in memory
//!              (fhir-pipes-sof), partition each table by a looked-up column
//!   tiles      the registry → PMTiles (duckdb + tippecanoe); runs beside views
//!   manifest   manifest.json: row counts, views, the extract state
//!   catalog    catalog.sql: a DuckDB view per table, and catalog.spark.sql
//!              with spark = true; only with [catalog]
//!   database   the view tables into PostgreSQL; only with [database] and a
//!              build with the postgres feature
//!   hooks      commands from the config (e.g. a catalog build)
//!   push       mirror the warehouse to object storage (rclone); only with --push
//!
//! The tables, views/, tiles/, the manifest and catalog.sql are written to
//! staging directories (stage.rs) and swapped in together after the database
//! step, so a run that fails before then leaves the previous run's warehouse
//! as it was. The hooks and the push work on the published warehouse.

mod catalog;
mod config;
#[cfg(feature = "postgres")]
mod database;
mod fhir;
mod files;
mod manifest;
mod partition;
mod push;
mod stage;
mod tiles;
mod tools;
pub(crate) mod views;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use fhir_pipes_sof::ViewResult;
use fhir_pipes_sof::output::WriteSummary;
use serde_json::json;

use config::Settings;

use crate::error::usage;

#[derive(clap::Args, Debug, Clone)]
pub struct RefreshArgs {
    /// Config file (default: ./fhir-pipes.toml, else ./fhir-warehouse.toml).
    #[arg(long, env = "FHIR_PIPES_CONFIG")]
    config: Option<PathBuf>,
    /// Warehouse directory.
    #[arg(long, env = "FHIR_PIPES_DATA")]
    data: Option<PathBuf>,
    /// Directory of ViewDefinition JSON files (e.g. an IG's fsh-generated/resources).
    #[arg(long, env = "FHIR_PIPES_VIEWS")]
    views: Option<PathBuf>,
    /// FHIR server base URL, or a Cloud Healthcare API FHIR store path
    /// (projects/P/locations/L/datasets/D/fhirStores/S).
    #[arg(long, env = "FHIR_PIPES_SERVER")]
    server: Option<String>,
    /// Name for the server's raw downloads: raw/<name>/.
    #[arg(long, env = "FHIR_PIPES_SERVER_NAME")]
    server_name: Option<String>,
    /// Read the resources from files instead of a FHIR server: an NDJSON
    /// file, a Bundle, a JSON resource or array, a directory of such files
    /// (searched recursively), or a glob pattern. Repeat for more; replaces
    /// the config's `source.files`.
    #[arg(long, value_name = "PATH")]
    input: Vec<PathBuf>,
    /// Fixed bearer token for the FHIR server.
    #[arg(long, env = "FHIR_PIPES_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// OAuth token endpoint, used with FHIR_PIPES_CLIENT_ID and
    /// FHIR_PIPES_CLIENT_SECRET, which are read only from the environment.
    #[arg(long, env = "FHIR_PIPES_TOKEN_URL")]
    token_url: Option<String>,
    /// Username for HTTP Basic auth, with the password from
    /// FHIR_PIPES_PASSWORD, which is read only from the environment.
    #[arg(long, env = "FHIR_PIPES_USERNAME")]
    username: Option<String>,
    /// Google Application Default Credentials, for a Cloud Healthcare API
    /// FHIR store (as `server.google_auth = true`). `--google-auth=false` or
    /// FHIR_PIPES_GOOGLE_AUTH=0 turns off the config file's setting.
    #[arg(
        long,
        env = "FHIR_PIPES_GOOGLE_AUTH",
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    google_auth: Option<bool>,
    /// Run only these steps (comma-separated). The manifest is added when a
    /// data step is among them.
    #[arg(long, value_delimiter = ',')]
    only: Vec<Step>,
    /// Leave out these steps (comma-separated).
    #[arg(long, value_delimiter = ',')]
    skip: Vec<Step>,
    /// Also push the warehouse to object storage at the end.
    #[arg(long)]
    push: bool,
    /// Countries to build tiles for (comma-separated; default: every country).
    #[arg(long, value_delimiter = ',')]
    countries: Vec<String>,
    /// Threads for reading and evaluating (default: the number of CPUs).
    #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    threads: Option<usize>,
    /// Reuse downloaded NDJSON where present instead of downloading again,
    /// and the Location snapshot in raw/<server>/locations, where there is
    /// one, instead of extracting.
    #[arg(long, env = "FHIR_PIPES_REUSE_RAW")]
    reuse_raw: bool,
    /// Download everything again (Locations and every resource type the views
    /// read) instead of only what changed since the last refresh.
    #[arg(long)]
    full: bool,
    /// Push: show what would change without changing it.
    #[arg(long)]
    dry_run: bool,
    /// Show the resolved settings and which tools are found, then exit.
    #[arg(long)]
    check: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Step {
    Locations,
    Views,
    Tiles,
    Manifest,
    Catalog,
    Database,
    Hooks,
    Push,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Locations => "locations",
            Step::Views => "views",
            Step::Tiles => "tiles",
            Step::Manifest => "manifest",
            Step::Catalog => "catalog",
            Step::Database => "database",
            Step::Hooks => "hooks",
            Step::Push => "push",
        }
    }
}

fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn run(args: &RefreshArgs, profile: Option<&str>) -> Result<()> {
    // Everything up to the tools check is the flags and the config: an error
    // there is the operator's to fix.
    let (s, steps, needed) = setup(args, profile).map_err(usage)?;
    if args.check {
        check(&s, &steps, &needed);
        return Ok(());
    }
    // With a file source, whether the tiles step runs is known only once the
    // files are read (without Locations it is skipped), so its tools are
    // checked then.
    let needed: Vec<(&str, &str)> = needed
        .iter()
        .map(|(t, w)| (t.as_str(), w.as_str()))
        .collect();
    let (later, now): (Vec<_>, Vec<_>) = needed
        .into_iter()
        .partition(|&(_, w)| s.reads_files() && w == "tiles");
    tools::require(&now)?;
    refresh(&s, &steps, &later)
}

/// The settings from the config file and flags, the steps to run and the
/// tools they need.
fn setup(args: &RefreshArgs, profile: Option<&str>) -> Result<(Settings, BTreeSet<Step>, Tools)> {
    let steps = steps(args);
    let (file, base) = config::load(args.config.as_deref())?;
    let s = config::settings(
        file,
        base,
        config::Overrides {
            profile: profile.map(str::to_string),
            data: args.data.clone(),
            views: args.views.clone(),
            input: args.input.clone(),
            server: args.server.clone(),
            server_name: args.server_name.clone(),
            token: args.token.clone(),
            client_id: env_secret("FHIR_PIPES_CLIENT_ID"),
            client_secret: env_secret("FHIR_PIPES_CLIENT_SECRET"),
            token_url: args.token_url.clone(),
            username: args.username.clone(),
            password: env_secret("FHIR_PIPES_PASSWORD"),
            google_auth: args.google_auth,
            countries: args.countries.clone(),
            threads: args.threads,
            reuse_raw: args.reuse_raw,
            full: args.full,
            dry_run: args.dry_run,
        },
    )?;
    let mut steps = steps;
    // The catalog step is in every run with a [catalog] section.
    if s.catalog.is_none() {
        if args.only.contains(&Step::Catalog) {
            bail!("--only catalog needs a [catalog] section in the config");
        }
        steps.remove(&Step::Catalog);
    }
    // The database step likewise, with a [database] section.
    if s.database.is_none() {
        if args.only.contains(&Step::Database) {
            bail!("--only database needs a [database] section in the config");
        }
        steps.remove(&Step::Database);
    } else if steps.contains(&Step::Database) && !cfg!(feature = "postgres") {
        bail!(
            "[database] needs PostgreSQL support, which this fhir-pipes was built without; \
             build it with `--features postgres`, or --skip database"
        );
    }
    let needed = required_tools(&s, &steps)?;
    Ok((s, steps, needed))
}

/// The steps a run takes, from --only, --skip and --push.
fn steps(args: &RefreshArgs) -> BTreeSet<Step> {
    let mut steps: BTreeSet<Step> = if args.only.is_empty() {
        [
            Step::Locations,
            Step::Views,
            Step::Tiles,
            Step::Manifest,
            Step::Catalog,
            Step::Database,
            Step::Hooks,
        ]
        .into()
    } else {
        let mut only: BTreeSet<Step> = args.only.iter().copied().collect();
        if only
            .iter()
            .any(|s| matches!(s, Step::Locations | Step::Views | Step::Tiles))
        {
            only.insert(Step::Manifest);
            only.insert(Step::Catalog);
            only.insert(Step::Database);
        }
        only
    };
    if args.push {
        steps.insert(Step::Push);
    }
    for s in &args.skip {
        steps.remove(s);
    }
    steps
}

/// A hook program as it is run: a path (with `/`, or `\` on Windows) is
/// relative to the config file.
fn hook_program(s: &Settings, program: &str) -> String {
    if tools::is_path(program) {
        s.base_dir.join(program).display().to_string()
    } else {
        program.to_string()
    }
}

/// External tools, each with what it is for.
type Tools = Vec<(String, String)>;

/// The external tools the chosen steps need, with what each is for.
fn required_tools(s: &Settings, steps: &BTreeSet<Step>) -> Result<Tools> {
    let mut t = Vec::new();
    if steps.contains(&Step::Views) && s.views.is_none() {
        bail!(
            "no views directory: pass --views, set FHIR_PIPES_VIEWS, or set `views` in the config"
        );
    }
    if steps.contains(&Step::Tiles) {
        t.push((s.duckdb.clone(), "tiles".into()));
        t.push((s.tippecanoe.clone(), "tiles".into()));
    }
    if let Some(c) = &s.catalog
        && c.duckdb_file
        && (steps.contains(&Step::Push) || (steps.contains(&Step::Catalog) && c.base_url.is_none()))
    {
        t.push((s.duckdb.clone(), "catalog.duckdb".into()));
    }
    if steps.contains(&Step::Push) {
        if s.push.is_none() {
            bail!("--push needs a [push] section (remote, bucket) in the config");
        }
        t.push((s.rclone.clone(), "push".into()));
    }
    if steps.contains(&Step::Hooks) {
        for h in &s.hooks {
            let Some(program) = h.run.first() else {
                bail!("hook {} has an empty `run`", h.name);
            };
            t.push((hook_program(s, program), format!("hook {}", h.name)));
        }
    }
    Ok(t)
}

fn check(s: &Settings, steps: &BTreeSet<Step>, needed: &[(String, String)]) {
    let names = |it: Vec<String>| it.join(", ");
    println!(
        "steps:      {}",
        names(steps.iter().map(|s| s.name().to_string()).collect())
    );
    if s.reads_files() {
        // The server settings are not used, so they are not shown.
        println!("source:     files, no server (raw/{})", s.server_name);
        for input in &s.files {
            let found = files::expand(std::slice::from_ref(input))
                .map_or_else(|e| format!("{e:#}"), |f| format!("{} files", f.len()));
            println!("input:      {}: {found}", input.display());
        }
    } else {
        println!("server:     {} (raw/{})", s.server, s.server_name);
        println!("auth:       {}", s.auth_description());
        match s.fetch {
            config::Fetch::Bulk => println!(
                "fetch:      bulk ($export, waiting up to {} s)",
                s.bulk_timeout
            ),
            config::Fetch::HapiDb => println!(
                "fetch:      hapi-db (the HAPI server's database, from ${}{})",
                crate::export::HAPI_DB_URL_ENV,
                if std::env::var(crate::export::HAPI_DB_URL_ENV).is_ok_and(|v| !v.is_empty()) {
                    ""
                } else {
                    ", which is not set"
                }
            ),
            f => println!(
                "fetch:      {} ({} requests at a time)",
                f.name(),
                s.concurrency
            ),
        }
    }
    println!("data:       {}", s.data.display());
    println!(
        "views:      {}",
        s.views
            .as_ref()
            .map_or("-".into(), |p| p.display().to_string())
    );
    println!("threads:    {}", s.threads);
    println!(
        "profile:    {}",
        s.profile
            .as_deref()
            .unwrap_or("none (the snapshot's, else base FHIR)")
    );
    println!(
        "partition:  {} via {}",
        s.partition_column,
        names(
            s.lookups
                .iter()
                .map(|l| format!("{} → {}", l.key, l.table))
                .collect()
        )
    );
    if let Some(d) = &s.database {
        println!(
            "database:   ${} → schema {}{}",
            d.url_env,
            d.schema,
            if cfg!(feature = "postgres") {
                ""
            } else {
                " (NOT AVAILABLE: built without the postgres feature)"
            }
        );
    }
    if let Some(c) = &s.catalog {
        println!(
            "catalog:    {}{}",
            c.base_url.as_deref().unwrap_or("relative paths"),
            if c.duckdb_file {
                ", with catalog.duckdb"
            } else {
                ""
            }
        );
        if let Some(spark) = &c.spark {
            println!(
                "spark:      database {}, {}",
                spark.database,
                spark
                    .location
                    .as_deref()
                    .unwrap_or("the warehouse directory")
            );
        }
    }
    for (tool, why) in needed {
        let found = tools::find(tool).map_or("NOT FOUND".to_string(), |p| p.display().to_string());
        println!("tool:       {tool} ({why}): {found}");
    }
    if let Some(dir) = &s.views {
        match views::discover(dir, &s.strip_prefix)
            .and_then(|vs| unwritten_tables(s, vs.into_iter().map(|v| v.table).collect()))
        {
            Ok(t) if t.is_empty() => {}
            Ok(t) => println!(
                "unwritten:  {} (in parquet/, written by no current view; kept)",
                t.join(", ")
            ),
            Err(e) => println!("unwritten:  cannot tell: {e:#}"),
        }
    }
}

/// The tables in parquet/ that no current view writes (`written`), leaving
/// out `locations` and the lookup tables, which need not come from a view.
/// A view that was removed or renamed leaves its table behind: refresh keeps
/// it, and it stays in the manifest and the catalog, until it is removed by
/// hand.
fn unwritten_tables(s: &Settings, written: BTreeSet<String>) -> Result<Vec<String>> {
    Ok(stage::table_names(&s.parquet_dir())?
        .into_iter()
        .filter(|t| {
            t != "locations" && !written.contains(t) && !s.lookups.iter().any(|l| &l.table == t)
        })
        .collect())
}

/// `tiles_tools`: the tiles step's tools, when they are checked only once the
/// step is known to run.
fn refresh(s: &Settings, steps: &BTreeSet<Step>, tiles_tools: &[(&str, &str)]) -> Result<()> {
    let started = Instant::now();
    let mut timings: Vec<(&str, f64)> = Vec::new();
    let mut ran: BTreeSet<Step> = BTreeSet::new();
    // Whether the run downloaded anything from the server, for the server
    // the manifest records.
    let mut contacted = false;
    std::fs::create_dir_all(s.raw_dir())?;
    std::fs::create_dir_all(s.parquet_dir())?;
    let stage = stage::Stage::new(s)?;

    // A file source is split by type into raw/ first, as a download would
    // be; --reuse-raw reruns on the last split.
    let reads = steps.contains(&Step::Locations) || steps.contains(&Step::Views);
    if s.reads_files() && reads && !s.reuse_raw {
        let t = Instant::now();
        files::split(s)?;
        timings.push(("files", t.elapsed().as_secs_f64()));
    }

    if steps.contains(&Step::Locations) {
        let t = Instant::now();
        let built = if s.reads_files() {
            files::locations(s, stage.tables_dir())?
        } else {
            contacted |= fhir::locations(s, stage.tables_dir())?;
            true
        };
        if built {
            timings.push(("locations", t.elapsed().as_secs_f64()));
            ran.insert(Step::Locations);
        }
    }

    // Files without Locations leave no locations table to build tiles from.
    let tiles_wanted =
        steps.contains(&Step::Tiles) && !(s.reads_files() && !stage.read("locations").is_dir());
    if steps.contains(&Step::Tiles) && !tiles_wanted {
        println!("== tiles: there is no locations table; skipped");
    }
    if tiles_wanted {
        tools::require(tiles_tools)?;
    }

    // Views and tiles both read the location registry and nothing of each
    // other's, so they run side by side.
    let (views, tiles) = std::thread::scope(|scope| {
        let tiles = tiles_wanted.then(|| {
            scope.spawn(|| {
                let t = Instant::now();
                tiles::build(s, &stage.read("locations"), &stage.dir("tiles"))
                    .map(|()| t.elapsed().as_secs_f64())
            })
        });
        let views = steps.contains(&Step::Views).then(|| {
            let t = Instant::now();
            views_step(s, &stage).map(|done| (done, t.elapsed().as_secs_f64()))
        });
        let tiles = tiles.map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("tiles thread panicked")))
        });
        (views, tiles)
    });
    let mut view_tables = None;
    let mut mismatches = None;
    if let Some(v) = views {
        let (done, secs) = v.context("views")?;
        timings.push(("views", secs));
        ran.insert(Step::Views);
        contacted |= done.downloaded;
        view_tables = Some(done.tables);
        // Said as soon as the views step is done, so a later step that fails
        // (a hook, the push) does not hide them once the tables are published.
        print_mismatches(&done.mismatches);
        mismatches = Some(done.mismatches);
    }
    let server = manifest::recorded_server(s, contacted);
    if let Some(t) = tiles {
        timings.push(("tiles", t.context("tiles")?));
        ran.insert(Step::Tiles);
    }

    let mut refreshed_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string();
    if steps.contains(&Step::Manifest) {
        let t = Instant::now();
        refreshed_at = manifest::write(s, &stage, server.as_deref())?;
        timings.push(("manifest", t.elapsed().as_secs_f64()));
        ran.insert(Step::Manifest);
    }

    if steps.contains(&Step::Catalog)
        && let Some(c) = &s.catalog
    {
        let t = Instant::now();
        catalog::write(s, c, &refreshed_at, &stage)?;
        timings.push(("catalog", t.elapsed().as_secs_f64()));
        ran.insert(Step::Catalog);
    }

    #[cfg(feature = "postgres")]
    if steps.contains(&Step::Database)
        && let Some(d) = &s.database
    {
        let t = Instant::now();
        let tables: Vec<(String, PathBuf)> = database_tables(s, d)?
            .into_iter()
            .map(|n| {
                let dir = stage.read(&n);
                (n, dir)
            })
            .collect();
        database::load(s, d, &tables)?;
        timings.push(("database", t.elapsed().as_secs_f64()));
        ran.insert(Step::Database);
    }

    // Everything above wrote to the staging directories; swap it all in now,
    // the tables first, then the manifest and catalog.sql.
    if !stage.is_empty()? {
        let t = Instant::now();
        println!("== publish");
        let (n, dirs) = stage.publish()?;
        let dirs: String = dirs.iter().map(|d| format!(", {d}/")).collect();
        println!("   {n} table(s){dirs}, then the manifest and catalog");
        // DuckDB reads each table's schema from its files when it creates a
        // view, so a catalog.duckdb with relative paths can only be built once
        // the tables are in place. If that fails the run stops here, with the
        // new tables, manifest and catalog.sql and the previous catalog.duckdb.
        if ran.contains(&Step::Catalog)
            && let Some(c) = &s.catalog
            && c.duckdb_file
            && c.base_url.is_none()
        {
            catalog::build_duckdb(s, c)?;
        }
        timings.push(("publish", t.elapsed().as_secs_f64()));
    }

    if steps.contains(&Step::Hooks) {
        let t = Instant::now();
        for h in &s.hooks {
            let wanted = h.after.is_none_or(|a| ran.contains(&a));
            if wanted {
                push::hook(s, h, &hook_program(s, &h.run[0]), &refreshed_at)?;
            }
        }
        timings.push(("hooks", t.elapsed().as_secs_f64()));
    }

    if steps.contains(&Step::Push) {
        let t = Instant::now();
        push::push(s)?;
        timings.push(("push", t.elapsed().as_secs_f64()));
    }

    if let Some(written) = view_tables {
        let unwritten = unwritten_tables(s, written)?;
        if !unwritten.is_empty() {
            println!(
                "note: no current view writes {}; kept, and still in the manifest and \
                 the catalog. Remove a table's directory by hand if it is no longer wanted.",
                unwritten
                    .iter()
                    .map(|t| format!("parquet/{t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    let total = started.elapsed().as_secs_f64();
    println!(
        "done → {} in {total:.1}s ({})",
        s.data.display(),
        timings
            .iter()
            .map(|(n, t)| format!("{n} {t:.1}s"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut report = json!({
        "refreshed_at": refreshed_at,
        "server": server,
        "seconds": total,
        "steps": timings.iter().map(|(n, t)| (n.to_string(), json!(t))).collect::<serde_json::Map<_, _>>(),
    });
    if s.reads_files() {
        report["files"] = json!(s.inputs());
    }
    if let Some(m) = &mismatches {
        report["type_mismatches"] = mismatch_report(m);
    }
    std::fs::write(
        s.raw_dir().join("last-refresh.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(())
}

/// Print the values the views step could not write as they are, as
/// `table.column: 3 values did not fit integer and were written as null`.
fn print_mismatches(m: &[(String, WriteSummary)]) {
    let lines: Vec<String> = m
        .iter()
        .flat_map(|(table, w)| {
            w.messages()
                .into_iter()
                .map(move |l| format!("{table}.{l}"))
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    println!("values that did not match their column's type:");
    for l in &lines {
        println!("   {l}");
    }
}

/// The views step's type mismatches for last-refresh.json: table → column →
/// its type and counts. Tables and columns without any are left out.
fn mismatch_report(m: &[(String, WriteSummary)]) -> serde_json::Value {
    let tables: serde_json::Map<String, serde_json::Value> = m
        .iter()
        .filter(|(_, w)| !w.is_empty())
        .map(|(table, w)| {
            let columns: serde_json::Map<String, serde_json::Value> = w
                .columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        json!({
                            "type": c.col_type.to_string(),
                            "written_as_date": c.written_as_date,
                            "written_as_null": c.written_as_null,
                        }),
                    )
                })
                .collect();
            (table.clone(), columns.into())
        })
        .collect();
    tables.into()
}

/// The view tables [database] loads: every table a view writes, or those
/// `tables` names, each of which must be one.
#[cfg(feature = "postgres")]
fn database_tables(s: &Settings, d: &config::Database) -> Result<Vec<String>> {
    let dir = s
        .views
        .as_ref()
        .context("[database] loads the view tables, so it needs the views directory")
        .map_err(usage)?;
    let all: BTreeSet<String> = views::discover(dir, &s.strip_prefix)
        .map_err(usage)?
        .into_iter()
        .map(|v| v.table)
        .collect();
    match &d.tables {
        None => Ok(all.into_iter().collect()),
        Some(names) => {
            if let Some(bad) = names.iter().find(|n| !all.contains(*n)) {
                return Err(usage(anyhow::anyhow!(
                    "[database] tables: `{bad}` is not a view table ({})",
                    all.iter().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
            Ok(names.clone())
        }
    }
}

/// What the views step did.
struct ViewsDone {
    /// Whether anything was downloaded from the server.
    downloaded: bool,
    /// The tables the views wrote.
    tables: BTreeSet<String>,
    /// Each table's values that did not match their column's type, in the
    /// order the tables were written.
    mismatches: Vec<(String, WriteSummary)>,
}

/// Download each resource type the views read, then run each type's views on
/// one read of it, a chunk of resources at a time, and write each view's
/// table, partitioned by the lookup rules, to the staging directory, and the
/// views to the staged views/. A type's tables are written as soon as its
/// views are done, and its rows then dropped, except that the types whose
/// views write lookup tables run first and their tables are all written
/// before any other (`write_order`).
fn views_step(s: &Settings, stage: &stage::Stage) -> Result<ViewsDone> {
    let dir = s
        .views
        .as_ref()
        .context("no views directory")
        .map_err(usage)?;
    println!("== views ({})", dir.display());
    // A missing, unreadable or invalid ViewDefinition is the operator's to fix.
    let all = views::discover(dir, &s.strip_prefix).map_err(usage)?;
    if all.is_empty() {
        return Err(usage(anyhow::anyhow!(
            "no ViewDefinitions in {}",
            dir.display()
        )));
    }
    views::save(&all, &stage.dir("views"))?;

    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, v) in all.iter().enumerate() {
        by_type.entry(v.resource.as_str()).or_default().push(i);
    }
    let ndjson = |t: &str| s.raw_dir().join(format!("{t}.ndjson"));
    let download: Vec<&str> = by_type
        .keys()
        .copied()
        .filter(|t| {
            !s.reuse_raw
                || std::fs::metadata(ndjson(t)).is_ok_and(|m| m.len() == 0)
                || !ndjson(t).exists()
        })
        .collect();
    let downloaded = !s.reads_files() && !download.is_empty();
    if downloaded {
        fhir::export(s, &download)?;
    }

    // Each type's views, run over one read of its file.
    let run_type = |rtype: &str, idx: &[usize]| -> Result<Vec<(usize, ViewResult)>> {
        let t = Instant::now();
        let vs: Vec<&views::View> = idx.iter().map(|&i| &all[i]).collect();
        // A type the files lack gives its views empty tables.
        let (n, results) = if s.reads_files() && !ndjson(rtype).exists() {
            println!("   {rtype}: none in the files; its views write empty tables");
            let rows = views::ViewRows::new(
                vs.iter()
                    .map(|v| (v.name.as_str(), &v.definition))
                    .collect(),
            );
            (0, rows.finish(s.threads)?)
        } else {
            views::run_file(&vs, &ndjson(rtype), s.threads)?
        };
        println!(
            "   {rtype}: {n} resources, {} view(s), {:.1}s",
            idx.len(),
            t.elapsed().as_secs_f64()
        );
        Ok(idx.iter().copied().zip(results).collect())
    };

    // Tables other views look their partition up in are written first, and
    // read back from the staging directory: the types whose views write them
    // run first, and their tables are written once they have all run, the
    // lookup tables first, each group in view order, as if every view had
    // run before any table was written.
    let sources: BTreeSet<&str> = s.lookups.iter().map(|l| l.table.as_str()).collect();
    let is_source = |i: usize| sources.contains(all[i].table.as_str());
    let write_order = |i: usize| (!is_source(i), i);
    let (first, rest): (Vec<_>, Vec<_>) = by_type
        .iter()
        .partition(|(_, idx)| idx.iter().any(|&i| is_source(i)));

    let mut writer = TableWriter {
        s,
        stage,
        lookups: HashMap::new(),
        mismatches: Vec::new(),
    };
    let mut held = Vec::new();
    for (rtype, idx) in first {
        held.extend(run_type(rtype, idx)?);
    }
    held.sort_by_key(|(i, _)| write_order(*i));
    for (i, result) in held {
        writer.write(i, &all[i], result)?;
    }
    for (rtype, idx) in rest {
        for (i, result) in run_type(rtype, idx)? {
            writer.write(i, &all[i], result)?;
        }
    }
    // The type mismatches in the order the tables were written before they
    // were written a type at a time.
    let mut mismatches = writer.mismatches;
    mismatches.sort_by_key(|(i, _, _)| write_order(*i));
    Ok(ViewsDone {
        downloaded,
        mismatches: mismatches.into_iter().map(|(_, t, w)| (t, w)).collect(),
        tables: all.into_iter().map(|v| v.table).collect(),
    })
}

/// Writes the views' tables to the staging directory, reading the lookup
/// tables their partitions come from once each.
struct TableWriter<'a> {
    s: &'a Settings,
    stage: &'a stage::Stage,
    /// (table, key) → key → partition value.
    lookups: HashMap<(String, String), HashMap<String, String>>,
    /// Each table written, by its view's index, with the values that did not
    /// match their column's type.
    mismatches: Vec<(usize, String, WriteSummary)>,
}

impl TableWriter<'_> {
    /// Write view `i`'s table from its rows.
    fn write(&mut self, i: usize, v: &views::View, result: ViewResult) -> Result<()> {
        let (s, stage) = (self.s, self.stage);
        // The partition column is added to every row; a view column of the
        // same name would be lost beside it.
        if result.columns.iter().any(|c| c.name == s.partition_column) {
            return Err(usage(anyhow::anyhow!(
                "view {} has a column `{}`, which is the partition column; \
                 set [partition] column to another name",
                v.name,
                s.partition_column
            )));
        }
        let rule = partition::rule_for(&result, &s.lookups);
        let mut map: Option<&HashMap<String, String>> = None;
        let mut lookup_table = None;
        if let Some((l, _)) = rule {
            let table_dir = stage.read(&l.table);
            let key = l.table_key.clone().unwrap_or_else(|| l.key.clone());
            lookup_table = Some((l, key.clone(), table_dir.is_dir()));
            if table_dir.is_dir() {
                let cache_key = (l.table.clone(), key.clone());
                if !self.lookups.contains_key(&cache_key) {
                    let m = partition::read_lookup(&table_dir, &key, &s.partition_column)
                        .with_context(|| format!("reading lookup table {}", l.table))?;
                    self.lookups.insert(cache_key.clone(), m);
                }
                map = self.lookups.get(&cache_key);
            }
        }
        let col = rule.map(|(_, c)| c);
        // Rows whose key is null, and rows whose key is not in the lookup
        // table (or is not text: the lookup's keys are read as text), for
        // the warning below.
        let no_key = Cell::new(0usize);
        let not_found = Cell::new(0usize);
        let (n, summary) =
            partition::write(result, &stage.table(&v.table), &s.partition_column, |row| {
                let c = col?;
                if row[c].is_null() {
                    no_key.set(no_key.get() + 1);
                    return None;
                }
                let found = map.and_then(|m| row[c].as_str().and_then(|k| m.get(k)).cloned());
                if found.is_none() {
                    not_found.set(not_found.get() + 1);
                }
                found
            })?;
        println!(
            "   {} ({}) → parquet/{} ({n} rows)",
            v.name, v.resource, v.table
        );
        self.mismatches.push((i, v.table.clone(), summary));
        if let Some((l, key, present)) = lookup_table {
            let (no_key, not_found) = (no_key.get(), not_found.get());
            if !present {
                println!(
                    "   warning: parquet/{}: {} of {n} rows in the null partition: there is no \
                     lookup table parquet/{} to find `{}` in",
                    v.table,
                    no_key + not_found,
                    l.table,
                    l.key
                );
            } else if no_key + not_found > 0 {
                let mut why = Vec::new();
                if not_found > 0 {
                    why.push(format!(
                        "{not_found} with a `{}` not in {}.{key}",
                        l.key, l.table
                    ));
                }
                if no_key > 0 {
                    why.push(format!("{no_key} without a `{}`", l.key));
                }
                println!(
                    "   warning: parquet/{}: {} of {n} rows in the null partition ({})",
                    v.table,
                    no_key + not_found,
                    why.join(", ")
                );
            }
        }
        Ok(())
    }
}
