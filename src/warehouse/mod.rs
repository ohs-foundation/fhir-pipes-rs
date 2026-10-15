//! `fhir-pipes refresh`: refresh a SQL-on-FHIR Parquet warehouse from a FHIR
//! server. This was fhir-warehouse, which ran kiln as a separate program; the
//! FHIR side now runs in process.
//!
//! Steps, in order:
//!   locations  extract and transform the Location registry → parquet/locations
//!   views      export each resource type the views read (only what changed
//!              since the last refresh), run the views in memory
//!              (fhir-pipes-sof), partition each table by a looked-up column
//!   tiles      the registry → PMTiles (duckdb + tippecanoe); runs beside views
//!   manifest   manifest.json: row counts, views, the extract state
//!   catalog    catalog.sql: a DuckDB view per table; only with [catalog]
//!   database   the view tables into PostgreSQL; only with [database] and a
//!              build with the postgres feature
//!   hooks      commands from the config (e.g. a catalog build)
//!   push       mirror the warehouse to object storage (rclone); only with --push

mod catalog;
mod config;
#[cfg(feature = "postgres")]
mod database;
mod fhir;
mod manifest;
mod partition;
mod push;
mod tiles;
mod tools;
pub(crate) mod views;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde_json::json;

use config::Settings;

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
    /// FHIR server base URL.
    #[arg(long, env = "FHIR_PIPES_SERVER")]
    server: Option<String>,
    /// Name for the server's raw downloads: raw/<name>/.
    #[arg(long, env = "FHIR_PIPES_SERVER_NAME")]
    server_name: Option<String>,
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
    #[arg(long)]
    threads: Option<usize>,
    /// Reuse downloaded NDJSON where present instead of downloading again.
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
    let steps = steps(args);
    let (file, base) = config::load(args.config.as_deref())?;
    let s = config::settings(
        file,
        base,
        config::Overrides {
            profile: profile.map(str::to_string),
            data: args.data.clone(),
            views: args.views.clone(),
            server: args.server.clone(),
            server_name: args.server_name.clone(),
            token: args.token.clone(),
            client_id: env_secret("FHIR_PIPES_CLIENT_ID"),
            client_secret: env_secret("FHIR_PIPES_CLIENT_SECRET"),
            token_url: args.token_url.clone(),
            username: args.username.clone(),
            password: env_secret("FHIR_PIPES_PASSWORD"),
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
    if args.check {
        check(&s, &steps, &needed);
        return Ok(());
    }
    let needed: Vec<(&str, &str)> = needed
        .iter()
        .map(|(t, w)| (t.as_str(), w.as_str()))
        .collect();
    tools::require(&needed)?;
    refresh(&s, &steps)
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

/// The external tools the chosen steps need, with what each is for.
fn required_tools(s: &Settings, steps: &BTreeSet<Step>) -> Result<Vec<(String, String)>> {
    let mut t = Vec::new();
    if steps.contains(&Step::Views) && s.views.is_none() {
        bail!("no views directory: pass --views, set VIEWS, or set `views` in the config");
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
            if let Some(after) = &h.after
                && Step::from_str(after, true).is_err()
            {
                bail!("hook {}: unknown step `{after}` in `after`", h.name);
            }
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
    println!("server:     {} (raw/{})", s.server, s.server_name);
    println!("auth:       {}", s.auth_kind());
    match s.fetch {
        config::Fetch::Bulk => println!(
            "fetch:      bulk ($export, waiting up to {} s)",
            s.bulk_timeout
        ),
        f => println!("fetch:      {}", f.name()),
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
    }
    for (tool, why) in needed {
        let found = tools::find(tool).map_or("NOT FOUND".to_string(), |p| p.display().to_string());
        println!("tool:       {tool} ({why}): {found}");
    }
}

fn refresh(s: &Settings, steps: &BTreeSet<Step>) -> Result<()> {
    let started = Instant::now();
    let mut timings: Vec<(&str, f64)> = Vec::new();
    let mut ran: BTreeSet<Step> = BTreeSet::new();
    std::fs::create_dir_all(s.raw_dir())?;
    std::fs::create_dir_all(s.parquet_dir())?;

    if steps.contains(&Step::Locations) {
        let t = Instant::now();
        fhir::locations(s)?;
        timings.push(("locations", t.elapsed().as_secs_f64()));
        ran.insert(Step::Locations);
    }

    // Views and tiles both read the location registry and nothing of each
    // other's, so they run side by side.
    let (views, tiles) = std::thread::scope(|scope| {
        let tiles = steps.contains(&Step::Tiles).then(|| {
            scope.spawn(|| {
                let t = Instant::now();
                tiles::build(s).map(|()| t.elapsed().as_secs_f64())
            })
        });
        let views = steps.contains(&Step::Views).then(|| {
            let t = Instant::now();
            views_step(s).map(|()| t.elapsed().as_secs_f64())
        });
        let tiles = tiles.map(|h| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("tiles thread panicked")))
        });
        (views, tiles)
    });
    if let Some(v) = views {
        timings.push(("views", v.context("views")?));
        ran.insert(Step::Views);
    }
    if let Some(t) = tiles {
        timings.push(("tiles", t.context("tiles")?));
        ran.insert(Step::Tiles);
    }

    let mut refreshed_at = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string();
    if steps.contains(&Step::Manifest) {
        let t = Instant::now();
        refreshed_at = manifest::write(s)?;
        timings.push(("manifest", t.elapsed().as_secs_f64()));
        ran.insert(Step::Manifest);
    }

    if steps.contains(&Step::Catalog)
        && let Some(c) = &s.catalog
    {
        let t = Instant::now();
        catalog::write(s, c, &refreshed_at)?;
        timings.push(("catalog", t.elapsed().as_secs_f64()));
        ran.insert(Step::Catalog);
    }

    #[cfg(feature = "postgres")]
    if steps.contains(&Step::Database)
        && let Some(d) = &s.database
    {
        let t = Instant::now();
        database::load(s, d, &database_tables(s, d)?)?;
        timings.push(("database", t.elapsed().as_secs_f64()));
        ran.insert(Step::Database);
    }

    if steps.contains(&Step::Hooks) {
        let t = Instant::now();
        for h in &s.hooks {
            let wanted = h
                .after
                .as_deref()
                .is_none_or(|a| ran.iter().any(|r| r.name() == a));
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
    let report = json!({
        "refreshed_at": refreshed_at,
        "server": s.server,
        "seconds": total,
        "steps": timings.iter().map(|(n, t)| (n.to_string(), json!(t))).collect::<serde_json::Map<_, _>>(),
    });
    std::fs::write(
        s.raw_dir().join("last-refresh.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(())
}

/// The view tables [database] loads: every table a view writes, or those
/// `tables` names, each of which must be one.
#[cfg(feature = "postgres")]
fn database_tables(s: &Settings, d: &config::Database) -> Result<Vec<String>> {
    let dir = s
        .views
        .as_ref()
        .context("[database] loads the view tables, so it needs the views directory")?;
    let all: BTreeSet<String> = views::discover(dir, &s.strip_prefix)?
        .into_iter()
        .map(|v| v.table)
        .collect();
    match &d.tables {
        None => Ok(all.into_iter().collect()),
        Some(names) => {
            if let Some(bad) = names.iter().find(|n| !all.contains(*n)) {
                bail!(
                    "[database] tables: `{bad}` is not a view table ({})",
                    all.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(names.clone())
        }
    }
}

/// Download each resource type the views read, run each type's views on one
/// read of it, then write each view's table partitioned by the lookup rules.
fn views_step(s: &Settings) -> Result<()> {
    let dir = s.views.as_ref().context("no views directory")?;
    println!("== views ({})", dir.display());
    let all = views::discover(dir, &s.strip_prefix)?;
    if all.is_empty() {
        bail!("no ViewDefinitions in {}", dir.display());
    }
    views::save(&all, &s.views_out_dir())?;

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
    fhir::export(s, &download)?;

    let mut results: Vec<Option<fhir_pipes_sof::ViewResult>> = all.iter().map(|_| None).collect();
    for (rtype, idx) in &by_type {
        let t = Instant::now();
        let resources = views::read_ndjson(&ndjson(rtype), s.threads)?;
        let vs: Vec<&views::View> = idx.iter().map(|&i| &all[i]).collect();
        for (&i, r) in idx.iter().zip(views::run(&vs, &resources, s.threads)?) {
            results[i] = Some(r);
        }
        println!(
            "   {rtype}: {} resources, {} view(s), {:.1}s",
            resources.len(),
            idx.len(),
            t.elapsed().as_secs_f64()
        );
    }

    // Tables other views look their partition up in are written first.
    let sources: BTreeSet<&str> = s.lookups.iter().map(|l| l.table.as_str()).collect();
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_by_key(|&i| !sources.contains(all[i].table.as_str()));

    let mut lookups: HashMap<(String, String), HashMap<String, String>> = HashMap::new();
    for i in order {
        let v = &all[i];
        let result = results[i].take().expect("every view ran");
        let rule = partition::rule_for(&result, &s.lookups);
        let mut map: Option<&HashMap<String, String>> = None;
        if let Some((l, _)) = rule {
            let table_dir = s.parquet_dir().join(&l.table);
            let key = l.table_key.clone().unwrap_or_else(|| l.key.clone());
            if table_dir.is_dir() {
                let cache_key = (l.table.clone(), key.clone());
                if !lookups.contains_key(&cache_key) {
                    let m = partition::read_lookup(&table_dir, &key, &s.partition_column)
                        .with_context(|| format!("reading lookup table {}", l.table))?;
                    lookups.insert(cache_key.clone(), m);
                }
                map = lookups.get(&cache_key);
            }
        }
        let col = rule.map(|(_, c)| c);
        let n = partition::write(
            result,
            &s.parquet_dir().join(&v.table),
            &s.partition_column,
            |row| {
                let (Some(map), Some(c)) = (map, col) else {
                    return None;
                };
                row[c].as_str().and_then(|k| map.get(k)).cloned()
            },
        )?;
        println!(
            "   {} ({}) → parquet/{} ({n} rows)",
            v.name, v.resource, v.table
        );
    }
    Ok(())
}
