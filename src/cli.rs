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
