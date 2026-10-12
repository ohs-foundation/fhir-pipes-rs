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

#[derive(Parser, Debug)]
#[command(
    name = "kiln",
    version,
    about = "Bridge between a FHIR Location registry and GeoParquet"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Convert a snapshot into a partitioned GeoParquet dataset (offline)
    Transform(TransformArgs),
    /// Summarise a written dataset
    Inspect(InspectArgs),
    /// Fetch Locations from a FHIR server into a snapshot (incremental)
    Extract(ExtractArgs),
    /// Extract then transform
    Run(RunArgs),
    /// Compare an edited GeoJSON or GeoParquet file to the snapshot; write changed Locations as FHIR NDJSON (offline)
    Diff(DiffArgs),
    /// Send changed resources to a FHIR server as version-checked transaction bundles
    Load(LoadArgs),
    /// Backfill spatial-index cells (quadkey, geohash) onto snapshot Locations that lack them; writes the changed resources as FHIR NDJSON for `kiln load` (offline)
    Index(IndexArgs),
    /// Population denominators: sum a WorldPop-style raster under every admin unit at one level, roll the sums up the hierarchy, and write ICRTargetPopulation Groups as FHIR NDJSON for `kiln load` (offline once the raster is cached)
    Population(PopulationArgs),
    /// Read a DHIS2 instance: org units as Locations, indicators as Measures, indicator values as MeasureReports, all as FHIR NDJSON for `kiln load`
    #[command(subcommand)]
    Dhis2(Dhis2Command),
}

#[derive(Subcommand, Debug)]
pub enum Dhis2Command {
    /// Org units as ICR Locations (facilities with their Organizations), each carrying its DHIS2 UID
    Locations(Dhis2LocationsArgs),
    /// Indicators as Measures
    Measures(Dhis2MeasuresArgs),
    /// Indicator values from DHIS2 analytics as MeasureReports, one per indicator, org unit and period
    Reports(Dhis2ReportsArgs),
}

/// Where the DHIS2 instance is, how to sign in, and the short name kiln
/// files it under.
#[derive(clap::Args, Debug, Clone)]
pub struct Dhis2Conn {
    /// DHIS2 base URL, e.g. https://play.im.dhis2.org/stable-2-41-10 (a trailing /api is accepted); falls back to $KILN_DHIS2_URL
    #[arg(long = "dhis2", env = "KILN_DHIS2_URL")]
    pub url: String,
    /// Short name for this instance (letters, digits, hyphens; up to 16), used in identifier systems, ids and the meta tag, e.g. sl-demo
    #[arg(long)]
    pub instance: String,
    /// DHIS2 personal access token (d2pat_…); falls back to $KILN_DHIS2_TOKEN
    #[arg(
        long = "dhis2-token",
        env = "KILN_DHIS2_TOKEN",
        hide_env_values = true,
        conflicts_with = "user"
    )]
    pub token: Option<String>,
    /// DHIS2 username for Basic auth; falls back to $KILN_DHIS2_USER
    #[arg(long = "dhis2-user", env = "KILN_DHIS2_USER", requires = "password")]
    pub user: Option<String>,
    /// DHIS2 password; falls back to $KILN_DHIS2_PASSWORD
    #[arg(
        long = "dhis2-password",
        env = "KILN_DHIS2_PASSWORD",
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
    /// Country code put on the top org unit as its national admin code (e.g. SLE); kiln transform partitions by it
    #[arg(long)]
    pub country_code: Option<String>,
    /// DHIS2 level whose org units are facilities (a Location plus an Organization); defaults to the deepest level
    #[arg(long)]
    pub facility_level: Option<u32>,
    /// Only this org unit and everything below it
    #[arg(long)]
    pub root: Option<String>,
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
    /// NDJSON holding the Locations that carry this instance's org unit UIDs: the output of `kiln dhis2 locations`, or a snapshot's locations.ndjson
    #[arg(long)]
    pub locations: PathBuf,
    /// DHIS2 period, repeatable: ISO (2025, 2025Q1, 202501, 2025W5, …) or relative (LAST_12_MONTHS, THIS_YEAR, …)
    #[arg(long = "period", required = true)]
    pub periods: Vec<String>,
    /// DHIS2 org unit level to report on, repeatable (1 = the top)
    #[arg(long = "level", required = true)]
    pub levels: Vec<u32>,
    /// Also break every value down by this DHIS2 dimension, repeatable: a category (e.g. Fixed/Outreach), an org unit group set (e.g. Facility Type) or a data element group set. Each becomes a stratifier on the report; DHIS2 leaves out items the dimension does not apply to
    #[arg(long = "breakdown")]
    pub breakdowns: Vec<String>,
    /// Indicators per analytics request (halved automatically when DHIS2 says a result is too large)
    #[arg(long, default_value_t = DEFAULT_INDICATORS_PER_REQUEST)]
    pub chunk: usize,
    /// Output NDJSON file of MeasureReport resources
    #[arg(long)]
    pub out: PathBuf,
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
    #[arg(long)]
    pub level: u8,
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
}

#[derive(clap::Args, Debug, Clone)]
pub struct InspectArgs {
    /// Output directory previously written by transform
    #[arg(long)]
    pub out: PathBuf,
}

/// How extract and load authenticate: no header, a fixed bearer token, or an
/// OAuth client id and secret that kiln trades for tokens itself.
#[derive(clap::Args, Debug, Clone)]
pub struct AuthArgs {
    /// Bearer token; falls back to $KILN_TOKEN
    #[arg(
        long,
        env = "KILN_TOKEN",
        hide_env_values = true,
        conflicts_with = "client_id"
    )]
    pub token: Option<String>,
    /// OAuth client id; kiln gets its own token (client-credentials grant) and renews it before it expires. Falls back to $KILN_CLIENT_ID
    #[arg(long, env = "KILN_CLIENT_ID", requires_all = ["client_secret", "token_url"])]
    pub client_id: Option<String>,
    /// OAuth client secret; falls back to $KILN_CLIENT_SECRET
    #[arg(
        long,
        env = "KILN_CLIENT_SECRET",
        hide_env_values = true,
        requires = "client_id"
    )]
    pub client_secret: Option<String>,
    /// OAuth token endpoint the client id and secret are sent to; falls back to $KILN_TOKEN_URL
    #[arg(long, env = "KILN_TOKEN_URL", requires = "client_id")]
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
}

impl RunArgs {
    pub fn transform_args(&self) -> TransformArgs {
        TransformArgs {
            snapshot: self.extract.snapshot.clone(),
            out: self.out.clone(),
            country: self.country.clone(),
            row_group_size: self.row_group_size,
            partition_by: self.partition_by.clone(),
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
    /// Attempts per request
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: usize,
    /// Total timeout per HTTP request, in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}
