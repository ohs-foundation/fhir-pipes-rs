//! The command line of fhir-pipes-dhis2. These are the options
//! `fhir-pipes dhis2` had when it was a command of fhir-pipes.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use fhir_pipes::{DEFAULT_RETRIES, DEFAULT_TIMEOUT_SECS};

pub const DEFAULT_INDICATORS_PER_REQUEST: usize = 10;
/// Rows fhir-pipes aims to keep one analytics request under; DHIS2's own default
/// limit is 50,000.
pub const DEFAULT_ANALYTICS_MAX_ROWS: usize = 20_000;
/// Analytics requests in flight at once. Each one is a database query on
/// the DHIS2 server, so this stays low.
pub const DEFAULT_ANALYTICS_CONCURRENCY: usize = 4;

#[derive(Parser, Debug)]
#[command(
    name = "fhir-pipes-dhis2",
    version,
    about = "Read a DHIS2 instance: `extract` writes org units, indicators and indicator values as records for `fhir-pipes import run`; `locations`, `measures` and `reports` also map them with an import package and write FHIR NDJSON for `fhir-pipes load`. Also runs as `fhir-pipes dhis2`"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Dhis2Command,
    /// The settings file of a DHIS2 instance (TOML): the server, the instance name and what to export, so a command needs only --out. An option given on the command line, or its environment variable, overrides the file's value. Falls back to $FHIR_PIPES_DHIS2_CONFIG. See docs/dhis2.md
    #[arg(
        long,
        global = true,
        env = "FHIR_PIPES_DHIS2_CONFIG",
        value_name = "FILE"
    )]
    pub config: Option<PathBuf>,
    /// Not read: no dhis2 command uses a profile. Accepted so that
    /// `fhir-pipes dhis2 … --profile FILE` still runs.
    #[arg(long, global = true, hide = true, value_name = "FILE|URL")]
    pub profile: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Dhis2Command {
    /// Org units as Locations (facilities with their Organizations): `extract locations`, then the package's map, in one step
    Locations(Dhis2LocationsArgs),
    /// Indicators as Measures: `extract measures`, then the package's map, in one step
    Measures(Dhis2MeasuresArgs),
    /// Indicator values from DHIS2 analytics as MeasureReports, one per indicator, org unit and period: `extract reports`, then the package's map, in one step
    Reports(Box<Dhis2ReportsArgs>),
    /// Write the records, without mapping them: one NDJSON file per record type in a folder, for `fhir-pipes import run`
    #[command(subcommand)]
    Extract(Dhis2ExtractCommand),
    /// Check a settings file (--config FILE) without running anything
    #[command(subcommand)]
    Config(ConfigCommand),
    /// List the UIDs (or levels) an option or settings key takes, such as the indicator groups, from the DHIS2 metadata API. Reads nothing else and writes nothing to DHIS2
    List(Box<ListArgs>),
}

/// What `dhis2 list` lists.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// For --indicator-group
    IndicatorGroups,
    /// For --indicator; --group UID lists one indicator group's
    Indicators,
    /// For --data-element-group
    DataElementGroups,
    /// For --data-element-group-set and --breakdown
    DataElementGroupSets,
    /// Aggregate data elements, for --data-element; --group UID lists one data element group's
    DataElements,
    /// For --data-set
    DataSets,
    /// Level numbers, for --level and --facility-level
    OrgUnitLevels,
    /// For --group-set and --breakdown
    OrgUnitGroupSets,
    /// For --org-unit-group; --group UID lists one org unit group set's
    OrgUnitGroups,
    /// Org units, for --root; --level N lists one level's
    OrgUnits,
    /// Categories that are data dimensions, for --breakdown
    Categories,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListFormat {
    /// Columns of UID, name and a count
    #[default]
    Table,
    /// A JSON array of objects
    Json,
}

/// `dhis2 list KIND`.
#[derive(clap::Args, Debug, Clone)]
pub struct ListArgs {
    /// What to list
    #[arg(value_enum)]
    pub kind: ListKind,
    #[command(flatten)]
    pub conn: Dhis2Conn,
    /// Only those whose name contains TEXT, ignoring case
    #[arg(long, value_name = "TEXT")]
    pub filter: Option<String>,
    /// With indicators, data-elements or org-unit-groups: only the members of this group (or org unit group set)
    #[arg(long, value_name = "UID", value_parser = parse_uid)]
    pub group: Option<String>,
    /// With org-units: only this level
    #[arg(long, value_name = "N")]
    pub level: Option<u32>,
    /// table or json
    #[arg(long, value_enum, default_value_t = ListFormat::Table)]
    pub format: ListFormat,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Parse and check the settings file given with --config, and print the settings a run would use. Exits 0 when the file is valid, 2 when it is not
    Check(ConfigCheckArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct ConfigCheckArgs {
    /// Also sign in to the DHIS2 server, to check the URL and the credentials. The UIDs in the file are not checked
    #[arg(long)]
    pub connect: bool,
}

#[derive(Subcommand, Debug)]
pub enum Dhis2ExtractCommand {
    /// Org units as DHIS2OrgUnit records, to DIR/DHIS2OrgUnit.ndjson
    Locations(ExtractLocationsArgs),
    /// Indicators, data elements and reporting rates as DHIS2Indicator records, to DIR/DHIS2Indicator.ndjson
    Measures(ExtractMeasuresArgs),
    /// Indicator values as DHIS2Value records, to DIR/DHIS2Value.ndjson
    Reports(Box<ExtractReportsArgs>),
    /// `extract locations`, then `extract measures`, then `extract reports`, into one folder. Stops at the first step that fails
    All(Box<ExtractAllArgs>),
}

/// A step of `dhis2 extract all`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Locations,
    Measures,
    Reports,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Locations => "locations",
            Step::Measures => "measures",
            Step::Reports => "reports",
        }
    }
}

/// Where `fhir-pipes dhis2 reports` takes its values from.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReportSource {
    /// DHIS2's analytics API: DHIS2 computes every value.
    #[default]
    Analytics,
    /// Raw data values; fhir-pipes computes the values.
    DataValues,
}

impl ReportSource {
    pub fn code(self) -> &'static str {
        match self {
            ReportSource::Analytics => "analytics",
            ReportSource::DataValues => "data-values",
        }
    }

    /// The code and display of the coding written in the profile's
    /// `source_extension`, under its `source_system`.
    pub fn coding(self) -> (&'static str, &'static str) {
        match self {
            ReportSource::Analytics => ("dhis2-analytics", "DHIS2 analytics"),
            ReportSource::DataValues => ("dhis2-data-values", "DHIS2 data values"),
        }
    }
}

/// Where the DHIS2 instance is, how to sign in, and the short name fhir-pipes
/// files it under.
#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2Conn {
    /// DHIS2 base URL, e.g. https://play.im.dhis2.org/stable-2-42-7-1 (a trailing /api is accepted); falls back to $FHIR_PIPES_DHIS2_URL, then to `url` in the settings file
    #[arg(long = "dhis2", env = "FHIR_PIPES_DHIS2_URL")]
    pub url: Option<String>,
    /// Short name for this instance (letters, digits, hyphens; up to 16), used in identifier systems, ids and the meta tag, e.g. sl-demo. Falls back to `instance` in the settings file
    #[arg(long)]
    pub instance: Option<String>,
    /// DHIS2 personal access token (d2pat_…); falls back to $FHIR_PIPES_DHIS2_TOKEN
    #[arg(
        long = "dhis2-token",
        env = "FHIR_PIPES_DHIS2_TOKEN",
        hide_env_values = true,
        conflicts_with = "user"
    )]
    pub token: Option<String>,
    /// DHIS2 username for Basic auth; falls back to $FHIR_PIPES_DHIS2_USER, then to `user` in the settings file. Needs a password
    #[arg(long = "dhis2-user", env = "FHIR_PIPES_DHIS2_USER")]
    pub user: Option<String>,
    /// DHIS2 password; falls back to $FHIR_PIPES_DHIS2_PASSWORD. Never read from the settings file
    #[arg(
        long = "dhis2-password",
        env = "FHIR_PIPES_DHIS2_PASSWORD",
        hide_env_values = true
    )]
    pub password: Option<String>,
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// The settings file the run read, for its report: set from --config.
    #[arg(skip)]
    pub config: Option<PathBuf>,
}

impl Dhis2Conn {
    /// The DHIS2 URL. A run checks that there is one (config::settle)
    /// before it connects.
    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or_default()
    }

    /// The instance name; checked as `url` is.
    pub fn instance(&self) -> &str {
        self.instance.as_deref().unwrap_or_default()
    }
}

/// `--package`: the import package `dhis2 locations|measures|reports` map
/// their records with.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Dhis2Package {
    /// The import package that turns the records into resources, such as examples/import/icr-dhis2 (its maps/ folder is also accepted). Each record goes through the package's map that reads its type: DHIS2OrgUnit (locations), DHIS2Indicator (measures), DHIS2Value (reports). Falls back to $FHIR_PIPES_DHIS2_MAPS
    #[arg(
        long = "package",
        alias = "maps",
        value_name = "DIR",
        env = "FHIR_PIPES_DHIS2_MAPS"
    )]
    pub dir: Option<PathBuf>,
    /// Set a parameter of the package, overriding its default: NAME=VALUE (repeatable)
    #[arg(long = "param", value_name = "NAME=VALUE", value_parser = fhir_pipes::import::package::parse_param)]
    pub params: Vec<(String, String)>,
}

/// A DHIS2 UID (11 letters and digits, the first a letter), for the flags
/// that name one: it goes into URL paths and `id:in:[…]` filters, where
/// `,`, `]` or `/` would change the request.
pub fn parse_uid(s: &str) -> std::result::Result<String, String> {
    let uid = s.trim();
    let shaped = uid.len() == 11
        && uid.starts_with(|c: char| c.is_ascii_alphabetic())
        && uid.bytes().all(|b| b.is_ascii_alphanumeric());
    if shaped {
        Ok(uid.to_string())
    } else {
        Err(format!("{s:?} is not a DHIS2 UID (11 letters and digits)"))
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2LocationsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub opts: LocationsOptions,
    /// Only this org unit and everything below it
    #[arg(long, value_parser = parse_uid)]
    pub root: Option<String>,
    #[command(flatten)]
    pub package: Dhis2Package,
    /// Output NDJSON file of Location and Organization resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Write DHIS2OrgUnit records instead of resources: set by `dhis2 extract locations`.
    #[arg(skip)]
    pub records: bool,
}

/// `dhis2 extract locations`.
#[derive(clap::Args, Debug, Clone)]
pub struct ExtractLocationsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub opts: LocationsOptions,
    /// Only this org unit and everything below it
    #[arg(long, value_parser = parse_uid)]
    pub root: Option<String>,
    /// Output folder; the records are written to DIR/DHIS2OrgUnit.ndjson. It is created if missing
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

impl ExtractLocationsArgs {
    /// The `dhis2 locations` run that writes these records.
    pub fn into_command(self, out: PathBuf) -> Dhis2LocationsArgs {
        Dhis2LocationsArgs {
            conn: self.conn,
            opts: self.opts,
            root: self.root,
            package: Dhis2Package::default(),
            out,
            report: self.report,
            records: true,
        }
    }
}

/// Which org units `dhis2 locations` and `dhis2 extract locations` read, and how.
#[derive(clap::Args, Debug, Clone)]
pub struct LocationsOptions {
    /// Prefix of the Location ids (PREFIX-<uid>); defaults to the instance name
    #[arg(long)]
    pub id_prefix: Option<String>,
    /// Country code put on the top org unit as its national admin code (e.g. SLE); fhir-pipes transform locations partitions by it
    #[arg(long)]
    pub country_code: Option<String>,
    /// DHIS2 level whose org units are facilities (a Location plus an Organization); defaults to the deepest level
    #[arg(long)]
    pub facility_level: Option<u32>,
    /// Org unit group set UID, repeatable (e.g. Facility Type): each org unit's group in the set is added to its Location's type (and its Organization's, for facilities), coded like a --breakdown on the same set
    #[arg(long = "group-set", value_parser = parse_uid)]
    pub group_sets: Vec<String>,
    /// Org unit group UID, repeatable (e.g. the group of all districts): each member's Location (and Organization) gets the group in its type. A group in a group set is coded as --group-set codes it; a group in no set under …/CodeSystem/dhis2/INSTANCE/org-unit-group
    #[arg(long = "org-unit-group", value_parser = parse_uid)]
    pub org_unit_groups: Vec<String>,
    /// Every org unit group, as --org-unit-group
    #[arg(long, conflicts_with = "org_unit_groups")]
    pub all_org_unit_groups: bool,
}

/// Which indicators and data elements: by group, one by one, or any mix.
/// At least one, from these options or the settings file's `[indicators]`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct IndicatorSelection {
    /// Indicator group UID, repeatable
    #[arg(long = "indicator-group", value_parser = parse_uid)]
    pub groups: Vec<String>,
    /// Indicator UID, repeatable
    #[arg(long = "indicator", value_parser = parse_uid)]
    pub indicators: Vec<String>,
    /// Data element group UID, repeatable: each aggregate data element's total becomes a count
    #[arg(long = "data-element-group", value_parser = parse_uid)]
    pub data_element_groups: Vec<String>,
    /// Data element UID, repeatable
    #[arg(long = "data-element", value_parser = parse_uid)]
    pub data_elements: Vec<String>,
    /// Data element group set UID, repeatable: every data element in every group of the set
    #[arg(long = "data-element-group-set", value_parser = parse_uid)]
    pub data_element_group_sets: Vec<String>,
    /// Data set UID, repeatable: its reporting rate (actual ÷ expected reports)
    #[arg(long = "data-set", value_parser = parse_uid)]
    pub data_sets: Vec<String>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2MeasuresArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    #[command(flatten)]
    pub package: Dhis2Package,
    /// Output NDJSON file of Measure resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Write DHIS2Indicator records instead of resources: set by `dhis2 extract measures`.
    #[arg(skip)]
    pub records: bool,
}

/// `dhis2 extract measures`.
#[derive(clap::Args, Debug, Clone)]
pub struct ExtractMeasuresArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    /// Output folder; the records are written to DIR/DHIS2Indicator.ndjson. It is created if missing
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

impl ExtractMeasuresArgs {
    /// The `dhis2 measures` run that writes these records.
    pub fn into_command(self, out: PathBuf) -> Dhis2MeasuresArgs {
        Dhis2MeasuresArgs {
            conn: self.conn,
            select: self.select,
            package: Dhis2Package::default(),
            out,
            report: self.report,
            records: true,
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2ReportsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    /// NDJSON holding the Locations that carry this instance's org unit UIDs, under the identifier system <base>/identifiers/dhis2/<instance>/orgunit (base: the package's parameter): the output of `fhir-pipes dhis2 locations`, a snapshot's locations.ndjson, or a Location.ndjson that `import run` wrote
    #[arg(long, required = true)]
    pub locations: Option<PathBuf>,
    #[command(flatten)]
    pub opts: ReportsOptions,
    #[command(flatten)]
    pub package: Dhis2Package,
    /// With --source data-values and --previous: recompute only the periods and ranges with data values updated or deleted since this date or time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS); --out keeps the previous reports for the rest
    #[arg(long, requires = "previous")]
    pub updated_since: Option<String>,
    /// Output NDJSON file of MeasureReport resources: every report this run returns, with or without --previous, so it is the baseline for the next run
    #[arg(long)]
    pub out: PathBuf,
    /// A previous run's --out (it may be the same file as --out): reports with the same content there count as unchanged. `date`, when DHIS2 last built its analytics tables, is not compared
    #[arg(long)]
    pub previous: Option<PathBuf>,
    /// With --previous: also write the new and changed reports here, the ones to load
    #[arg(long, requires = "previous")]
    pub changed: Option<PathBuf>,
    /// With --previous: write here, one per line, the ids of previous reports this run asked for again (same indicators, levels' org units and periods) that DHIS2 no longer returns, to delete from the FHIR server
    #[arg(long, requires = "previous")]
    pub removed: Option<PathBuf>,
    /// Write DHIS2Value records instead of resources: set by `dhis2 extract reports`.
    #[arg(skip)]
    pub records: bool,
    /// `dhis2 extract reports`: the `DHIS2OrgUnit` records read in place of
    /// `--locations`.
    #[arg(skip)]
    pub org_units: Option<PathBuf>,
}

/// `dhis2 extract reports`. The comparison with a previous run
/// (`--previous`, `--changed`, `--removed`, `--updated-since`) compares
/// MeasureReports, so it is not here: `import run --previous` does it after
/// the import.
#[derive(clap::Args, Debug, Clone)]
pub struct ExtractReportsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    #[command(flatten)]
    pub opts: ReportsOptions,
    /// Output folder; the records are written to DIR/DHIS2Value.ndjson. It is created if missing. An interrupted run leaves DHIS2Value.ndjson.tmp and DHIS2Value.ndjson.resume there for --resume; `import run` reads neither
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// The DHIS2OrgUnit records `dhis2 extract locations` wrote (NDJSON, or .ndjson.gz): each org unit's Location and, for --source data-values, its parent. Default: DIR/DHIS2OrgUnit.ndjson (or .ndjson.gz) when --out holds it
    #[arg(long, value_name = "FILE")]
    pub org_units: Option<PathBuf>,
}

impl ExtractReportsArgs {
    /// The `dhis2 reports` run that writes these records, reading the org
    /// units from `org_units`.
    pub fn into_command(self, out: PathBuf, org_units: PathBuf) -> Dhis2ReportsArgs {
        Dhis2ReportsArgs {
            conn: self.conn,
            select: self.select,
            locations: None,
            opts: self.opts,
            package: Dhis2Package::default(),
            updated_since: None,
            out,
            previous: None,
            changed: None,
            removed: None,
            records: true,
            org_units: Some(org_units),
        }
    }
}

/// What `dhis2 reports` and `dhis2 extract reports` ask DHIS2 for, and how.
#[derive(clap::Args, Debug, Clone)]
pub struct ReportsOptions {
    /// DHIS2 period, repeatable: ISO (2025, 2025Q1, 202501, 2025W5, …) or relative (LAST_12_MONTHS, THIS_YEAR, …). Needed unless --range is given, or the settings file has `periods` or `ranges`
    #[arg(long = "period")]
    pub periods: Vec<String>,
    /// Date range START:END, repeatable (ISO dates, both included, e.g. 2026-04-05:2026-05-20): one value per indicator and org unit for the whole range, asked for with DHIS2's startDate and endDate instead of a period. Report ids end in 20260405-20260520
    #[arg(long = "range")]
    pub ranges: Vec<String>,
    /// Campaign every report belongs to, as a reference (e.g. CarePlan/bgd-mr-sia-2026), added as the ICR campaign extension. Report ids do not include it, so one run should cover one campaign's periods
    #[arg(long)]
    pub campaign: Option<String>,
    /// DHIS2 org unit level to report on, repeatable (1 = the top). Needed unless the settings file has `levels`
    #[arg(long = "level")]
    pub levels: Vec<u32>,
    /// Only org units at or below this one: each level is asked for as LEVEL-n;ROOT. Needed when
    /// the user can see top-level org units outside the tree being reported on
    #[arg(long, value_parser = parse_uid)]
    pub root: Option<String>,
    /// Also break every value down by this DHIS2 dimension, repeatable: a category (e.g. Fixed/Outreach), an org unit group set (e.g. Facility Type) or a data element group set. Each becomes a stratifier on the report; DHIS2 leaves out items the dimension does not apply to
    #[arg(long = "breakdown")]
    pub breakdowns: Vec<String>,
    /// Indicators per analytics request (halved automatically when DHIS2 says a result is too large)
    #[arg(long, default_value_t = DEFAULT_INDICATORS_PER_REQUEST)]
    pub chunk: usize,
    /// Rows to aim for per analytics request: periods of the same kind (days, weeks, months, …) are asked for together while indicators × periods × org units at the level stays under this. Breakdown requests return a row per option on top; whatever DHIS2 still calls too large is split, periods first
    #[arg(long, default_value_t = DEFAULT_ANALYTICS_MAX_ROWS)]
    pub max_rows: usize,
    /// Where values come from: analytics (DHIS2 computes them) or data-values (raw data values from /api/dataValueSets, added up the org unit tree (the parents in --locations, or in the org unit records for dhis2 extract) and computed by fhir-pipes: SUM and AVERAGE_SUM_ORG_UNIT data elements, indicators of #{…}, C{…} and arithmetic, category breakdowns)
    #[arg(long, value_enum, default_value_t = ReportSource::Analytics)]
    pub source: ReportSource,
    /// Refuse to run when DHIS2's analytics tables were last built longer ago than this (e.g. 36h, 7d, 2w): values are only as fresh as that build. Without it, a build over 7 days old is warned about
    #[arg(long, value_parser = crate::api::parse_age)]
    pub max_analytics_age: Option<std::time::Duration>,
    /// Analytics requests in flight at once; reports are written as each batch arrives, in the same order whatever the setting
    #[arg(long, default_value_t = DEFAULT_ANALYTICS_CONCURRENCY)]
    pub concurrency: usize,
    /// Continue an interrupted run after its last fully written batch instead of starting over. Refused if anything that shapes the batches has changed since (arguments, org unit counts, the analytics build, the --locations, --org-units or --previous file)
    #[arg(long)]
    pub resume: bool,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `dhis2 extract all`: the options of the three extract commands, once.
#[derive(clap::Args, Debug, Clone)]
pub struct ExtractAllArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub locations: LocationsOptions,
    #[command(flatten)]
    pub select: IndicatorSelection,
    /// `--root` and the other options of `extract reports`. In `extract
    /// all`, `--root` is the root of both the locations and the reports
    /// step, and `--report` the report of all three.
    #[command(flatten)]
    pub reports: ReportsOptions,
    /// Output folder for all three record files. It is created if missing
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// The DHIS2OrgUnit records the reports step reads. Default: the DIR/DHIS2OrgUnit.ndjson the locations step writes
    #[arg(long, value_name = "FILE")]
    pub org_units: Option<PathBuf>,
    /// Leave a step out (repeatable): locations, measures or reports
    #[arg(long, value_enum, value_name = "STEP")]
    pub skip: Vec<Step>,
    /// The locations step's root: `--root`, else the settings file's
    /// `[locations] root` (set by config::settle).
    #[arg(skip)]
    pub locations_root: Option<String>,
}
