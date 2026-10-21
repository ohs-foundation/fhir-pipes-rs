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
//!              raw/<server>/.changes.json (`Changes::begin`)
//!   hooks      commands from the config (e.g. a catalog build); hooks with
//!              `on = "failure"` or `"always"` also run after a run that failed
//!   push       mirror the warehouse to object storage (rclone); only with --push
//!
//! The tables, views/, tiles/, the manifest and catalog.sql are written to
//! staging directories (stage.rs) and swapped in together after the database
//! step, so a run that fails before then leaves the previous run's warehouse
//! as it was. The hooks and the push work on the published warehouse.
//!
//! A run holds the warehouse's lock (stage::Lock) from before the tools check
//! to the end, and while it holds it keeps `last-run.json` in the warehouse
//! directory up to date (status.rs): `running`, then `succeeded` or `failed`
//! with the step that failed and why.

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
mod status;
mod tiles;
mod tools;
mod views_step;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use fhir_pipes_sof::output::WriteSummary;
use serde_json::json;

use config::Settings;
use views_step::views_step;

use crate::error::usage;
use crate::export::changes::Changes;
use crate::extract::page::ServerKind;
use crate::http::TokenAuth;

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
    /// Seconds to wait for another refresh of the same warehouse to finish
    /// before failing; 0 fails at once. Default: wait as long as it takes.
    #[arg(long, env = "FHIR_PIPES_LOCK_TIMEOUT", value_name = "SECONDS")]
    lock_timeout: Option<u64>,
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
    crate::stop::install();
    let mut ran = Ran::default();
    locked(
        &s,
        &steps,
        args.lock_timeout.map(Duration::from_secs),
        &mut ran,
        |ran, lock| {
            ran.at("tools")?;
            tools::require(&now.iter().map(|t| t.pair()).collect::<Vec<_>>())?;
            refresh(
                &s,
                &steps,
                &later.iter().map(|t| t.pair()).collect::<Vec<_>>(),
                ran,
                lock,
            )
        },
    )
    .map(|_| ())
}

/// Run `f` under the warehouse's lock, with `last-run.json` saying the run
/// is running until it ends, then how it ended. When it fails, the failure
/// hooks run (with the hooks step) before the lock is released, so the
/// last-run.json they read is this run's. A run that cannot get the lock
/// writes nothing there, since the run that holds it owns the file; its
/// failure hooks run without the lock.
fn locked(
    s: &Settings,
    steps: &BTreeSet<Step>,
    timeout: Option<Duration>,
    ran: &mut Ran,
    f: impl FnOnce(&mut Ran, &stage::Lock) -> Result<String>,
) -> Result<String> {
    let hooks = steps.contains(&Step::Hooks);
    ran.label("lock");
    let lock = match stage::Lock::acquire(&s.data, timeout) {
        Ok(lock) => lock,
        Err(e) => {
            let result = Err(e);
            status::Status::unlocked(s, steps).finish(
                &result,
                ran.current,
                &ran.timings,
                ran.committed,
                ran.published,
            );
            if hooks && let Err(e) = &result {
                failure_hooks(s, ran.current, e);
            }
            return result;
        }
    };
    let result = status::Status::start(s, steps).and_then(|status| {
        let result = f(ran, &lock);
        status.finish(
            &result,
            ran.current,
            &ran.timings,
            ran.committed,
            ran.published,
        );
        result
    });
    if let Err(e) = &result
        && hooks
    {
        failure_hooks(s, ran.current, e);
    }
    drop(lock);
    result
}

/// Run the hooks with `on = "failure"` or `"always"` after a run that failed
/// at `step` with `err`. A hook that fails is reported; the run's error and
/// exit status stay the run's own.
fn failure_hooks(s: &Settings, step: &str, err: &anyhow::Error) {
    // A supervisor (fhir-pipes serve) that alerts once per run turns them
    // off for its attempts.
    if std::env::var("FHIR_PIPES_FAILURE_HOOKS").is_ok_and(|v| v == "off") {
        if s.hooks.iter().any(|h| h.on.on_failure()) {
            println!("   failure hooks are off (FHIR_PIPES_FAILURE_HOOKS=off)");
        }
        return;
    }
    let message = status::short(&crate::error::message(err));
    let code = crate::error::exit_code(err).to_string();
    for h in s.hooks.iter().filter(|h| h.on.on_failure()) {
        let env = [
            ("STATUS", "failed"),
            ("FAILED_STEP", step),
            ("ERROR", message.as_str()),
            ("EXIT_CODE", code.as_str()),
        ];
        if let Err(e) = push::hook(s, h, &hook_program(s, &h.run[0]), &env) {
            eprintln!(
                "fhir-pipes: warning: {} (after the run failed)",
                crate::error::message(&e)
            );
        }
    }
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
        for v in database::view_tables(&s, d)? {
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
        }
        // The database holds the view tables only: without the views step
        // there is nothing new to load, and a load would be in full.
        if only.contains(&Step::Views) {
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
            // A failure hook's program is not needed for the run to succeed:
            // one that cannot be started is a warning once the run fails.
            if !h.on.on_success() {
                continue;
            }
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
        match crate::views::run::discover(dir, &s.strip_prefix)
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
    /// The part of the run under way, which is the one that failed when the
    /// run fails: a step, or `lock`, `tools`, `start`, `files`, `publish`,
    /// `finish`.
    current: &'static str,
    /// Whether the database step's commit happened.
    committed: status::Committed,
    /// Whether anything live in the warehouse has changed.
    published: bool,
}

impl Ran {
    /// The run goes on to part `name`, unless a stop has been asked for
    /// (stop.rs): then it fails as stopped in the part it was in.
    fn at(&mut self, name: &'static str) -> Result<()> {
        crate::stop::check()?;
        self.label(name);
        Ok(())
    }

    /// The run is in part `name`, whatever else: for naming the part an
    /// error came from.
    fn label(&mut self, name: &'static str) {
        self.current = name;
    }

    /// Runs `f` as the part `name`. When it succeeds its time is recorded,
    /// and `step`, if given, counts as run.
    fn run_step<T>(
        &mut self,
        name: &'static str,
        step: Option<Step>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.at(name)?;
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

/// For the stop tests: with FHIR_PIPES_TEST_PAUSE set to `point`, write
/// `<data>/.paused` and wait 3 s there, inside the commit window. Debug
/// builds only.
fn test_pause(s: &Settings, point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("FHIR_PIPES_TEST_PAUSE").is_ok_and(|p| p == point) {
        let marker = s.data.join(".paused");
        let _ = std::fs::write(&marker, point);
        std::thread::sleep(Duration::from_secs(3));
        let _ = std::fs::remove_file(&marker);
    }
    #[cfg(not(debug_assertions))]
    let _ = (s, point);
}

/// `tiles_tools`: the tiles step's tools, when they are checked only once the
/// step is known to run. Returns the run's `refreshed_at`.
fn refresh(
    s: &Settings,
    steps: &BTreeSet<Step>,
    tiles_tools: &[(&str, &str)],
    ran: &mut Ran,
    lock: &stage::Lock,
) -> Result<String> {
    let started = Instant::now();
    ran.at("start")?;
    // Whether the run downloaded anything from the server, for the server
    // the manifest records.
    let mut contacted = false;
    std::fs::create_dir_all(s.raw_dir())?;
    std::fs::create_dir_all(s.parquet_dir())?;
    let stage = stage::Stage::new(s, lock)?;
    // The change log of an upsert run (export/changes.rs), which the
    // exports and the file source add to; in any other mode it is removed.
    if s.upsert() {
        Changes::begin(&s.raw_dir(), s.full)?;
    } else {
        Changes::remove(&s.raw_dir())?;
    }

    // A file source is split by type into raw/ first, as a download would
    // be; --reuse-raw reruns on the last split.
    let reads = steps.contains(&Step::Locations) || steps.contains(&Step::Views);
    if s.reads_files() && reads && !s.reuse_raw {
        ran.run_step("files", None, || files::split(s))?;
    }

    if steps.contains(&Step::Locations) {
        ran.at("locations")?;
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
        ran.at("tiles")?;
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
        ran.label("views");
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
        ran.label("tiles");
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

    // The commit window (stop.rs): from the database step, which may commit
    // the new rows, to the end of the publish, a stop is held back so the
    // files follow the database.
    #[allow(unused_mut)]
    let mut window: Option<crate::stop::Hold> = None;
    #[cfg(feature = "postgres")]
    if steps.contains(&Step::Database)
        && let Some(d) = &s.database
    {
        ran.at("database")?;
        window = Some(crate::stop::hold());
        let mut committed = status::Committed::No;
        let loaded = ran.run_step("database", Some(Step::Database), || {
            let tables: Vec<database::Table> = database::view_tables(s, d)?
                .into_iter()
                .map(|v| database::Table {
                    dir: stage.read(&v.table),
                    key: database::key_column(&v.definition),
                    time_dependent: database::time_dependent(&v.definition),
                    resource: v.resource,
                    name: v.table,
                })
                .collect();
            database::load(s, d, &tables, built.as_ref(), &mut committed)
        });
        ran.committed = committed;
        loaded?;
        test_pause(s, "database-committed");
    }

    // Everything above wrote to the staging directories; swap it all in now,
    // the tables first, then the manifest and catalog.sql.
    if !stage.is_empty()? {
        ran.at("publish")?;
        window.get_or_insert_with(crate::stop::hold);
        test_pause(s, "before-publish");
        let catalog_ran = ran.steps.contains(&Step::Catalog);
        let mut published = false;
        let swapped = ran.run_step("publish", None, || {
            println!("== publish");
            let (n, dirs) = stage.publish(&mut published)?;
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
        });
        ran.published |= published;
        swapped?;
    }
    drop(window);

    if steps.contains(&Step::Hooks) {
        let wanted: Vec<_> = s
            .hooks
            .iter()
            .filter(|h| h.on.on_success() && h.after.is_none_or(|a| ran.steps.contains(&a)))
            .collect();
        let env = [
            ("STATUS", "succeeded"),
            ("REFRESHED_AT", refreshed_at.as_str()),
        ];
        ran.run_step("hooks", None, || {
            for h in wanted {
                push::hook(s, h, &hook_program(s, &h.run[0]), &env)?;
            }
            Ok(())
        })?;
    }

    if steps.contains(&Step::Push) {
        ran.run_step("push", None, || push::push(s))?;
    }

    ran.at("finish")?;
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
    if s.upsert() {
        Changes::finish(&s.raw_dir())?;
    }
    Ok(refreshed_at)
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn steps_of(args: &[&str]) -> BTreeSet<Step> {
        let cli =
            crate::cli::Cli::try_parse_from(["fhir-pipes", "refresh"].iter().chain(args).copied())
                .unwrap();
        let crate::cli::Command::Refresh(r) = cli.command else {
            unreachable!()
        };
        steps(&r)
    }

    #[test]
    fn the_database_step_follows_the_views_step() {
        assert!(steps_of(&[]).contains(&Step::Database));
        assert!(steps_of(&["--only", "views"]).contains(&Step::Database));
        for only in ["locations", "tiles"] {
            let s = steps_of(&["--only", only]);
            assert!(!s.contains(&Step::Database), "{only}");
            assert!(s.contains(&Step::Catalog), "{only}");
        }
        assert!(steps_of(&["--only", "database"]).contains(&Step::Database));
    }

    #[test]
    fn push_is_asked_for_and_skip_has_the_last_word() {
        let all = steps_of(&[]);
        assert!(!all.contains(&Step::Push));
        assert_eq!(all.len(), 7);
        assert!(steps_of(&["--push"]).contains(&Step::Push));
        let s = steps_of(&["--push", "--skip", "tiles,push,catalog"]);
        assert!(!s.contains(&Step::Push));
        assert!(!s.contains(&Step::Tiles));
        assert!(!s.contains(&Step::Catalog));
        // --skip takes out what --only brings in.
        let s = steps_of(&["--only", "views", "--skip", "database"]);
        assert_eq!(
            s,
            [Step::Views, Step::Manifest, Step::Catalog]
                .into_iter()
                .collect()
        );
        // Steps that write no data do not bring the manifest.
        assert_eq!(
            steps_of(&["--only", "hooks"]),
            [Step::Hooks].into_iter().collect()
        );
    }

    /// Settings from the config `text`, in `/cfg`.
    fn settings(text: &str) -> Settings {
        let file = toml::from_str(&format!("data = 'data'\nviews = 'views'\n{text}")).unwrap();
        config::settings(file, PathBuf::from("/cfg"), Default::default()).unwrap()
    }

    /// (program, step, purpose) of each tool the steps need.
    fn tools(s: &Settings, steps: &[Step]) -> Result<Vec<(String, Step, String)>> {
        Ok(required_tools(s, &steps.iter().copied().collect())?
            .into_iter()
            .map(|t| (t.program, t.step, t.purpose))
            .collect())
    }

    #[test]
    fn each_step_needs_its_tools() {
        let s = settings(
            "[tools]\nduckdb = '/opt/duckdb'\n\
             [push]\nremote = 'r'\nbucket = 'b'\n\
             [[hooks]]\nname = 'build'\nrun = ['bin/build', '--all']\n\
             [[hooks]]\nname = 'notify'\nrun = ['curl']\n",
        );
        let t =
            |program: &str, step, purpose: &str| (program.to_string(), step, purpose.to_string());
        assert_eq!(tools(&s, &[Step::Views, Step::Manifest]).unwrap(), []);
        assert_eq!(
            tools(&s, &[Step::Tiles, Step::Push, Step::Hooks]).unwrap(),
            [
                t("/opt/duckdb", Step::Tiles, "tiles"),
                t("tippecanoe", Step::Tiles, "tiles"),
                t("rclone", Step::Push, "push"),
                // A program path is the config file's, a name is looked up.
                t(
                    &PathBuf::from("/cfg")
                        .join("bin/build")
                        .display()
                        .to_string(),
                    Step::Hooks,
                    "hook build"
                ),
                t("curl", Step::Hooks, "hook notify"),
            ]
        );
    }

    #[test]
    fn catalog_duckdb_is_built_at_publish_or_in_the_push() {
        let local = settings("[catalog]\nduckdb_file = true\n");
        assert_eq!(
            tools(&local, &[Step::Catalog]).unwrap(),
            [("duckdb".into(), Step::Catalog, "catalog.duckdb".into())]
        );
        let served = settings(
            "[catalog]\nduckdb_file = true\nbase_url = 'https://x.example'\n\
             [push]\nremote = 'r'\nbucket = 'b'\n",
        );
        assert_eq!(tools(&served, &[Step::Catalog]).unwrap(), []);
        assert_eq!(
            tools(&served, &[Step::Catalog, Step::Push]).unwrap(),
            [
                ("duckdb".into(), Step::Push, "catalog.duckdb".into()),
                ("rclone".into(), Step::Push, "push".into()),
            ]
        );
    }

    #[test]
    fn steps_without_their_settings_are_refused() {
        let err = |s: &Settings, steps: &[Step]| tools(s, steps).unwrap_err().to_string();
        assert!(err(&settings(""), &[Step::Push]).starts_with("--push needs a [push] section"));
        assert_eq!(
            err(
                &settings("[[hooks]]\nname = 'h'\nrun = []\n"),
                &[Step::Hooks]
            ),
            "hook h has an empty `run`"
        );
        let file = toml::from_str("data = 'data'").unwrap();
        let no_views = config::settings(file, PathBuf::from("/cfg"), Default::default()).unwrap();
        assert!(err(&no_views, &[Step::Views]).starts_with("no views directory"));
        // Without the views step the views directory is not needed.
        tools(&no_views, &[Step::Locations]).unwrap();
    }

    #[test]
    fn the_partition_line_names_the_lookups() {
        assert_eq!(
            partition_line("country", &[]),
            "country, no [[partition.lookup]] rules: view rows go to the null partition"
        );
        let s = settings(
            "[[partition.lookup]]\nkey = 'location_id'\ntable = 'locations'\n\
             [[partition.lookup]]\nkey = 'patient_id'\ntable = 'patients'\n",
        );
        assert_eq!(
            partition_line("country", &s.lookups),
            "country via location_id → locations, patient_id → patients"
        );
    }

    #[test]
    fn unwritten_tables_leave_out_locations_and_lookup_tables() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = settings("[[partition.lookup]]\nkey = 'patient_id'\ntable = 'patients'\n");
        s.data = dir.path().to_path_buf();
        for t in ["obs", "old", "locations", "patients"] {
            std::fs::create_dir_all(s.parquet_dir().join(t)).unwrap();
        }
        let written = ["obs".to_string()].into_iter().collect();
        assert_eq!(unwritten_tables(&s, written).unwrap(), ["old"]);
    }

    #[test]
    fn the_mismatch_report_lists_only_tables_with_mismatches() {
        use fhir_pipes_sof::ColumnType;
        use fhir_pipes_sof::output::ColumnSummary;
        let summary = WriteSummary {
            columns: vec![ColumnSummary {
                name: "born".into(),
                col_type: ColumnType::Date,
                written_as_date: 2,
                written_as_null: 1,
            }],
        };
        let m = vec![
            ("clean".to_string(), WriteSummary::default()),
            ("patients".to_string(), summary),
        ];
        assert_eq!(
            mismatch_report(&m),
            json!({"patients": {"born": {
                "type": "date", "written_as_date": 2, "written_as_null": 1
            }}})
        );
        assert_eq!(mismatch_report(&[]), json!({}));
    }

    #[test]
    fn a_step_counts_as_run_only_when_it_succeeds() {
        let mut ran = Ran::default();
        assert_eq!(
            ran.run_step("views", Some(Step::Views), || Ok(3)).unwrap(),
            3
        );
        assert!(
            ran.run_step("tiles", Some(Step::Tiles), || -> Result<()> { bail!("no") })
                .is_err()
        );
        ran.run_step("publish", None, || Ok(())).unwrap();
        let names: Vec<&str> = ran.timings.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["views", "publish"]);
        assert_eq!(ran.steps, [Step::Views].into_iter().collect());
    }
}
