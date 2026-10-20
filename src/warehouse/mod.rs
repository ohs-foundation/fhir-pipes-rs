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
//!              build with the postgres feature. With mode = "upsert", only
//!              the rows of the resources that changed, from the change log
//!              raw/<server>/.changes.json (`begin_changes`)
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
mod sql;
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
use crate::extract::client::TokenAuth;
use crate::extract::page::ServerKind;

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
    /// The kind of FHIR server, for its search parameters (as `server.kind`):
    /// medplum pages with `_sort=_lastUpdated`, aidbox with `_total=none`;
    /// generic (the default) and hapi add nothing.
    #[arg(long, env = "FHIR_PIPES_SERVER_KIND", value_enum)]
    server_kind: Option<ServerKind>,
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
    /// How the client id and secret are sent to the token URL: `post` in
    /// the form body (the default) or `basic` in a Basic Authorization
    /// header, as Aidbox expects (as `server.token_auth`).
    #[arg(long, env = "FHIR_PIPES_TOKEN_AUTH", value_enum)]
    token_auth: Option<TokenAuth>,
    /// The OAuth scope to ask for with the client id (as `server.scope`).
    #[arg(long, env = "FHIR_PIPES_SCOPE")]
    scope: Option<String>,
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
    let (later, now): (Vec<_>, Vec<_>) = needed
        .iter()
        .partition(|t| s.reads_files() && t.step == Step::Tiles);
    tools::require(&now.iter().map(|t| t.pair()).collect::<Vec<_>>())?;
    refresh(
        &s,
        &steps,
        &later.iter().map(|t| t.pair()).collect::<Vec<_>>(),
    )
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
            server_kind: args.server_kind,
            token: args.token.clone(),
            client_id: env_secret("FHIR_PIPES_CLIENT_ID"),
            client_secret: env_secret("FHIR_PIPES_CLIENT_SECRET"),
            token_url: args.token_url.clone(),
            token_auth: args.token_auth,
            scope: args.scope.clone(),
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
    // Upsert finds a resource's rows by its view's getResourceKey() column:
    // a view without one is refused before anything is downloaded.
    #[cfg(feature = "postgres")]
    if steps.contains(&Step::Database)
        && s.upsert()
        && let Some(d) = &s.database
    {
        for v in database_tables(&s, d)? {
            if database::key_column(&v.definition).is_none() {
                bail!(database::no_key(&v.name, &v.table));
            }
        }
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

/// An external tool a step needs.
#[derive(Debug)]
struct Tool {
    program: String,
    /// The step that runs it.
    step: Step,
    /// What it is for, as messages name it: `tiles`, `catalog.duckdb`,
    /// `hook <name>`.
    purpose: String,
}

impl Tool {
    /// (program, purpose), as `tools::require` takes them.
    fn pair(&self) -> (&str, &str) {
        (&self.program, &self.purpose)
    }
}

type Tools = Vec<Tool>;

/// The external tools the chosen steps need, each with its step.
fn required_tools(s: &Settings, steps: &BTreeSet<Step>) -> Result<Tools> {
    let mut t = Vec::new();
    let mut need = |program: &str, step: Step, purpose: String| {
        t.push(Tool {
            program: program.to_string(),
            step,
            purpose,
        });
    };
    if steps.contains(&Step::Views) && s.views.is_none() {
        bail!(
            "no views directory: pass --views, set FHIR_PIPES_VIEWS, or set `views` in the config"
        );
    }
    if steps.contains(&Step::Tiles) {
        need(&s.duckdb, Step::Tiles, "tiles".into());
        need(&s.tippecanoe, Step::Tiles, "tiles".into());
    }
    if let Some(c) = &s.catalog
        && c.duckdb_file
        && (steps.contains(&Step::Push) || (steps.contains(&Step::Catalog) && c.base_url.is_none()))
    {
        // With a base_url it is built during the push, else at publish
        // after the catalog step.
        let step = if c.base_url.is_some() {
            Step::Push
        } else {
            Step::Catalog
        };
        need(&s.duckdb, step, "catalog.duckdb".into());
    }
    if steps.contains(&Step::Push) {
        if s.push.is_none() {
            bail!("--push needs a [push] section (remote, bucket) in the config");
        }
        need(&s.rclone, Step::Push, "push".into());
    }
    if steps.contains(&Step::Hooks) {
        for h in &s.hooks {
            let Some(program) = h.run.first() else {
                bail!("hook {} has an empty `run`", h.name);
            };
            need(
                &hook_program(s, program),
                Step::Hooks,
                format!("hook {}", h.name),
            );
        }
    }
    Ok(t)
}

/// The `partition:` line of `--check`: the column and the lookups that fill
/// it. Without a lookup every view row goes to the null partition.
fn partition_line(column: &str, lookups: &[config::Lookup]) -> String {
    if lookups.is_empty() {
        return format!(
            "{column}, no [[partition.lookup]] rules: view rows go to the null partition"
        );
    }
    let via: Vec<String> = lookups
        .iter()
        .map(|l| format!("{} → {}", l.key, l.table))
        .collect();
    format!("{column} via {}", via.join(", "))
}

fn check(s: &Settings, steps: &BTreeSet<Step>, needed: &[Tool]) {
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
        println!(
            "server:     {} (raw/{})",
            crate::redact::url(&s.server),
            s.server_name
        );
        println!("kind:       {}", s.server_kind.name());
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
        "partition:  {}",
        partition_line(&s.partition_column, &s.lookups)
    );
    if let Some(d) = &s.database {
        println!(
            "database:   ${} → schema {}{}{}",
            d.url_env,
            d.schema,
            if d.mode == config::Mode::Upsert {
                ", upsert"
            } else {
                ""
            },
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
    for Tool {
        program, purpose, ..
    } in needed
    {
        let found =
            tools::find(program).map_or("NOT FOUND".to_string(), |p| p.display().to_string());
        println!("tool:       {program} ({purpose}): {found}");
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

/// How long each part of a run took, in order, and which steps ran (what a
/// hook's `after` names).
#[derive(Default)]
struct Ran {
    timings: Vec<(&'static str, f64)>,
    steps: BTreeSet<Step>,
}

impl Ran {
    /// Runs `f` as the part `name`. When it succeeds its time is recorded,
    /// and `step`, if given, counts as run.
    fn run_step<T>(
        &mut self,
        name: &'static str,
        step: Option<Step>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let t = Instant::now();
        let value = f()?;
        self.record(name, step, t.elapsed().as_secs_f64());
        Ok(value)
    }

    /// Records a part timed elsewhere.
    fn record(&mut self, name: &'static str, step: Option<Step>, secs: f64) {
        self.timings.push((name, secs));
        if let Some(step) = step {
            self.steps.insert(step);
        }
    }
}

/// `tiles_tools`: the tiles step's tools, when they are checked only once the
/// step is known to run.
fn refresh(s: &Settings, steps: &BTreeSet<Step>, tiles_tools: &[(&str, &str)]) -> Result<()> {
    let started = Instant::now();
    let mut ran = Ran::default();
    // Whether the run downloaded anything from the server, for the server
    // the manifest records.
    let mut contacted = false;
    std::fs::create_dir_all(s.raw_dir())?;
    std::fs::create_dir_all(s.parquet_dir())?;
    let stage = stage::Stage::new(s)?;
    begin_changes(s)?;

    // A file source is split by type into raw/ first, as a download would
    // be; --reuse-raw reruns on the last split.
    let reads = steps.contains(&Step::Locations) || steps.contains(&Step::Views);
    if s.reads_files() && reads && !s.reuse_raw {
        ran.run_step("files", None, || files::split(s))?;
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
            ran.record(
                "locations",
                Some(Step::Locations),
                t.elapsed().as_secs_f64(),
            );
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
    // What the views step built, for an upsert.
    #[cfg(feature = "postgres")]
    let mut built = None;
    if let Some(v) = views {
        let (done, secs) = v.context("views")?;
        ran.record("views", Some(Step::Views), secs);
        contacted |= done.downloaded;
        view_tables = Some(done.tables);
        #[cfg(feature = "postgres")]
        {
            built = Some(done.built);
        }
        // Said as soon as the views step is done, so a later step that fails
        // (a hook, the push) does not hide them once the tables are published.
        print_mismatches(&done.mismatches);
        mismatches = Some(done.mismatches);
    }
    let server = manifest::recorded_server(s, contacted);
    if let Some(t) = tiles {
        ran.record("tiles", Some(Step::Tiles), t.context("tiles")?);
    }

    let mut refreshed_at = manifest::now();
    if steps.contains(&Step::Manifest) {
        refreshed_at = ran.run_step("manifest", Some(Step::Manifest), || {
            manifest::write(s, &stage, server.as_deref())
        })?;
    }

    if steps.contains(&Step::Catalog)
        && let Some(c) = &s.catalog
    {
        ran.run_step("catalog", Some(Step::Catalog), || {
            catalog::write(s, c, &refreshed_at, &stage)
        })?;
    }

    #[cfg(feature = "postgres")]
    if steps.contains(&Step::Database)
        && let Some(d) = &s.database
    {
        ran.run_step("database", Some(Step::Database), || {
            let tables: Vec<database::Table> = database_tables(s, d)?
                .into_iter()
                .map(|v| database::Table {
                    dir: stage.read(&v.table),
                    key: database::key_column(&v.definition),
                    time_dependent: database::time_dependent(&v.definition),
                    resource: v.resource,
                    name: v.table,
                })
                .collect();
            database::load(s, d, &tables, built.as_ref())
        })?;
    }

    // Everything above wrote to the staging directories; swap it all in now,
    // the tables first, then the manifest and catalog.sql.
    if !stage.is_empty()? {
        let catalog_ran = ran.steps.contains(&Step::Catalog);
        ran.run_step("publish", None, || {
            println!("== publish");
            let (n, dirs) = stage.publish()?;
            let dirs: String = dirs.iter().map(|d| format!(", {d}/")).collect();
            println!("   {n} table(s){dirs}, then the manifest and catalog");
            // DuckDB reads each table's schema from its files when it creates
            // a view, so a catalog.duckdb with relative paths can only be
            // built once the tables are in place. If that fails the run stops
            // here, with the new tables, manifest and catalog.sql and the
            // previous catalog.duckdb.
            if catalog_ran
                && let Some(c) = &s.catalog
                && c.duckdb_file
                && c.base_url.is_none()
            {
                catalog::build_duckdb(s, c)?;
            }
            Ok(())
        })?;
    }

    if steps.contains(&Step::Hooks) {
        let wanted: Vec<_> = s
            .hooks
            .iter()
            .filter(|h| h.after.is_none_or(|a| ran.steps.contains(&a)))
            .collect();
        ran.run_step("hooks", None, || {
            for h in wanted {
                push::hook(s, h, &hook_program(s, &h.run[0]), &refreshed_at)?;
            }
            Ok(())
        })?;
    }

    if steps.contains(&Step::Push) {
        ran.run_step("push", None, || push::push(s))?;
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
        ran.timings
            .iter()
            .map(|(n, t)| format!("{n} {t:.1}s"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut report = json!({
        "refreshed_at": refreshed_at,
        "server": server,
        "seconds": total,
        "steps": ran.timings.iter().map(|(n, t)| (n.to_string(), json!(t))).collect::<serde_json::Map<_, _>>(),
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
    finish_changes(s)?;
    Ok(())
}

/// Start the change log of an upsert run (export/changes.rs), which the
/// exports and the file source add to. A log still marked running is from a
/// refresh that did not finish: what it lists may not match what the
/// database holds, so the next load is in full, as with `--full` and when
/// there is no log. In any other mode the log is removed, so that a later
/// upsert run starts with a full load.
fn begin_changes(s: &Settings) -> Result<()> {
    use crate::export::changes::{Changes, FILE};
    let raw = s.raw_dir();
    if !s.upsert() {
        let path = raw.join(FILE);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        return Ok(());
    }
    let mut log = match Changes::read(&raw) {
        Ok(Some(log)) => log,
        Ok(None) => Changes {
            reload: Some("no record of the last load".into()),
            ..Changes::default()
        },
        Err(why) => Changes {
            reload: Some(why),
            ..Changes::default()
        },
    };
    if log.running && log.reload.is_none() {
        log.reload = Some("the last refresh did not finish".into());
    }
    if s.full {
        log.reload = Some("--full".into());
    }
    if log.reload.is_some() {
        log.types.clear();
    }
    log.running = true;
    log.write(&raw)?;
    Ok(())
}

/// The end of a refresh that finished: its change log is no longer running.
fn finish_changes(s: &Settings) -> Result<()> {
    use crate::export::changes::Changes;
    if !s.upsert() {
        return Ok(());
    }
    let raw = s.raw_dir();
    if let Ok(Some(mut log)) = Changes::read(&raw) {
        log.running = false;
        log.write(&raw)?;
    }
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

/// The views whose tables [database] loads: every view's, sorted by table,
/// or those of the tables `tables` names, in its order, each of which must
/// be a view's.
#[cfg(feature = "postgres")]
fn database_tables(s: &Settings, d: &config::Database) -> Result<Vec<views::View>> {
    let dir = s
        .views
        .as_ref()
        .context("[database] loads the view tables, so it needs the views directory")
        .map_err(usage)?;
    let mut all: BTreeMap<String, views::View> = views::discover(dir, &s.strip_prefix)
        .map_err(usage)?
        .into_iter()
        .map(|v| (v.table.clone(), v))
        .collect();
    match &d.tables {
        None => Ok(all.into_values().collect()),
        Some(names) => {
            if let Some(bad) = names.iter().find(|n| !all.contains_key(*n)) {
                return Err(usage(anyhow::anyhow!(
                    "[database] tables: `{bad}` is not a view table ({})",
                    all.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
            Ok(names.iter().filter_map(|n| all.remove(n)).collect())
        }
    }
}

/// What the views step did.
struct ViewsDone {
    /// Whether anything was downloaded from the server.
    downloaded: bool,
    /// The tables the views wrote.
    tables: BTreeSet<String>,
    /// For an upsert: each table with what its rows were built from
    /// (`fingerprint`).
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    built: BTreeMap<String, String>,
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
        digests: HashMap::new(),
        built: BTreeMap::new(),
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
        built: writer.built,
        downloaded,
        mismatches: mismatches.into_iter().map(|(_, t, w)| (t, w)).collect(),
        tables: all.into_iter().map(|v| v.table).collect(),
    })
}

/// What a table's rows are built from besides its resources, for an upsert,
/// which rewrites only the rows of resources that changed: its view, its
/// partition column and lookup (`lookup`, with a digest of the lookup's
/// keys and values), and the fhir-pipes version, whose views engine could
/// give other rows. When it differs from the last load's, the table is
/// loaded in full.
fn fingerprint(v: &views::View, partition_column: &str, lookup: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [
        env!("CARGO_PKG_VERSION"),
        &v.json.to_string(),
        partition_column,
        lookup,
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

/// A digest of a lookup: its keys and values, sorted.
fn lookup_digest(m: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<(&String, &String)> = m.iter().collect();
    entries.sort();
    let mut h = Sha256::new();
    for (k, v) in entries {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes the views' tables to the staging directory, reading the lookup
/// tables their partitions come from once each.
struct TableWriter<'a> {
    s: &'a Settings,
    stage: &'a stage::Stage,
    /// (table, key) → key → partition value.
    lookups: HashMap<(String, String), HashMap<String, String>>,
    /// (table, key) → a digest of that lookup, for an upsert.
    digests: HashMap<(String, String), String>,
    /// Each table written, with its fingerprint, for an upsert.
    built: BTreeMap<String, String>,
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
                    if s.upsert() {
                        self.digests.insert(cache_key.clone(), lookup_digest(&m));
                    }
                    self.lookups.insert(cache_key.clone(), m);
                }
                map = self.lookups.get(&cache_key);
            }
        }
        if s.upsert() {
            let lookup = match &lookup_table {
                None => "none".to_string(),
                Some((l, key, present)) => format!(
                    "{} in {}.{}: {}",
                    l.key,
                    l.table,
                    key,
                    if *present {
                        &self.digests[&(l.table.clone(), key.clone())]
                    } else {
                        "no table"
                    }
                ),
            };
            self.built.insert(
                v.table.clone(),
                fingerprint(v, &s.partition_column, &lookup),
            );
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
