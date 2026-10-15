use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::extract::client::Auth;

pub const DEFAULT_ROW_GROUP_SIZE: usize = 20_000;
pub const DEFAULT_PARTITION_BY: &str = "country,geom_type,type";
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_RETRIES: usize = 3;
pub const DEFAULT_MAX_CONSECUTIVE_FAILURES: usize = 50;
/// Generous by design: a large boundary over a slow link can legitimately
/// take minutes, and reqwest's blocking client has no per-read timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_BATCH_SIZE: usize = 100;
/// A country raster is 100–200 MB; the FHIR-paging default would cut a slow
/// download off after five minutes. Total per-request deadline, in seconds.
pub const DEFAULT_RASTER_TIMEOUT_SECS: u64 = 3600;
pub const DEFAULT_INDICATORS_PER_REQUEST: usize = 10;
/// Rows fhir-pipes aims to keep one analytics request under; DHIS2's own default
/// limit is 50,000.
pub const DEFAULT_ANALYTICS_MAX_ROWS: usize = 20_000;
/// Analytics requests in flight at once. Each one is a database query on
/// the DHIS2 server, so this stays low.
pub const DEFAULT_ANALYTICS_CONCURRENCY: usize = 4;

#[derive(Parser, Debug)]
#[command(
    name = "fhir-pipes",
    version,
    about = "Move data into and out of FHIR servers: SQL-on-FHIR warehouses, GeoParquet, DHIS2"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Profile: an implementation guide's extra columns, codes and URLs, as a
    /// TOML file path or an https URL (see docs/profiles.md). Without one, a
    /// command working from a snapshot uses the profile it was last
    /// transformed with, and otherwise reads base FHIR only.
    #[arg(
        long,
        global = true,
        env = "FHIR_PIPES_PROFILE",
        value_name = "FILE|URL"
    )]
    pub profile: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Refresh a SQL-on-FHIR Parquet warehouse: locations, views, tiles,
    /// manifest and hooks, and push with --push
    Refresh(crate::warehouse::RefreshArgs),
    /// Run, check and test SQL-on-FHIR ViewDefinitions directly: run, sql,
    /// validate, lint, test
    Views(crate::views::ViewsArgs),
    /// Turn a snapshot of FHIR resources into a dataset for another tool (offline)
    #[command(subcommand)]
    Transform(TransformCommand),
    /// Summarise a written dataset
    Inspect(InspectArgs),
    /// Fetch Locations from a FHIR server into a snapshot (incremental)
    Extract(ExtractArgs),
    /// Download every resource of one or more types from a FHIR server as NDJSON, one file per type
    Export(ExportArgs),
    /// Extract then transform
    Run(RunArgs),
    /// Compare an edited GeoJSON or GeoParquet file to the snapshot; write changed Locations as FHIR NDJSON (offline)
    Diff(DiffArgs),
    /// Send changed resources to a FHIR server as version-checked transaction bundles
    Load(LoadArgs),
    /// Backfill spatial-index cells (quadkey, geohash) onto snapshot Locations that lack them; writes the changed resources as FHIR NDJSON for `fhir-pipes load` (offline)
    Index(IndexArgs),
    /// Population denominators: sum a WorldPop-style raster under every admin unit at one level, roll the sums up the hierarchy, and write ICRTargetPopulation Groups as FHIR NDJSON for `fhir-pipes load` (offline once the raster is cached)
    Population(PopulationArgs),
    /// Read a DHIS2 instance: org units as Locations, indicators as Measures, indicator values as MeasureReports, all as FHIR NDJSON for `fhir-pipes load`
    #[command(subcommand)]
    Dhis2(Box<Dhis2Command>),
    /// Work with the profile given by --profile
    #[command(subcommand)]
    Profile(ProfileCommand),
}

#[derive(Subcommand, Debug)]
pub enum ProfileCommand {
    /// Compare the profile with its implementation guide: extensions, code
    /// systems, profiles and identifier systems it names. Exits 1 on errors
    Check(ProfileCheckArgs),
}

#[derive(clap::Args, Debug)]
pub struct ProfileCheckArgs {
    /// The IG: a FHIR package (package.tgz, a path or an https URL) or a
    /// directory of JSON resources such as SUSHI's fsh-generated/resources
    #[arg(long)]
    pub ig: String,
}

#[derive(Subcommand, Debug)]
pub enum TransformCommand {
    /// The Location registry as a partitioned GeoParquet dataset, with its
    /// hierarchy and geometry, for GIS tools
    Locations(TransformArgs),
}

#[derive(Subcommand, Debug)]
pub enum Dhis2Command {
    /// Org units as ICR Locations (facilities with their Organizations), each carrying its DHIS2 UID
    Locations(Dhis2LocationsArgs),
    /// Indicators as Measures
    Measures(Dhis2MeasuresArgs),
    /// Indicator values from DHIS2 analytics as MeasureReports, one per indicator, org unit and period
    Reports(Box<Dhis2ReportsArgs>),
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
    /// DHIS2 base URL, e.g. https://play.im.dhis2.org/stable-2-41-10 (a trailing /api is accepted); falls back to $FHIR_PIPES_DHIS2_URL
    #[arg(long = "dhis2", env = "FHIR_PIPES_DHIS2_URL")]
    pub url: String,
    /// Short name for this instance (letters, digits, hyphens; up to 16), used in identifier systems, ids and the meta tag, e.g. sl-demo
    #[arg(long)]
    pub instance: String,
    /// DHIS2 personal access token (d2pat_…); falls back to $FHIR_PIPES_DHIS2_TOKEN
    #[arg(
        long = "dhis2-token",
        env = "FHIR_PIPES_DHIS2_TOKEN",
        hide_env_values = true,
        conflicts_with = "user"
    )]
    pub token: Option<String>,
    /// DHIS2 username for Basic auth; falls back to $FHIR_PIPES_DHIS2_USER
    #[arg(
        long = "dhis2-user",
        env = "FHIR_PIPES_DHIS2_USER",
        requires = "password"
    )]
    pub user: Option<String>,
    /// DHIS2 password; falls back to $FHIR_PIPES_DHIS2_PASSWORD
    #[arg(
        long = "dhis2-password",
        env = "FHIR_PIPES_DHIS2_PASSWORD",
        hide_env_values = true,
        requires = "user"
    )]
    pub password: Option<String>,
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2LocationsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    /// Prefix of the Location ids (PREFIX-<uid>); defaults to the instance name
    #[arg(long)]
    pub id_prefix: Option<String>,
    /// Country code put on the top org unit as its national admin code (e.g. SLE); fhir-pipes transform locations partitions by it
    #[arg(long)]
    pub country_code: Option<String>,
    /// DHIS2 level whose org units are facilities (a Location plus an Organization); defaults to the deepest level
    #[arg(long)]
    pub facility_level: Option<u32>,
    /// Only this org unit and everything below it
    #[arg(long)]
    pub root: Option<String>,
    /// Org unit group set UID, repeatable (e.g. Facility Type): each org unit's group in the set is added to its Location's type (and its Organization's, for facilities), coded like a --breakdown on the same set
    #[arg(long = "group-set")]
    pub group_sets: Vec<String>,
    /// Org unit group UID, repeatable (e.g. the group of all districts): each member's Location (and Organization) gets the group in its type. A group in a group set is coded as --group-set codes it; a group in no set under …/CodeSystem/dhis2/INSTANCE/org-unit-group
    #[arg(long = "org-unit-group")]
    pub org_unit_groups: Vec<String>,
    /// Every org unit group, as --org-unit-group
    #[arg(long, conflicts_with = "org_unit_groups")]
    pub all_org_unit_groups: bool,
    /// Output NDJSON file of Location and Organization resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// Which indicators and data elements: by group, one by one, or any mix.
#[derive(clap::Args, Debug, Clone)]
#[group(required = true, multiple = true)]
pub struct IndicatorSelection {
    /// Indicator group UID, repeatable
    #[arg(long = "indicator-group")]
    pub groups: Vec<String>,
    /// Indicator UID, repeatable
    #[arg(long = "indicator")]
    pub indicators: Vec<String>,
    /// Data element group UID, repeatable: each aggregate data element's total becomes a count
    #[arg(long = "data-element-group")]
    pub data_element_groups: Vec<String>,
    /// Data element UID, repeatable
    #[arg(long = "data-element")]
    pub data_elements: Vec<String>,
    /// Data element group set UID, repeatable: every data element in every group of the set
    #[arg(long = "data-element-group-set")]
    pub data_element_group_sets: Vec<String>,
    /// Data set UID, repeatable: its reporting rate (actual ÷ expected reports)
    #[arg(long = "data-set")]
    pub data_sets: Vec<String>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2MeasuresArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    /// Output NDJSON file of Measure resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2ReportsArgs {
    #[command(flatten)]
    pub conn: Dhis2Conn,
    #[command(flatten)]
    pub select: IndicatorSelection,
    /// NDJSON holding the Locations that carry this instance's org unit UIDs: the output of `fhir-pipes dhis2 locations`, or a snapshot's locations.ndjson
    #[arg(long)]
    pub locations: PathBuf,
    /// DHIS2 period, repeatable: ISO (2025, 2025Q1, 202501, 2025W5, …) or relative (LAST_12_MONTHS, THIS_YEAR, …). Needed unless --range is given
    #[arg(long = "period", required_unless_present = "ranges")]
    pub periods: Vec<String>,
    /// Date range START:END, repeatable (ISO dates, both included, e.g. 2026-04-05:2026-05-20): one value per indicator and org unit for the whole range, asked for with DHIS2's startDate and endDate instead of a period. Report ids end in 20260405-20260520
    #[arg(long = "range")]
    pub ranges: Vec<String>,
    /// Campaign every report belongs to, as a reference (e.g. CarePlan/bgd-mr-sia-2026), added as the ICR campaign extension. Report ids do not include it, so one run should cover one campaign's periods
    #[arg(long)]
    pub campaign: Option<String>,
    /// DHIS2 org unit level to report on, repeatable (1 = the top)
    #[arg(long = "level", required = true)]
    pub levels: Vec<u32>,
    /// Only org units at or below this one: each level is asked for as LEVEL-n;ROOT. Needed when
    /// the user can see top-level org units outside the tree being reported on
    #[arg(long)]
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
    /// Where values come from: analytics (DHIS2 computes them) or data-values (raw data values from /api/dataValueSets, added up the --locations hierarchy and computed by fhir-pipes: SUM and AVERAGE_SUM_ORG_UNIT data elements, indicators of #{…}, C{…} and arithmetic, category breakdowns)
    #[arg(long, value_enum, default_value_t = ReportSource::Analytics)]
    pub source: ReportSource,
    /// With --source data-values and --previous: recompute only the periods and ranges with data values updated or deleted since this date or time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS); --out keeps the previous reports for the rest
    #[arg(long, requires = "previous")]
    pub updated_since: Option<String>,
    /// Refuse to run when DHIS2's analytics tables were last built longer ago than this (e.g. 36h, 7d, 2w): values are only as fresh as that build. Without it, a build over 7 days old is warned about
    #[arg(long, value_parser = crate::dhis2::api::parse_age)]
    pub max_analytics_age: Option<std::time::Duration>,
    /// Analytics requests in flight at once; reports are written as each batch arrives, in the same order whatever the setting
    #[arg(long, default_value_t = DEFAULT_ANALYTICS_CONCURRENCY)]
    pub concurrency: usize,
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
    /// Continue an interrupted run after its last fully written batch instead of starting over. Refused if anything that shapes the batches has changed since (arguments, org unit counts, the analytics build, the --locations or --previous file)
    #[arg(long)]
    pub resume: bool,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct IndexArgs {
    /// Snapshot directory containing locations.ndjson
    #[arg(long)]
    pub snapshot: PathBuf,
    /// SCHEME:LEVEL to ensure on every positioned Location, repeatable (e.g. quadkey:18, geohash:8)
    #[arg(long = "spatial-index", required = true)]
    pub spatial_index: Vec<String>,
    /// Recompute cells at the requested levels even where one is present
    #[arg(long)]
    pub refresh: bool,
    /// Output NDJSON file of changed resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct PopulationArgs {
    /// Snapshot directory containing locations.ndjson
    #[arg(long)]
    pub snapshot: PathBuf,
    /// GeoTIFF to sum: a local path, or an http(s) URL fetched once into the raster cache
    #[arg(long)]
    pub raster: String,
    /// Admin level to measure from the raster (0 = country); every admin ancestor gets a rolled-up total
    #[arg(
        long,
        required_unless_present = "type_code",
        conflicts_with = "type_code"
    )]
    pub level: Option<u8>,
    /// Instead of an admin level: measure every Location whose icr-location-type code is this
    /// (e.g. facility-catchment). Each polygon is summed on its own; nothing is rolled up into ancestors
    #[arg(long = "type", value_name = "CODE")]
    pub type_code: Option<String>,
    /// Reference year of the raster; part of each Group's id and the default estimate-date
    #[arg(long)]
    pub year: u16,
    /// Date the estimate was made (YYYY-MM-DD) for the estimate-date extension; defaults to YEAR-01-01, the raster's reference year
    #[arg(long)]
    pub estimate_date: Option<String>,
    /// denominator-source code (icr-denominator-source-cs)
    #[arg(long, default_value = "worldpop")]
    pub source: String,
    /// Mark every written Group as the planning denominator
    #[arg(long)]
    pub planning_denominator: bool,
    /// Output NDJSON file of Group resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Raster cache directory (default: SNAPSHOT/rasters)
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Attempts per HTTP request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout for the raster download, in seconds
    #[arg(long, default_value_t = DEFAULT_RASTER_TIMEOUT_SECS)]
    pub timeout: u64,
}

#[derive(clap::Args, Debug, Clone)]
pub struct TransformArgs {
    /// Snapshot directory containing locations.ndjson
    #[arg(long)]
    pub snapshot: PathBuf,
    /// Output directory; the dataset is written to OUT/locations
    #[arg(long)]
    pub out: PathBuf,
    /// Override the country code derived from the level-0 admin unit
    #[arg(long)]
    pub country: Option<String>,
    /// Rows per Parquet row group
    #[arg(long, default_value_t = DEFAULT_ROW_GROUP_SIZE)]
    pub row_group_size: usize,
    /// Comma-separated partition keys: any of country, geom_type, tier, type
    #[arg(long, default_value = DEFAULT_PARTITION_BY)]
    pub partition_by: String,
    /// Also write Locations that have no point and no usable boundary, with the geometry columns null (by default they are reported and left out)
    #[arg(long)]
    pub keep_without_geometry: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct InspectArgs {
    /// Output directory previously written by transform
    #[arg(long)]
    pub out: PathBuf,
}

/// How extract and load authenticate: no header, a fixed bearer token, or an
/// OAuth client id and secret that fhir-pipes trades for tokens itself.
#[derive(clap::Args, Debug, Clone)]
pub struct AuthArgs {
    /// Bearer token; falls back to $FHIR_PIPES_TOKEN
    #[arg(
        long,
        env = "FHIR_PIPES_TOKEN",
        hide_env_values = true,
        conflicts_with = "client_id"
    )]
    pub token: Option<String>,
    /// OAuth client id; fhir-pipes gets its own token (client-credentials grant) and renews it before it expires. Falls back to $FHIR_PIPES_CLIENT_ID
    #[arg(long, env = "FHIR_PIPES_CLIENT_ID", requires_all = ["client_secret", "token_url"])]
    pub client_id: Option<String>,
    /// OAuth client secret; falls back to $FHIR_PIPES_CLIENT_SECRET
    #[arg(
        long,
        env = "FHIR_PIPES_CLIENT_SECRET",
        hide_env_values = true,
        requires = "client_id"
    )]
    pub client_secret: Option<String>,
    /// OAuth token endpoint the client id and secret are sent to; falls back to $FHIR_PIPES_TOKEN_URL
    #[arg(long, env = "FHIR_PIPES_TOKEN_URL", requires = "client_id")]
    pub token_url: Option<String>,
}

impl AuthArgs {
    pub fn auth(&self) -> Auth {
        match (&self.client_id, &self.client_secret, &self.token_url) {
            (Some(id), Some(secret), Some(url)) => Auth::ClientCredentials {
                token_url: url.clone(),
                client_id: id.clone(),
                client_secret: secret.clone(),
            },
            _ => self.token.clone().map_or(Auth::None, Auth::Token),
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct ExtractArgs {
    /// Base URL of the FHIR server (Location is appended)
    #[arg(long)]
    pub server: String,
    #[command(flatten)]
    pub auth: AuthArgs,
    /// Snapshot directory to create or update
    #[arg(long)]
    pub snapshot: PathBuf,
    /// Ignore the existing snapshot and watermark; fetch everything
    #[arg(long)]
    pub full: bool,
    /// Use this instant instead of the stored watermark for this run
    #[arg(long)]
    pub since: Option<String>,
    /// Boundary fetch worker threads
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    pub concurrency: usize,
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Abort after this many consecutive boundary failures with no success yet; 0 disables
    #[arg(long, default_value_t = DEFAULT_MAX_CONSECUTIVE_FAILURES)]
    pub max_consecutive_failures: usize,
    /// Neither read nor write the boundary cache (holds fetched boundaries in memory)
    #[arg(long)]
    pub no_cache: bool,
    /// Skip cache reads but still write fetched boundaries to it
    #[arg(long)]
    pub refresh: bool,
    /// Boundary cache directory (default: SNAPSHOT/boundaries)
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}

#[derive(clap::Args, Debug, Clone)]
pub struct ExportArgs {
    /// Base URL of the FHIR server (the resource type is appended)
    #[arg(long)]
    pub server: String,
    #[command(flatten)]
    pub auth: AuthArgs,
    /// Resource type to download, e.g. MeasureReport; repeat for several
    #[arg(long = "type", value_name = "TYPE", required = true)]
    pub types: Vec<String>,
    /// Output directory; each type is written to OUT/<TYPE>.ndjson
    #[arg(long)]
    pub out: PathBuf,
    /// Download only what changed since the last export into OUT (by
    /// meta.lastUpdated) and merge it by id; a type whose count then differs
    /// from the server's is downloaded in full
    #[arg(long)]
    pub incremental: bool,
    /// Also write the report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub extract: ExtractArgs,
    /// Output directory; the dataset is written to OUT/locations
    #[arg(long)]
    pub out: PathBuf,
    /// Override the country code derived from the level-0 admin unit
    #[arg(long)]
    pub country: Option<String>,
    /// Rows per Parquet row group
    #[arg(long, default_value_t = DEFAULT_ROW_GROUP_SIZE)]
    pub row_group_size: usize,
    /// Comma-separated partition keys: any of country, geom_type, tier, type
    #[arg(long, default_value = DEFAULT_PARTITION_BY)]
    pub partition_by: String,
    /// Also write Locations that have no point and no usable boundary, with the geometry columns null (by default they are reported and left out)
    #[arg(long)]
    pub keep_without_geometry: bool,
}

impl RunArgs {
    pub fn transform_args(&self) -> TransformArgs {
        TransformArgs {
            snapshot: self.extract.snapshot.clone(),
            out: self.out.clone(),
            country: self.country.clone(),
            row_group_size: self.row_group_size,
            partition_by: self.partition_by.clone(),
            keep_without_geometry: self.keep_without_geometry,
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct DiffArgs {
    /// Snapshot directory containing locations.ndjson
    #[arg(long)]
    pub snapshot: PathBuf,
    /// Edited file: .geojson/.json (FeatureCollection), .geojsonl/.geojsons (one Feature per line), or .parquet
    #[arg(long = "in")]
    pub input: PathBuf,
    /// Output NDJSON file of changed resources
    #[arg(long)]
    pub out: PathBuf,
    /// Also write the diff report as JSON to this file
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct LoadArgs {
    /// Base URL of the FHIR server
    #[arg(long)]
    pub server: String,
    #[command(flatten)]
    pub auth: AuthArgs,
    /// NDJSON of FHIR resources to load (normally diff output)
    #[arg(long = "in")]
    pub input: PathBuf,
    /// Run the preflight and print the bundle plan without posting
    #[arg(long)]
    pub dry_run: bool,
    /// Resources per transaction bundle
    #[arg(long, default_value_t = DEFAULT_BATCH_SIZE)]
    pub batch_size: usize,
    /// Bundles posted at once. A bundle waits for the earlier bundles holding resources it references (partOf parents, managing Organizations), so parents still land first; otherwise bundles commit in any order
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}
