//! The settings file of one DHIS2 instance (`--config FILE`, or
//! `$FHIR_PIPES_DHIS2_CONFIG`): the server, the instance name and what to
//! export, written down once instead of on every command line.
//!
//! It follows `fhir-pipes.toml` (src/warehouse/config.rs): keys in
//! snake_case, an unknown key is an error, a relative path is relative to
//! the file's folder, and no password or token is read from it. An option
//! given on the command line, or through its environment variable,
//! overrides the file's value; a repeatable option replaces the file's list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use clap::ValueEnum;
use clap::parser::ValueSource;
use serde::{Deserialize, Serialize};

use crate::cli::{
    ConfigCheckArgs, Dhis2Command, Dhis2Conn, Dhis2ExtractCommand, Dhis2Package, ExtractAllArgs,
    IndicatorSelection, LocationsOptions, ReportSource, ReportsOptions, Step, parse_uid,
};
use fhir_pipes::error::{PipesError, Result};

/// The file. Every key is optional; see docs/dhis2.md for the table of
/// keys and the options they stand for.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// `--dhis2`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `--instance`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// `--dhis2-user`. The password is never in the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `--retries`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<usize>,
    /// `--timeout`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// `--package`, for `dhis2 locations|measures|reports`; relative to the
    /// file's folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<PathBuf>,
    /// `--param NAME=VALUE`, one key per parameter.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub locations: LocationsKeys,
    #[serde(default)]
    pub indicators: IndicatorKeys,
    #[serde(default)]
    pub reports: ReportsKeys,
}

/// `[locations]`: the options of `extract locations`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocationsKeys {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facility_level: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_sets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_unit_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_org_unit_groups: bool,
}

/// `[indicators]`: what `measures` and `reports` select.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndicatorKeys {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indicator_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indicators: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_element_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_elements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_element_group_sets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_sets: Vec<String>,
}

impl IndicatorKeys {
    fn selection(&self) -> IndicatorSelection {
        IndicatorSelection {
            groups: self.indicator_groups.clone(),
            indicators: self.indicators.clone(),
            data_element_groups: self.data_element_groups.clone(),
            data_elements: self.data_elements.clone(),
            data_element_group_sets: self.data_element_group_sets.clone(),
            data_sets: self.data_sets.clone(),
        }
    }
}

/// `[reports]`: the options of `extract reports` that describe what to ask
/// for. `--resume` and `--report` are per run, and not here.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportsKeys {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub periods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdowns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_analytics_age: Option<String>,
}

/// A settings file, read and checked.
#[derive(Debug, Clone)]
pub struct Config {
    /// The file, as an absolute path.
    pub path: PathBuf,
    /// Its folder, which relative paths in it are resolved against.
    pub dir: PathBuf,
    pub file: File,
}

impl Config {
    /// `package`, resolved against the file's folder (without `..` when
    /// the folder exists).
    pub fn package(&self) -> Option<PathBuf> {
        let p = self.dir.join(self.file.package.as_ref()?);
        Some(p.canonicalize().unwrap_or(p))
    }

    /// `[parameters]` as `--param` gives them, sorted by name.
    pub fn params(&self) -> Vec<(String, String)> {
        self.file
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), param_text(v).unwrap_or_default()))
            .collect()
    }
}

/// Keys that would hold a secret. They are refused wherever they are,
/// except as a package parameter's name.
fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["password", "token", "secret"]
        .iter()
        .any(|s| k.contains(s))
}

/// Reads and checks the file at `path`: every key known, no secret, each
/// value as its option would take it. Errors exit 2.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PipesError::Usage(format!("--config {}: {e}", path.display())))?;
    let abs = path
        .canonicalize()
        .map_err(|e| PipesError::Usage(format!("--config {}: {e}", path.display())))?;
    let shown = path.display();
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| PipesError::Usage(format!("{shown}: {}", e.to_string().trim_end())))?;
    if let Some(key) = secret_key(&table, "") {
        return Err(PipesError::Usage(format!(
            "{shown}: {key}: passwords and tokens are not read from the settings file. Set \
             $FHIR_PIPES_DHIS2_PASSWORD (with `user` in the file or $FHIR_PIPES_DHIS2_USER) or \
             $FHIR_PIPES_DHIS2_TOKEN instead"
        )));
    }
    let file: File = toml::from_str(&text)
        .map_err(|e| PipesError::Usage(format!("{shown}: {}", e.to_string().trim_end())))?;
    validate(&file).map_err(|e| PipesError::Usage(format!("{shown}: {e}")))?;
    let dir = abs.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(Config {
        path: abs,
        dir,
        file,
    })
}

/// The first key, outside `[parameters]`, that names a secret.
fn secret_key(table: &toml::Table, prefix: &str) -> Option<String> {
    for (k, v) in table {
        let name = if prefix.is_empty() {
            k.clone()
        } else {
            format!("[{prefix}] {k}")
        };
        if is_secret(k) {
            return Some(name);
        }
        if let toml::Value::Table(t) = v
            && !(prefix.is_empty() && k == "parameters")
            && let Some(found) = secret_key(t, k)
        {
            return Some(found);
        }
    }
    None
}

/// A package parameter's value as `--param` would give it.
fn param_text(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Each value checked with the parser or check of its option, so the file
/// accepts what the command line accepts.
fn validate(f: &File) -> std::result::Result<(), String> {
    let uids = |key: &str, list: &[String]| -> std::result::Result<(), String> {
        for u in list {
            parse_uid(u).map_err(|e| format!("{key}: {e}"))?;
        }
        Ok(())
    };
    if let Some(url) = &f.url {
        crate::api::api_base(url).map_err(|e| format!("url: {e}"))?;
    }
    if let Some(name) = &f.instance {
        crate::Instance::new(name).map_err(|e| format!("instance: {e}"))?;
    }
    for (name, v) in &f.parameters {
        let text = param_text(v)
            .ok_or_else(|| format!("[parameters] {name}: give a string, a number or true/false"))?;
        fhir_pipes::import::package::parse_param(&format!("{name}={text}"))
            .map_err(|e| format!("[parameters] {name}: {e}"))?;
    }
    let l = &f.locations;
    uids("[locations] root", l.root.as_slice())?;
    uids("[locations] group_sets", &l.group_sets)?;
    uids("[locations] org_unit_groups", &l.org_unit_groups)?;
    if l.all_org_unit_groups && !l.org_unit_groups.is_empty() {
        return Err(
            "[locations]: all_org_unit_groups and org_unit_groups cannot be used together".into(),
        );
    }
    let i = &f.indicators;
    uids("[indicators] indicator_groups", &i.indicator_groups)?;
    uids("[indicators] indicators", &i.indicators)?;
    uids("[indicators] data_element_groups", &i.data_element_groups)?;
    uids("[indicators] data_elements", &i.data_elements)?;
    uids(
        "[indicators] data_element_group_sets",
        &i.data_element_group_sets,
    )?;
    uids("[indicators] data_sets", &i.data_sets)?;
    let r = &f.reports;
    for range in &r.ranges {
        crate::reports::check_range(range).map_err(|e| format!("[reports] ranges: {e}"))?;
    }
    if r.levels.contains(&0) {
        return Err("[reports] levels: --level counts from 1, the top of the DHIS2 tree".into());
    }
    uids("[reports] root", r.root.as_slice())?;
    for b in &r.breakdowns {
        crate::reports::check_breakdown(b).map_err(|e| format!("[reports] breakdowns: {e}"))?;
    }
    if let Some(c) = &r.campaign {
        crate::reports::check_campaign(c).map_err(|e| format!("[reports] campaign: {e}"))?;
    }
    if let Some(s) = &r.source {
        source(s).map_err(|e| format!("[reports] source: {e}"))?;
    }
    for (key, n) in [
        ("chunk", r.chunk),
        ("max_rows", r.max_rows),
        ("concurrency", r.concurrency),
    ] {
        if n == Some(0) {
            return Err(format!("[reports] {key}: must be at least 1"));
        }
    }
    if let Some(age) = &r.max_analytics_age {
        crate::api::parse_age(age).map_err(|e| format!("[reports] max_analytics_age: {e}"))?;
    }
    Ok(())
}

/// `--source`'s parser.
fn source(s: &str) -> std::result::Result<ReportSource, String> {
    ReportSource::from_str(s, false)
        .map_err(|_| format!("{s:?}: expected one of analytics, data-values"))
}

/// True when `id` was given on the command line or through its
/// environment variable, rather than left at its default.
fn given(m: &ArgMatches, id: &str) -> bool {
    matches!(
        m.value_source(id),
        Some(ValueSource::CommandLine | ValueSource::EnvVariable)
    )
}

/// The matches of the command being run: `extract reports`, not `extract`.
pub fn leaf(m: &ArgMatches) -> &ArgMatches {
    let mut m = m;
    while let Some((_, sub)) = m.subcommand() {
        m = sub;
    }
    m
}

/// How to give a setting that is missing: its option, or its key in the
/// file.
fn missing(what: &str, option: &str, key: &str, cfg: Option<&Config>) -> String {
    let file = match cfg {
        Some(c) => format!("`{key}` in {}", c.path.display()),
        None => format!("`{key}` in a settings file given with --config FILE"),
    };
    format!("{what}: give {option}, or {file}")
}

/// Fills each option the command line left out from the settings file, and
/// checks that the settings every run needs are given one way or the
/// other. Without a file, only the check.
pub fn settle(cmd: &mut Dhis2Command, m: &ArgMatches, cfg: Option<&Config>) -> Result<()> {
    let m = leaf(m);
    let usage = |r: std::result::Result<(), String>| r.map_err(PipesError::Usage);
    match cmd {
        Dhis2Command::Locations(a) => {
            conn(&mut a.conn, m, cfg)?;
            locations(&mut a.opts, cfg);
            a.root = a.root.take().or_else(|| file_of(cfg).locations.root);
            package(&mut a.package, cfg);
        }
        Dhis2Command::Measures(a) => {
            conn(&mut a.conn, m, cfg)?;
            select(&mut a.select, cfg);
            usage(need_select(&a.select, cfg))?;
            package(&mut a.package, cfg);
        }
        Dhis2Command::Reports(a) => {
            conn(&mut a.conn, m, cfg)?;
            select(&mut a.select, cfg);
            reports(&mut a.opts, m, cfg)?;
            usage(need_select(&a.select, cfg))?;
            usage(need_reports(&a.opts, cfg))?;
            package(&mut a.package, cfg);
        }
        Dhis2Command::Extract(Dhis2ExtractCommand::Locations(a)) => {
            conn(&mut a.conn, m, cfg)?;
            locations(&mut a.opts, cfg);
            a.root = a.root.take().or_else(|| file_of(cfg).locations.root);
        }
        Dhis2Command::Extract(Dhis2ExtractCommand::Measures(a)) => {
            conn(&mut a.conn, m, cfg)?;
            select(&mut a.select, cfg);
            usage(need_select(&a.select, cfg))?;
        }
        Dhis2Command::Extract(Dhis2ExtractCommand::Reports(a)) => {
            conn(&mut a.conn, m, cfg)?;
            select(&mut a.select, cfg);
            reports(&mut a.opts, m, cfg)?;
            usage(need_select(&a.select, cfg))?;
            usage(need_reports(&a.opts, cfg))?;
        }
        Dhis2Command::Extract(Dhis2ExtractCommand::All(a)) => all(a, m, cfg)?,
        Dhis2Command::Config(_) => {}
        Dhis2Command::List(a) => conn_for(&mut a.conn, m, cfg, false)?,
    }
    Ok(())
}

/// `extract all`: every step's settings, each checked unless the step is
/// left out with `--skip`.
fn all(a: &mut ExtractAllArgs, m: &ArgMatches, cfg: Option<&Config>) -> Result<()> {
    conn(&mut a.conn, m, cfg)?;
    locations(&mut a.locations, cfg);
    // `--root` is the root of both steps; in the file each has its own.
    let root = a.reports.root.clone();
    a.locations_root = root.or_else(|| file_of(cfg).locations.root);
    select(&mut a.select, cfg);
    reports(&mut a.reports, m, cfg)?;
    let runs = |s: Step| !a.skip.contains(&s);
    if ![Step::Locations, Step::Measures, Step::Reports]
        .into_iter()
        .any(runs)
    {
        return Err(PipesError::Usage(
            "extract all: every step is left out with --skip; nothing to do".into(),
        ));
    }
    let step = |s: Step, r: std::result::Result<(), String>| {
        r.map_err(|e| {
            PipesError::Usage(format!(
                "extract all, {name} step: {e}; or leave the step out with --skip {name}",
                name = s.name()
            ))
        })
    };
    for s in [Step::Measures, Step::Reports] {
        if runs(s) {
            step(s, need_select(&a.select, cfg))?;
        }
    }
    if runs(Step::Reports) {
        step(Step::Reports, need_reports(&a.reports, cfg))?;
    }
    Ok(())
}

fn file_of(cfg: Option<&Config>) -> File {
    cfg.map(|c| c.file.clone()).unwrap_or_default()
}

/// The server, the instance, the user name and the HTTP settings.
fn conn(c: &mut Dhis2Conn, m: &ArgMatches, cfg: Option<&Config>) -> Result<()> {
    conn_for(c, m, cfg, true)
}

/// `conn`; `dhis2 list` writes no ids, and needs no instance name.
fn conn_for(c: &mut Dhis2Conn, m: &ArgMatches, cfg: Option<&Config>, instance: bool) -> Result<()> {
    if let Some(cfg) = cfg {
        let f = &cfg.file;
        c.url = c.url.take().or_else(|| f.url.clone());
        c.instance = c.instance.take().or_else(|| f.instance.clone());
        // A token or user name given on the command line or in the
        // environment wins over the file's user name.
        if c.token.is_none() && c.user.is_none() {
            c.user = f.user.clone();
        }
        if !given(m, "retries")
            && let Some(r) = f.retries
        {
            c.retries = r;
        }
        if !given(m, "timeout")
            && let Some(t) = f.timeout
        {
            c.timeout = t;
        }
        c.config = Some(cfg.path.clone());
    }
    if c.url.is_none() {
        return Err(PipesError::Usage(missing(
            "no DHIS2 URL",
            "--dhis2 URL (or $FHIR_PIPES_DHIS2_URL)",
            "url",
            cfg,
        )));
    }
    if instance && c.instance.is_none() {
        return Err(PipesError::Usage(missing(
            "no instance name",
            "--instance NAME",
            "instance",
            cfg,
        )));
    }
    if c.token.is_none() {
        match (&c.user, &c.password) {
            (Some(_), None) => {
                return Err(PipesError::Usage(
                    "a DHIS2 user name is given but no password: set $FHIR_PIPES_DHIS2_PASSWORD \
                     (or give --dhis2-password)"
                        .into(),
                ));
            }
            (None, Some(_)) => {
                return Err(PipesError::Usage(missing(
                    "a DHIS2 password is given but no user name",
                    "--dhis2-user (or $FHIR_PIPES_DHIS2_USER)",
                    "user",
                    cfg,
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `[locations]`. The org unit groups are one setting: `--org-unit-group`
/// or `--all-org-unit-groups` on the command line replaces both keys.
fn locations(o: &mut LocationsOptions, cfg: Option<&Config>) {
    let Some(cfg) = cfg else { return };
    let f = &cfg.file.locations;
    o.id_prefix = o.id_prefix.take().or_else(|| f.id_prefix.clone());
    o.country_code = o.country_code.take().or_else(|| f.country_code.clone());
    o.facility_level = o.facility_level.or(f.facility_level);
    if o.group_sets.is_empty() {
        o.group_sets = f.group_sets.clone();
    }
    if o.org_unit_groups.is_empty() && !o.all_org_unit_groups {
        o.org_unit_groups = f.org_unit_groups.clone();
        o.all_org_unit_groups = f.all_org_unit_groups;
    }
}

/// `[indicators]`: any selection option on the command line replaces the
/// whole selection of the file.
fn select(s: &mut IndicatorSelection, cfg: Option<&Config>) {
    let Some(cfg) = cfg else { return };
    if is_empty(s) {
        *s = cfg.file.indicators.selection();
    }
}

fn is_empty(s: &IndicatorSelection) -> bool {
    s.groups.is_empty()
        && s.indicators.is_empty()
        && s.data_element_groups.is_empty()
        && s.data_elements.is_empty()
        && s.data_element_group_sets.is_empty()
        && s.data_sets.is_empty()
}

fn need_select(s: &IndicatorSelection, cfg: Option<&Config>) -> std::result::Result<(), String> {
    if is_empty(s) {
        return Err(missing(
            "no indicators selected",
            "--indicator-group, --indicator, --data-element-group, --data-element, \
             --data-element-group-set or --data-set",
            "[indicators]",
            cfg,
        ));
    }
    Ok(())
}

/// `[reports]`. `--period` and `--range` are one setting: either on the
/// command line replaces both `periods` and `ranges` of the file.
fn reports(o: &mut ReportsOptions, m: &ArgMatches, cfg: Option<&Config>) -> Result<()> {
    let Some(cfg) = cfg else { return Ok(()) };
    let f = &cfg.file.reports;
    if o.periods.is_empty() && o.ranges.is_empty() {
        o.periods = f.periods.clone();
        o.ranges = f.ranges.clone();
    }
    if o.levels.is_empty() {
        o.levels = f.levels.clone();
    }
    o.root = o.root.take().or_else(|| f.root.clone());
    if o.breakdowns.is_empty() {
        o.breakdowns = f.breakdowns.clone();
    }
    o.campaign = o.campaign.take().or_else(|| f.campaign.clone());
    if !given(m, "source")
        && let Some(s) = &f.source
    {
        o.source = source(s).map_err(PipesError::Usage)?;
    }
    for (id, slot, value) in [
        ("chunk", &mut o.chunk, f.chunk),
        ("max_rows", &mut o.max_rows, f.max_rows),
        ("concurrency", &mut o.concurrency, f.concurrency),
    ] {
        if !given(m, id)
            && let Some(v) = value
        {
            *slot = v;
        }
    }
    if o.max_analytics_age.is_none()
        && let Some(age) = &f.max_analytics_age
    {
        o.max_analytics_age = Some(crate::api::parse_age(age).map_err(PipesError::Usage)?);
    }
    Ok(())
}

fn need_reports(o: &ReportsOptions, cfg: Option<&Config>) -> std::result::Result<(), String> {
    if o.levels.is_empty() {
        return Err(missing(
            "no org unit levels to report on",
            "--level N",
            "[reports] levels",
            cfg,
        ));
    }
    if o.periods.is_empty() && o.ranges.is_empty() {
        return Err(missing(
            "no periods",
            "--period or --range",
            "[reports] periods or ranges",
            cfg,
        ));
    }
    Ok(())
}

/// `package` and `[parameters]`. `--package` (or `$FHIR_PIPES_DHIS2_MAPS`)
/// replaces the file's package; a `--param` replaces the file's value of
/// that one parameter.
fn package(p: &mut Dhis2Package, cfg: Option<&Config>) {
    let Some(cfg) = cfg else { return };
    p.dir = p.dir.take().or_else(|| cfg.package());
    let mut params = cfg.params();
    params.retain(|(name, _)| !p.params.iter().any(|(n, _)| n == name));
    params.append(&mut p.params);
    p.params = params;
}

/// `dhis2 config check`: the file read and checked, and the settings a run
/// with no options would use, as TOML. Secrets are never printed: only
/// where the credentials come from.
pub fn check(cfg: Option<&Config>, args: &ConfigCheckArgs) -> Result<()> {
    let Some(cfg) = cfg else {
        return Err(PipesError::Usage(
            "dhis2 config check needs a settings file: give --config FILE or set \
             $FHIR_PIPES_DHIS2_CONFIG"
                .into(),
        ));
    };
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let mut f = cfg.file.clone();
    let mut notes = Vec::new();
    if let Some(url) = env("FHIR_PIPES_DHIS2_URL") {
        f.url = Some(url);
        notes.push("url is from $FHIR_PIPES_DHIS2_URL".to_string());
    }
    let token = env("FHIR_PIPES_DHIS2_TOKEN");
    if let Some(user) = env("FHIR_PIPES_DHIS2_USER") {
        f.user = Some(user);
        notes.push("user is from $FHIR_PIPES_DHIS2_USER".to_string());
    }
    if let Some(dir) = env("FHIR_PIPES_DHIS2_MAPS") {
        f.package = Some(dir.into());
        notes.push("package is from $FHIR_PIPES_DHIS2_MAPS".to_string());
    } else {
        f.package = cfg.package();
    }
    let password = env("FHIR_PIPES_DHIS2_PASSWORD").is_some();
    let auth = match (&token, &f.user, password) {
        (Some(_), _, _) => "a personal access token from $FHIR_PIPES_DHIS2_TOKEN".to_string(),
        (None, Some(u), true) => {
            format!("user {u} with the password from $FHIR_PIPES_DHIS2_PASSWORD")
        }
        (None, Some(u), false) => {
            format!("user {u}, but $FHIR_PIPES_DHIS2_PASSWORD is not set")
        }
        (None, None, _) => "none: set $FHIR_PIPES_DHIS2_TOKEN, or a user and \
                            $FHIR_PIPES_DHIS2_PASSWORD"
            .to_string(),
    };
    let text = toml::to_string(&f)
        .map_err(|e| PipesError::Usage(format!("{}: {e}", cfg.path.display())))?;
    println!("# {}", cfg.path.display());
    for n in &notes {
        println!("# {n}");
    }
    println!("# authentication: {auth}");
    let probe = Config {
        file: f.clone(),
        ..cfg.clone()
    };
    let p = Some(&probe);
    let mut sel = IndicatorSelection::default();
    select(&mut sel, p);
    let o = ReportsOptions {
        periods: f.reports.periods.clone(),
        ranges: f.reports.ranges.clone(),
        levels: f.reports.levels.clone(),
        ..default_reports()
    };
    let reports_need = need_select(&sel, p).and(need_reports(&o, p));
    let mut steps = Vec::new();
    for (s, need) in [
        (Step::Locations, Ok(())),
        (Step::Measures, need_select(&sel, p)),
        (Step::Reports, reports_need),
    ] {
        steps.push(match need {
            Ok(()) => format!("# extract all, {}: ready", s.name()),
            Err(e) => format!("# extract all, {}: {e}", s.name()),
        });
    }
    let missing_conn = [(f.url.is_none(), "url"), (f.instance.is_none(), "instance")]
        .into_iter()
        .filter(|(m, _)| *m)
        .map(|(_, k)| k)
        .collect::<Vec<_>>();
    if !missing_conn.is_empty() {
        println!(
            "# not in the file, so needed on the command line: {}",
            missing_conn.join(", ")
        );
    }
    for s in steps {
        println!("{s}");
    }
    print!("{text}");
    if args.connect {
        let conn = Dhis2Conn {
            url: f.url.clone(),
            instance: f.instance.clone(),
            token,
            user: f.user.clone(),
            password: env("FHIR_PIPES_DHIS2_PASSWORD"),
            retries: f.retries.unwrap_or(fhir_pipes::DEFAULT_RETRIES),
            timeout: f.timeout.unwrap_or(fhir_pipes::DEFAULT_TIMEOUT_SECS),
            config: None,
        };
        if conn.url.is_none() {
            return Err(PipesError::Usage(
                "--connect needs `url` in the file or $FHIR_PIPES_DHIS2_URL".into(),
            ));
        }
        let api = crate::api::Dhis2::connect(&conn)?;
        println!(
            "# signed in to {}: DHIS2 {}",
            fhir_pipes::redact::url(conn.url()),
            api.info.version.as_deref().unwrap_or("(version not given)")
        );
    }
    Ok(())
}

/// `ReportsOptions` at the defaults of its options.
fn default_reports() -> ReportsOptions {
    ReportsOptions {
        periods: Vec::new(),
        ranges: Vec::new(),
        campaign: None,
        levels: Vec::new(),
        root: None,
        breakdowns: Vec::new(),
        chunk: crate::cli::DEFAULT_INDICATORS_PER_REQUEST,
        max_rows: crate::cli::DEFAULT_ANALYTICS_MAX_ROWS,
        source: ReportSource::Analytics,
        max_analytics_age: None,
        concurrency: crate::cli::DEFAULT_ANALYTICS_CONCURRENCY,
        resume: false,
        report: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    use crate::cli::Cli;

    const FILE: &str = r#"
url = "https://dhis2.example.org"
instance = "sl-demo"
user = "reader"
retries = 7
package = "pkg"

[parameters]
base = "https://example.org"
lenient = true

[locations]
id_prefix = "sle"
facility_level = 4
org_unit_groups = ["hSgatV6qHFI"]

[indicators]
indicator_groups = ["srxUNt2w5jn"]
data_sets = ["QX4ZTUbOt3a"]

[reports]
levels = [2, 3]
periods = ["2025"]
ranges = ["2025-01-01:2025-03-31"]
source = "data-values"
chunk = 3
max_analytics_age = "7d"
"#;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("dhis2.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    /// The command `args` after `fhir-pipes-dhis2`, settled with the file
    /// `text`. The DHIS2 environment variables are not read: every option
    /// with one is given here or left to the file.
    fn settled(text: &str, args: &[&str]) -> Result<Dhis2Command> {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(&write(dir.path(), text))?;
        let mut argv = vec!["fhir-pipes-dhis2"];
        argv.extend_from_slice(args);
        let m = Cli::command().try_get_matches_from(argv).unwrap();
        let mut cmd = Cli::from_arg_matches(&m).unwrap().command;
        settle(&mut cmd, &m, Some(&cfg))?;
        Ok(cmd)
    }

    fn reports(cmd: Dhis2Command) -> crate::cli::ExtractReportsArgs {
        match cmd {
            Dhis2Command::Extract(Dhis2ExtractCommand::Reports(a)) => *a,
            other => panic!("{other:?}"),
        }
    }

    const PASSWORD: [&str; 2] = ["--dhis2-password", "x"];

    #[test]
    fn the_file_fills_what_the_command_line_leaves_out() {
        let mut args = vec!["extract", "reports", "--out", "o"];
        args.extend(PASSWORD);
        let a = reports(settled(FILE, &args).unwrap());
        assert_eq!(a.conn.url(), "https://dhis2.example.org");
        assert_eq!(a.conn.instance(), "sl-demo");
        assert_eq!(a.conn.user.as_deref(), Some("reader"));
        assert_eq!(a.conn.retries, 7);
        assert!(a.conn.config.as_ref().unwrap().is_absolute());
        assert_eq!(a.select.groups, ["srxUNt2w5jn"]);
        assert_eq!(a.select.data_sets, ["QX4ZTUbOt3a"]);
        assert_eq!(a.opts.levels, [2, 3]);
        assert_eq!(a.opts.periods, ["2025"]);
        assert_eq!(a.opts.ranges, ["2025-01-01:2025-03-31"]);
        assert_eq!(a.opts.source, ReportSource::DataValues);
        assert_eq!(a.opts.chunk, 3);
        assert_eq!(a.opts.max_rows, crate::cli::DEFAULT_ANALYTICS_MAX_ROWS);
        assert_eq!(
            a.opts.max_analytics_age,
            Some(std::time::Duration::from_secs(7 * 86_400))
        );
    }

    #[test]
    fn a_single_value_on_the_command_line_wins() {
        let mut args = vec![
            "extract",
            "reports",
            "--out",
            "o",
            "--dhis2",
            "https://other.org",
            "--instance",
            "other",
            "--chunk",
            "10",
            "--retries",
            "2",
            "--source",
            "analytics",
        ];
        args.extend(PASSWORD);
        let a = reports(settled(FILE, &args).unwrap());
        assert_eq!(a.conn.url(), "https://other.org");
        assert_eq!(a.conn.instance(), "other");
        assert_eq!(a.conn.retries, 2);
        // Given on the command line at its default value: still the
        // command line's.
        assert_eq!(a.opts.chunk, 10);
        assert_eq!(a.opts.source, ReportSource::Analytics);
    }

    #[test]
    fn a_repeatable_option_replaces_the_list() {
        let mut args = vec![
            "extract",
            "reports",
            "--out",
            "o",
            "--level",
            "4",
            "--period",
            "202501",
            "--indicator",
            "tUIlpyeeX9N",
        ];
        args.extend(PASSWORD);
        let a = reports(settled(FILE, &args).unwrap());
        assert_eq!(a.opts.levels, [4]);
        // --period replaces both periods and ranges.
        assert_eq!(a.opts.periods, ["202501"]);
        assert!(a.opts.ranges.is_empty());
        // Any selection option replaces the whole [indicators].
        assert_eq!(a.select.indicators, ["tUIlpyeeX9N"]);
        assert!(a.select.groups.is_empty() && a.select.data_sets.is_empty());
    }

    #[test]
    fn a_flag_on_the_command_line_replaces_the_groups_of_the_file() {
        let locations = |extra: &[&str]| {
            let mut args = vec!["extract", "locations", "--out", "o"];
            args.extend(PASSWORD);
            args.extend_from_slice(extra);
            match settled(FILE, &args).unwrap() {
                Dhis2Command::Extract(Dhis2ExtractCommand::Locations(a)) => a,
                other => panic!("{other:?}"),
            }
        };
        let a = locations(&[]);
        assert_eq!(a.opts.org_unit_groups, ["hSgatV6qHFI"]);
        assert!(!a.opts.all_org_unit_groups);
        assert_eq!(a.opts.facility_level, Some(4));
        let a = locations(&["--all-org-unit-groups", "--facility-level", "3"]);
        assert!(a.opts.all_org_unit_groups);
        assert!(a.opts.org_unit_groups.is_empty());
        assert_eq!(a.opts.facility_level, Some(3));
        // The flag in the file.
        let text = FILE.replace(
            "org_unit_groups = [\"hSgatV6qHFI\"]",
            "all_org_unit_groups = true",
        );
        let mut args = vec!["extract", "locations", "--out", "o"];
        args.extend(PASSWORD);
        match settled(&text, &args).unwrap() {
            Dhis2Command::Extract(Dhis2ExtractCommand::Locations(a)) => {
                assert!(a.opts.all_org_unit_groups)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn package_is_relative_to_the_file_and_param_overrides_one_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(&write(dir.path(), FILE)).unwrap();
        let folder = dir.path().canonicalize().unwrap();
        assert_eq!(cfg.package().unwrap(), folder.join("pkg"));
        let mut args = vec![
            "fhir-pipes-dhis2",
            "measures",
            "--out",
            "o.ndjson",
            "--param",
            "base=https://cli.org",
        ];
        args.extend(PASSWORD);
        let m = Cli::command().try_get_matches_from(args).unwrap();
        let mut cmd = Cli::from_arg_matches(&m).unwrap().command;
        settle(&mut cmd, &m, Some(&cfg)).unwrap();
        let Dhis2Command::Measures(a) = cmd else {
            panic!()
        };
        assert_eq!(a.package.dir.unwrap(), folder.join("pkg"));
        assert_eq!(
            a.package.params,
            [
                ("lenient".to_string(), "true".to_string()),
                ("base".to_string(), "https://cli.org".to_string())
            ]
        );
    }

    #[test]
    fn unknown_keys_are_errors_naming_the_file_and_key() {
        for (text, key) in [
            ("instanse = \"x\"\n", "instanse"),
            ("[reports]\nlevel = [2]\n", "level"),
            ("[locations]\nroot_uid = \"x\"\n", "root_uid"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let p = write(dir.path(), text);
            let e = load(&p).unwrap_err();
            assert_eq!(e.exit_code(), 2);
            let e = e.to_string();
            assert!(e.contains("dhis2.toml") && e.contains(key), "{e}");
        }
    }

    #[test]
    fn secrets_are_refused() {
        for (text, key) in [
            ("password = \"district\"\n", "password"),
            ("dhis2_token = \"d2pat_x\"\n", "dhis2_token"),
            ("[reports]\ntoken = \"x\"\n", "[reports] token"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let e = load(&write(dir.path(), text)).unwrap_err();
            assert_eq!(e.exit_code(), 2);
            let e = e.to_string();
            assert!(
                e.contains(key)
                    && e.contains("$FHIR_PIPES_DHIS2_PASSWORD")
                    && e.contains("$FHIR_PIPES_DHIS2_TOKEN"),
                "{e}"
            );
        }
        // A package parameter may have such a name.
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&write(dir.path(), "[parameters]\ntokenSystem = \"x\"\n")).is_ok());
    }

    #[test]
    fn values_are_checked_as_their_options_are() {
        for (text, want) in [
            (
                "[indicators]\nindicators = [\"no\"]\n",
                "is not a DHIS2 UID",
            ),
            ("[reports]\nlevels = [0]\n", "counts from 1"),
            ("[reports]\nlevels = [-1]\n", "levels"),
            ("[reports]\nranges = [\"2025\"]\n", "expected START:END"),
            ("[reports]\nsource = \"api\"\n", "analytics, data-values"),
            (
                "[reports]\nmax_analytics_age = \"7\"\n",
                "hours, days or weeks",
            ),
            ("[reports]\ncampaign = \"x\"\n", "ResourceType/id"),
            ("[reports]\nbreakdowns = [\"a:b\"]\n", "UID of a category"),
            ("[reports]\nchunk = 0\n", "at least 1"),
            ("instance = \"sl demo\"\n", "letters, digits and hyphens"),
            ("url = \"ftp://x\"\n", "http(s) URL"),
            (
                "[locations]\nall_org_unit_groups = true\norg_unit_groups = [\"hSgatV6qHFI\"]\n",
                "cannot be used together",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let e = load(&write(dir.path(), text)).unwrap_err().to_string();
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn a_missing_setting_names_both_ways_to_give_it() {
        let e = settled(
            "",
            &["extract", "measures", "--out", "o", "--instance", "x"],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("--dhis2 URL") && e.contains("`url` in") && e.contains("dhis2.toml"),
            "{e}"
        );
        let e = settled(
            "url = \"https://x\"\ninstance = \"x\"\n",
            &[
                "extract",
                "reports",
                "--out",
                "o",
                "--indicator",
                "tUIlpyeeX9N",
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("--level N") && e.contains("`[reports] levels` in"),
            "{e}"
        );
        // Without a file.
        let m = Cli::command()
            .try_get_matches_from([
                "x",
                "extract",
                "measures",
                "--out",
                "o",
                "--dhis2",
                "https://x",
            ])
            .unwrap();
        let mut cmd = Cli::from_arg_matches(&m).unwrap().command;
        let e = settle(&mut cmd, &m, None).unwrap_err().to_string();
        assert!(
            e.contains("--instance NAME")
                && e.contains("`instance` in a settings file given with --config FILE"),
            "{e}"
        );
    }

    #[test]
    fn extract_all_names_the_step_and_accepts_skip() {
        let text =
            "url = \"https://x\"\ninstance = \"x\"\n[indicators]\nindicators = [\"tUIlpyeeX9N\"]\n";
        let e = settled(text, &["extract", "all", "--out", "o"])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("extract all, reports step") && e.contains("--skip reports"),
            "{e}"
        );
        let cmd = settled(text, &["extract", "all", "--out", "o", "--skip", "reports"]).unwrap();
        assert!(matches!(
            cmd,
            Dhis2Command::Extract(Dhis2ExtractCommand::All(_))
        ));
        // --root is the root of both steps; the file has one per step.
        let roots = "[locations]\nroot = \"ImspTQPwCqd\"\n[reports]\nroot = \"O6uvpzGd5pu\"\n";
        let all = |args: &[&str]| match settled(&format!("{text}{roots}"), args).unwrap() {
            Dhis2Command::Extract(Dhis2ExtractCommand::All(a)) => a,
            other => panic!("{other:?}"),
        };
        let a = all(&["extract", "all", "--out", "o", "--skip", "reports"]);
        assert_eq!(a.locations_root.as_deref(), Some("ImspTQPwCqd"));
        assert_eq!(a.reports.root.as_deref(), Some("O6uvpzGd5pu"));
        let a = all(&[
            "extract",
            "all",
            "--out",
            "o",
            "--skip",
            "reports",
            "--root",
            "fdc6uOvgoji",
        ]);
        assert_eq!(a.locations_root.as_deref(), Some("fdc6uOvgoji"));
        assert_eq!(a.reports.root.as_deref(), Some("fdc6uOvgoji"));
    }
}
