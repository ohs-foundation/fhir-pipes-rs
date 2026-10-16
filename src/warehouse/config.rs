//! Settings for `refresh`: the config file, overridden by command-line flags and their
//! environment variables, with defaults for anything neither sets.
//!
//! Relative paths in the config file are resolved against the file's own
//! directory, so one file can be run from anywhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{AuthArgs, DEFAULT_BULK_TIMEOUT_SECS};

/// The config file (`fhir-pipes.toml`). Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// The warehouse directory (raw/, parquet/, views/, tiles/, manifest.json).
    pub data: Option<PathBuf>,
    /// Where the ViewDefinitions are: a directory of JSON files, such as an IG
    /// build's fsh-generated/resources.
    pub views: Option<PathBuf>,
    /// The profile for the locations step: a file path (relative to this
    /// file) or an https URL. See docs/profiles.md.
    pub profile: Option<String>,
    #[serde(default)]
    pub server: ServerFile,
    #[serde(default)]
    pub tools: ToolsFile,
    #[serde(default)]
    pub tables: TablesFile,
    #[serde(default)]
    pub partition: PartitionFile,
    #[serde(default)]
    pub tiles: TilesFile,
    pub push: Option<PushFile>,
    pub catalog: Option<CatalogFile>,
    pub database: Option<DatabaseFile>,
    #[serde(default)]
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    pub base: Option<String>,
    pub name: Option<String>,
    pub token_url: Option<String>,
    /// How the views step downloads resources (default `search`).
    pub fetch: Option<Fetch>,
    /// With `fetch = "bulk"`: seconds to wait for an export before cancelling
    /// it.
    pub bulk_timeout: Option<u64>,
    /// HTTP Basic username; the password comes only from FHIR_PIPES_PASSWORD.
    pub username: Option<String>,
}

/// How resources are downloaded from the FHIR server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fetch {
    /// Paged searches (`export`).
    #[default]
    Search,
    /// A Bulk Data `$export` (`export --bulk`).
    Bulk,
}

impl Fetch {
    pub fn name(self) -> &'static str {
        match self {
            Fetch::Search => "search",
            Fetch::Bulk => "bulk",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsFile {
    pub duckdb: Option<String>,
    pub tippecanoe: Option<String>,
    pub rclone: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TablesFile {
    /// Prefixes dropped from table names after snake-casing the view name
    /// (`IcrIndicatorValue` → `icr_indicator_value` → `indicator_value`).
    #[serde(default)]
    pub strip_prefix: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionFile {
    /// The partition column added to every view table (default `country`).
    pub column: Option<String>,
    /// How a view's rows find their partition value: the first rule whose `key`
    /// column the view has is used. A view matching no rule is written to the
    /// null partition.
    #[serde(default)]
    pub lookup: Vec<Lookup>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    /// The column in the view.
    pub key: String,
    /// The warehouse table holding the partition value for each key.
    pub table: String,
    /// The column in that table matching `key` (default: the same name).
    pub table_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TilesFile {
    /// Countries to build tiles for (`country=` partitions of the locations
    /// table). Empty: every country in it.
    #[serde(default)]
    pub countries: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushFile {
    /// rclone remote name.
    pub remote: String,
    pub bucket: String,
    /// Directories of the warehouse to mirror (sync, deleting what is gone).
    #[serde(default = "default_push_dirs")]
    pub dirs: Vec<String>,
    /// Single files to copy, when present.
    #[serde(default = "default_push_files")]
    pub files: Vec<String>,
    /// rclone exclude patterns for the directory syncs.
    #[serde(default = "default_push_exclude")]
    pub exclude: Vec<String>,
}

fn default_push_dirs() -> Vec<String> {
    vec!["parquet".into(), "tiles".into(), "views".into()]
}

fn default_push_files() -> Vec<String> {
    vec!["manifest.json".into()]
}

fn default_push_exclude() -> Vec<String> {
    vec![".DS_Store".into()]
}

/// `[catalog]`: write catalog.sql, a DuckDB view per table (see catalog.rs).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    /// Where the views read the tables from: unset, paths relative to the
    /// warehouse directory; an http(s) URL, each table's files listed; any
    /// other prefix DuckDB reads (s3://, r2://, gcs://, a path), globbed.
    pub base_url: Option<String>,
    /// SQL appended after the generated views, such as views joining tables
    /// (relative to this file).
    pub extra: Option<PathBuf>,
    /// Also build catalog.duckdb with the duckdb command.
    #[serde(default)]
    pub duckdb_file: bool,
    /// SQL run before the views when building catalog.duckdb, such as a
    /// `CREATE SECRET` for s3:// (relative to this file).
    pub duckdb_setup: Option<PathBuf>,
}

/// `[catalog]`, with its paths resolved.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub base_url: Option<String>,
    pub extra: Option<PathBuf>,
    pub duckdb_file: bool,
    pub duckdb_setup: Option<PathBuf>,
}

/// `[database]`: load the view tables into PostgreSQL (see database.rs).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseFile {
    /// The environment variable holding the connection string
    /// (default FHIR_PIPES_DATABASE_URL); it is never read from this file.
    pub url_env: Option<String>,
    /// The schema the tables go in (default `public`).
    pub schema: Option<String>,
    /// The view tables to load (default: all of them).
    pub tables: Option<Vec<String>>,
}

/// `[database]`, with its defaults.
#[derive(Debug, Clone)]
pub struct Database {
    pub url_env: String,
    pub schema: String,
    // Read only by the database step, which needs the postgres feature.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub tables: Option<Vec<String>>,
}

/// An external command run after the manifest is written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub name: String,
    /// Run only when this step ran (e.g. `locations`). Unset: always.
    pub after: Option<String>,
    /// The command and its arguments. A program path with a `/` is relative to
    /// the config file.
    pub run: Vec<String>,
}

/// Everything a run needs, resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    pub data: PathBuf,
    pub views: Option<PathBuf>,
    pub server: String,
    pub server_name: String,
    /// How to authenticate to the FHIR server, as the other commands take it.
    pub auth: AuthArgs,
    pub fetch: Fetch,
    pub bulk_timeout: u64,
    pub duckdb: String,
    pub tippecanoe: String,
    pub rclone: String,
    pub strip_prefix: Vec<String>,
    pub partition_column: String,
    pub lookups: Vec<Lookup>,
    pub tile_countries: Vec<String>,
    pub push: Option<PushFile>,
    pub catalog: Option<Catalog>,
    pub database: Option<Database>,
    pub hooks: Vec<Hook>,
    /// The config file's directory, for hook programs given as paths.
    pub base_dir: PathBuf,
    /// The profile for the locations step: a path or an https URL.
    pub profile: Option<String>,
    pub threads: usize,
    pub reuse_raw: bool,
    pub full: bool,
    pub dry_run: bool,
}

impl Settings {
    /// How the run authenticates, for `--check`.
    pub fn auth_kind(&self) -> &'static str {
        if self.auth.token.is_some() {
            "bearer token"
        } else if self.auth.client_id.is_some() {
            "client credentials"
        } else if self.auth.username.is_some() {
            "basic"
        } else {
            "none"
        }
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.data.join("raw").join(&self.server_name)
    }

    pub fn parquet_dir(&self) -> PathBuf {
        self.data.join("parquet")
    }

    pub fn views_out_dir(&self) -> PathBuf {
        self.data.join("views")
    }
}

/// Read a config file, or the empty config when there is none.
pub fn load(path: Option<&Path>) -> Result<(File, PathBuf)> {
    let found = match path {
        Some(p) => Some(p.to_path_buf()),
        // fhir-warehouse.toml is the name the file had before fhir-pipes.
        None => ["fhir-pipes.toml", "fhir-warehouse.toml"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists()),
    };
    let Some(path) = found else {
        return Ok((File::default(), std::env::current_dir()?));
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let dir = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok((file, dir))
}

/// Resolve a path from the config file against the file's directory.
pub fn resolve(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Values given on the command line (or through their environment variables).
#[derive(Debug, Default)]
pub struct Overrides {
    pub profile: Option<String>,
    pub data: Option<PathBuf>,
    pub views: Option<PathBuf>,
    pub server: Option<String>,
    pub server_name: Option<String>,
    pub token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub countries: Vec<String>,
    pub threads: Option<usize>,
    pub reuse_raw: bool,
    pub full: bool,
    pub dry_run: bool,
}

pub fn settings(file: File, base: PathBuf, o: Overrides) -> Result<Settings> {
    let data = match (o.data, file.data) {
        (Some(p), _) => p,
        (None, Some(p)) => resolve(&base, &p),
        (None, None) => {
            bail!(
                "no warehouse directory: pass --data, set FHIR_PIPES_DATA, or set `data` in the config"
            )
        }
    };
    let views = o.views.or_else(|| file.views.map(|p| resolve(&base, &p)));
    let username = o.username.or(file.server.username);
    let kinds: Vec<&str> = [
        (o.token.is_some(), "a token (FHIR_PIPES_TOKEN)"),
        (o.client_id.is_some(), "a client id (FHIR_PIPES_CLIENT_ID)"),
        (username.is_some(), "a username (FHIR_PIPES_USERNAME)"),
    ]
    .into_iter()
    .filter_map(|(set, what)| set.then_some(what))
    .collect();
    if kinds.len() > 1 {
        bail!("use one way to authenticate, not {}", kinds.join(" and "));
    }
    let none = AuthArgs {
        token: None,
        client_id: None,
        client_secret: None,
        token_url: None,
        username: None,
        password: None,
    };
    let auth = match (o.client_id, o.client_secret, username, o.password) {
        (Some(id), Some(secret), _, _) => AuthArgs {
            client_id: Some(id),
            client_secret: Some(secret),
            token_url: Some(o.token_url.or(file.server.token_url).context(
                "FHIR_PIPES_CLIENT_ID is set but no token URL: pass --token-url or set server.token_url",
            )?),
            ..none
        },
        (Some(_), None, _, _) => bail!("set FHIR_PIPES_CLIENT_SECRET with FHIR_PIPES_CLIENT_ID"),
        (None, _, Some(user), Some(password)) => AuthArgs {
            username: Some(user),
            password: Some(password),
            ..none
        },
        (None, _, Some(_), None) => {
            bail!("a username is set but no password: set FHIR_PIPES_PASSWORD")
        }
        _ => AuthArgs {
            token: o.token,
            ..none
        },
    };
    let threads = o
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let countries = if o.countries.is_empty() {
        file.tiles.countries
    } else {
        o.countries
    };
    Ok(Settings {
        data,
        views,
        server: o
            .server
            .or(file.server.base)
            .unwrap_or_else(|| "http://localhost:3447/fhir".into()),
        server_name: o
            .server_name
            .or(file.server.name)
            .unwrap_or_else(|| "hapi-local".into()),
        auth,
        fetch: file.server.fetch.unwrap_or_default(),
        bulk_timeout: file
            .server
            .bulk_timeout
            .unwrap_or(DEFAULT_BULK_TIMEOUT_SECS),
        duckdb: file.tools.duckdb.unwrap_or_else(|| "duckdb".into()),
        tippecanoe: file.tools.tippecanoe.unwrap_or_else(|| "tippecanoe".into()),
        rclone: file.tools.rclone.unwrap_or_else(|| "rclone".into()),
        strip_prefix: file.tables.strip_prefix,
        partition_column: file.partition.column.unwrap_or_else(|| "country".into()),
        lookups: file.partition.lookup,
        tile_countries: countries,
        push: file.push,
        catalog: file.catalog.map(|c| Catalog {
            base_url: c.base_url.filter(|b| !b.is_empty()),
            extra: c.extra.map(|p| resolve(&base, &p)),
            duckdb_file: c.duckdb_file,
            duckdb_setup: c.duckdb_setup.map(|p| resolve(&base, &p)),
        }),
        database: file.database.map(|d| Database {
            url_env: d
                .url_env
                .unwrap_or_else(|| "FHIR_PIPES_DATABASE_URL".into()),
            schema: d.schema.unwrap_or_else(|| "public".into()),
            tables: d.tables,
        }),
        hooks: file.hooks,
        profile: o.profile.or_else(|| {
            file.profile.map(|p| {
                if p.starts_with("https://") || p.starts_with("http://") {
                    p
                } else {
                    resolve(&base, std::path::Path::new(&p))
                        .display()
                        .to_string()
                }
            })
        }),
        base_dir: base,
        threads,
        reuse_raw: o.reuse_raw,
        full: o.full,
        dry_run: o.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths_resolve_against_the_file() {
        let file: File = toml::from_str("data = \"../data\"\nviews = \"/abs/views\"").unwrap();
        let s = settings(file, PathBuf::from("/repo/tools"), Overrides::default()).unwrap();
        assert_eq!(s.data, PathBuf::from("/repo/tools/../data"));
        assert_eq!(s.views, Some(PathBuf::from("/abs/views")));
    }

    #[test]
    fn flags_win_over_the_file() {
        let file: File =
            toml::from_str("data = \"d\"\n[server]\nbase = \"http://file\"\nname = \"file\"")
                .unwrap();
        let o = Overrides {
            server: Some("http://flag".into()),
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert_eq!(s.server, "http://flag");
        assert_eq!(s.server_name, "file");
    }

    #[test]
    fn secret_without_token_url_is_an_error() {
        let file: File = toml::from_str("data = \"d\"").unwrap();
        let o = Overrides {
            client_id: Some("id".into()),
            client_secret: Some("s".into()),
            ..Default::default()
        };
        assert!(settings(file, PathBuf::from("/b"), o).is_err());
    }

    #[test]
    fn fetch_is_search_unless_the_file_says_bulk() {
        let s = |text: &str| {
            let file: File = toml::from_str(text).unwrap();
            settings(file, PathBuf::from("/b"), Overrides::default()).unwrap()
        };
        let search = s("data = \"d\"");
        assert_eq!(search.fetch, Fetch::Search);
        assert_eq!(search.bulk_timeout, DEFAULT_BULK_TIMEOUT_SECS);
        let bulk = s("data = \"d\"\n[server]\nfetch = \"bulk\"\nbulk_timeout = 60");
        assert_eq!(bulk.fetch, Fetch::Bulk);
        assert_eq!(bulk.bulk_timeout, 60);
        assert!(toml::from_str::<File>("[server]\nfetch = \"jdbc\"").is_err());
    }

    #[test]
    fn basic_auth_takes_the_password_from_the_overrides() {
        let file: File = toml::from_str("data = \"d\"\n[server]\nusername = \"admin\"").unwrap();
        let o = Overrides {
            password: Some("Admin123".into()),
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert_eq!(s.auth_kind(), "basic");
        assert_eq!(s.auth.username.as_deref(), Some("admin"));

        let file: File = toml::from_str("data = \"d\"\n[server]\nusername = \"admin\"").unwrap();
        let err = settings(file, PathBuf::from("/b"), Overrides::default()).unwrap_err();
        assert!(err.to_string().contains("FHIR_PIPES_PASSWORD"), "{err}");

        let file: File = toml::from_str("data = \"d\"").unwrap();
        let o = Overrides {
            token: Some("t".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        };
        let err = settings(file, PathBuf::from("/b"), o).unwrap_err();
        assert!(
            err.to_string().contains("use one way to authenticate"),
            "{err}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<File>("dta = \"typo\"").is_err());
    }
}
