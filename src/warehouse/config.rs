//! Settings for `refresh`: the config file, overridden by command-line flags and their
//! environment variables, with defaults for anything neither sets.
//!
//! Relative paths in the config file are resolved against the file's own
//! directory, so one file can be run from anywhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{AuthArgs, DEFAULT_BULK_TIMEOUT_SECS, DEFAULT_EXPORT_CONCURRENCY};
use crate::extract::client::TokenAuth;
use crate::extract::google::{self, Credentials};
use crate::extract::page::ServerKind;

use super::Step;

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
    pub source: SourceFile,
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

/// `[source]`: where the resources come from when not from the server.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// NDJSON, Bundle and resource files, directories of them, or glob
    /// patterns, relative to this file. Set, the run reads these instead of a
    /// FHIR server (see files.rs).
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    pub base: Option<String>,
    pub name: Option<String>,
    pub token_url: Option<String>,
    /// How the client id and secret are sent to `token_url`: `post` (the
    /// default) or `basic` (`--token-auth`).
    pub token_auth: Option<TokenAuth>,
    /// The OAuth scope asked for with the client id (`--scope`).
    pub scope: Option<String>,
    /// The kind of FHIR server, for its search parameters (`--server-kind`,
    /// default `generic`).
    pub kind: Option<ServerKind>,
    /// How the views step downloads resources (default `search`).
    pub fetch: Option<Fetch>,
    /// With `fetch = "bulk"`: seconds to wait for an export before cancelling
    /// it.
    pub bulk_timeout: Option<u64>,
    /// With `fetch = "search"`: search requests in flight at once (`export
    /// --concurrency`).
    pub concurrency: Option<usize>,
    /// HTTP Basic username; the password comes only from FHIR_PIPES_PASSWORD.
    pub username: Option<String>,
    /// Google Application Default Credentials (`--google-auth`).
    pub google_auth: Option<bool>,
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
    /// The HAPI server's PostgreSQL database (`export --hapi-db`), its
    /// connection string in $FHIR_PIPES_HAPI_DATABASE_URL.
    #[serde(rename = "hapi-db")]
    HapiDb,
}

impl Fetch {
    pub fn name(self) -> &'static str {
        match self {
            Fetch::Search => "search",
            Fetch::Bulk => "bulk",
            Fetch::HapiDb => "hapi-db",
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

/// `[catalog]`: write catalog.sql, a DuckDB view per table, and optionally
/// catalog.spark.sql, a Spark SQL table per table (see catalog.rs).
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
    /// Also write catalog.spark.sql: a Spark SQL table per warehouse table.
    #[serde(default)]
    pub spark: bool,
    /// Where Spark reads the warehouse directory: s3a://, hdfs://, file:, or
    /// a path on the Spark machine (default: the warehouse directory).
    pub spark_location: Option<String>,
    /// The Spark database the tables go in (default `fhir`).
    pub spark_database: Option<String>,
    /// Spark SQL appended after the tables, such as views joining them
    /// (relative to this file).
    pub spark_extra: Option<PathBuf>,
}

/// `[catalog]`, with its paths resolved.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub base_url: Option<String>,
    pub extra: Option<PathBuf>,
    pub duckdb_file: bool,
    pub duckdb_setup: Option<PathBuf>,
    /// catalog.spark.sql, when `spark = true`.
    pub spark: Option<Spark>,
}

/// The Spark settings of `[catalog]`.
#[derive(Debug, Clone)]
pub struct Spark {
    /// Unset: the warehouse directory, as a file: URL.
    pub location: Option<String>,
    pub database: String,
    pub extra: Option<PathBuf>,
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
    /// How the tables are loaded (default `replace`).
    pub mode: Option<Mode>,
}

/// How the database step loads a table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every row, every run.
    #[default]
    Replace,
    /// Only the rows of the resources that changed since the last load.
    Upsert,
}

/// `[database]`, with its defaults.
#[derive(Debug, Clone)]
pub struct Database {
    pub url_env: String,
    pub schema: String,
    // Read only by the database step, which needs the postgres feature.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub tables: Option<Vec<String>>,
    pub mode: Mode,
}

/// An external command run after the manifest is written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub name: String,
    /// Run only when this step ran (e.g. `locations`). Unset: always.
    #[serde(default, deserialize_with = "hook_after")]
    pub after: Option<Step>,
    /// The command and its arguments. A program path with a `/` is relative to
    /// the config file.
    pub run: Vec<String>,
}

/// A hook's `after`: a step that runs before the hooks, in any case
/// (`Views` is `views`). `hooks` and `push` are refused, since a hook could
/// never run after either.
fn hook_after<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Step>, D::Error> {
    use clap::ValueEnum;
    let name = String::deserialize(d)?;
    match Step::from_str(&name, true) {
        Ok(step) if !matches!(step, Step::Hooks | Step::Push) => Ok(Some(step)),
        _ => {
            let steps: Vec<&str> = Step::value_variants()
                .iter()
                .filter(|s| !matches!(s, Step::Hooks | Step::Push))
                .map(|s| s.name())
                .collect();
            Err(serde::de::Error::custom(format!(
                "`after = \"{name}\"`: a hook runs after one of {}, which run before the hooks",
                steps.join(", ")
            )))
        }
    }
}

/// Everything a run needs, resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Absolute, since the hooks get it as DATA and run in the config file's
    /// directory, not the current one.
    pub data: PathBuf,
    pub views: Option<PathBuf>,
    /// The file source: files, directories and glob patterns, resolved. Empty:
    /// the resources come from the server.
    pub files: Vec<PathBuf>,
    pub server: String,
    pub server_name: String,
    /// How to authenticate to the FHIR server, as the other commands take it.
    pub auth: AuthArgs,
    /// The kind of FHIR server, for the searches of both FHIR steps.
    pub server_kind: ServerKind,
    pub fetch: Fetch,
    pub bulk_timeout: u64,
    /// Search requests in flight at once in the views step.
    pub concurrency: usize,
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
    /// Whether the run reads files instead of a FHIR server.
    pub fn reads_files(&self) -> bool {
        !self.files.is_empty()
    }

    /// Whether `[database]` loads only what changed (`mode = "upsert"`).
    pub fn upsert(&self) -> bool {
        self.database
            .as_ref()
            .is_some_and(|d| d.mode == Mode::Upsert)
    }

    /// The server the run reads, for the manifest, last-refresh.json and the
    /// hooks' FHIR_BASE; none with a file source.
    pub fn server_used(&self) -> Option<&str> {
        (!self.reads_files()).then_some(self.server.as_str())
    }

    /// The file source's inputs as given, for the manifest and
    /// last-refresh.json; empty when the run reads a server.
    pub fn inputs(&self) -> Vec<String> {
        self.files.iter().map(|p| p.display().to_string()).collect()
    }

    /// How the run authenticates, for `--check`.
    pub fn auth_kind(&self) -> &'static str {
        if self.auth.google_auth {
            "Google application default credentials"
        } else if self.auth.token.is_some() {
            "bearer token"
        } else if self.auth.client_id.is_some() {
            "client credentials"
        } else if self.auth.username.is_some() {
            "basic"
        } else {
            "none"
        }
    }

    /// `auth_kind`, and for Google where the credentials are, for `--check`.
    /// Never shows a secret.
    pub fn auth_description(&self) -> String {
        if !self.auth.google_auth {
            return self.auth_kind().to_string();
        }
        match Credentials::application_default() {
            Ok(c) => format!("{}: {}", self.auth_kind(), c.describe()),
            Err(e) => format!("{} ({e})", self.auth_kind()),
        }
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.data.join("raw").join(&self.server_name)
    }

    pub fn parquet_dir(&self) -> PathBuf {
        self.data.join("parquet")
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
    /// `--input`: replaces the file's `source.files`.
    pub input: Vec<PathBuf>,
    pub server: Option<String>,
    pub server_name: Option<String>,
    /// `--server-kind`: replaces the file's `server.kind`.
    pub server_kind: Option<ServerKind>,
    pub token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token_url: Option<String>,
    pub token_auth: Option<TokenAuth>,
    pub scope: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Given on the command line or in the environment, true or false; the
    /// file's `server.google_auth` applies only when this is unset.
    pub google_auth: Option<bool>,
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
    let data =
        std::path::absolute(&data).with_context(|| format!("resolving {}", data.display()))?;
    let views = o.views.or_else(|| file.views.map(|p| resolve(&base, &p)));
    let files = if o.input.is_empty() {
        file.source
            .files
            .iter()
            .map(|p| resolve(&base, p))
            .collect()
    } else {
        o.input
    };
    // With a file source no server is contacted, so the server's auth
    // settings are neither needed nor checked.
    let auth = if !files.is_empty() {
        AuthArgs::none()
    } else {
        let username = o.username.or(file.server.username);
        let google_auth = o.google_auth.or(file.server.google_auth).unwrap_or(false);
        let kinds: Vec<&str> = [
            (o.token.is_some(), "a token (FHIR_PIPES_TOKEN)"),
            (o.client_id.is_some(), "a client id (FHIR_PIPES_CLIENT_ID)"),
            (username.is_some(), "a username (FHIR_PIPES_USERNAME)"),
            (
                google_auth,
                "Google credentials (FHIR_PIPES_GOOGLE_AUTH or server.google_auth)",
            ),
        ]
        .into_iter()
        .filter_map(|(set, what)| set.then_some(what))
        .collect();
        if kinds.len() > 1 {
            bail!("use one way to authenticate, not {}", kinds.join(" and "));
        }
        let none = AuthArgs::none();
        match (o.client_id, o.client_secret, username, o.password) {
            _ if google_auth => AuthArgs {
                google_auth: true,
                ..none
            },
            (Some(id), Some(secret), _, _) => AuthArgs {
                client_id: Some(id),
                client_secret: Some(secret),
                token_url: Some(o.token_url.or(file.server.token_url).context(
                    "FHIR_PIPES_CLIENT_ID is set but no token URL: pass --token-url or set server.token_url",
                )?),
                token_auth: o.token_auth.or(file.server.token_auth),
                scope: o.scope.or(file.server.scope),
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
        }
    };
    let threads = o.threads.unwrap_or_else(crate::work::default_threads);
    let countries = if o.countries.is_empty() {
        file.tiles.countries
    } else {
        o.countries
    };
    Ok(Settings {
        data,
        views,
        server_name: o.server_name.or(file.server.name).unwrap_or_else(|| {
            if files.is_empty() {
                "hapi-local".into()
            } else {
                "files".into()
            }
        }),
        files,
        server: google::server_url(
            &o.server
                .or(file.server.base)
                .unwrap_or_else(|| "http://localhost:3447/fhir".into()),
        ),
        auth,
        server_kind: o.server_kind.or(file.server.kind).unwrap_or_default(),
        fetch: file.server.fetch.unwrap_or_default(),
        bulk_timeout: file
            .server
            .bulk_timeout
            .unwrap_or(DEFAULT_BULK_TIMEOUT_SECS),
        concurrency: match file.server.concurrency {
            Some(0) => bail!("server.concurrency must be at least 1"),
            n => n.unwrap_or(DEFAULT_EXPORT_CONCURRENCY),
        },
        duckdb: file.tools.duckdb.unwrap_or_else(|| "duckdb".into()),
        tippecanoe: file.tools.tippecanoe.unwrap_or_else(|| "tippecanoe".into()),
        rclone: file.tools.rclone.unwrap_or_else(|| "rclone".into()),
        strip_prefix: file.tables.strip_prefix,
        partition_column: file.partition.column.unwrap_or_else(|| "country".into()),
        lookups: file.partition.lookup,
        tile_countries: countries,
        push: file.push,
        catalog: file.catalog.map(|c| catalog(c, &base)).transpose()?,
        database: file.database.map(|d| Database {
            url_env: d
                .url_env
                .unwrap_or_else(|| "FHIR_PIPES_DATABASE_URL".into()),
            schema: d.schema.unwrap_or_else(|| "public".into()),
            tables: d.tables,
            mode: d.mode.unwrap_or_default(),
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

fn catalog(c: CatalogFile, base: &Path) -> Result<Catalog> {
    let spark = if c.spark {
        Some(Spark {
            location: c.spark_location.filter(|l| !l.is_empty()),
            database: c
                .spark_database
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "fhir".into()),
            extra: c.spark_extra.map(|p| resolve(base, &p)),
        })
    } else {
        if c.spark_location.is_some() || c.spark_database.is_some() || c.spark_extra.is_some() {
            bail!("[catalog] spark_location, spark_database and spark_extra need spark = true");
        }
        None
    };
    Ok(Catalog {
        base_url: c.base_url.filter(|b| !b.is_empty()),
        extra: c.extra.map(|p| resolve(base, &p)),
        duckdb_file: c.duckdb_file,
        duckdb_setup: c.duckdb_setup.map(|p| resolve(base, &p)),
        spark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths_resolve_against_the_file() {
        let file: File = toml::from_str("data = \"../data\"\nviews = \"/abs/views\"").unwrap();
        let s = settings(file, PathBuf::from("/repo/tools"), Overrides::default()).unwrap();
        // `data` is made absolute: as is on Unix; on Windows `/repo` gets the
        // current drive and `..` is resolved, as std::path::absolute does.
        if cfg!(windows) {
            assert!(s.data.is_absolute(), "{}", s.data.display());
            assert!(s.data.ends_with(r"repo\data"), "{}", s.data.display());
        } else {
            assert_eq!(s.data, PathBuf::from("/repo/tools/../data"));
        }
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
    fn token_auth_and_scope_come_from_the_file_or_the_flags() {
        let text = "data = \"d\"\n[server]\ntoken_url = \"http://t\"\ntoken_auth = \"basic\"\nscope = \"system/*.read\"";
        let credentials = || Overrides {
            client_id: Some("id".into()),
            client_secret: Some("s".into()),
            ..Default::default()
        };
        let s = settings(
            toml::from_str(text).unwrap(),
            PathBuf::from("/b"),
            credentials(),
        )
        .unwrap();
        assert_eq!(s.auth.token_auth, Some(TokenAuth::Basic));
        assert_eq!(s.auth.scope.as_deref(), Some("system/*.read"));
        let o = Overrides {
            token_auth: Some(TokenAuth::Post),
            scope: Some("system/Patient.read".into()),
            ..credentials()
        };
        let s = settings(toml::from_str(text).unwrap(), PathBuf::from("/b"), o).unwrap();
        assert_eq!(s.auth.token_auth, Some(TokenAuth::Post));
        assert_eq!(s.auth.scope.as_deref(), Some("system/Patient.read"));
        assert!(toml::from_str::<File>("[server]\ntoken_auth = \"header\"").is_err());
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
        assert_eq!(search.concurrency, DEFAULT_EXPORT_CONCURRENCY);
        assert_eq!(s("data = \"d\"\n[server]\nconcurrency = 2").concurrency, 2);
        let zero: File = toml::from_str("data = \"d\"\n[server]\nconcurrency = 0").unwrap();
        assert!(settings(zero, PathBuf::from("/b"), Overrides::default()).is_err());
        let db = s("data = \"d\"\n[server]\nfetch = \"hapi-db\"");
        assert_eq!(db.fetch, Fetch::HapiDb);
        assert_eq!(db.fetch.name(), "hapi-db");
    }

    #[test]
    fn the_server_kind_is_generic_unless_the_file_names_one() {
        let s = |text: &str| {
            let file: File = toml::from_str(text).unwrap();
            settings(file, PathBuf::from("/b"), Overrides::default()).unwrap()
        };
        assert_eq!(s("data = \"d\"").server_kind, ServerKind::Generic);
        assert_eq!(
            s("data = \"d\"\n[server]\nkind = \"medplum\"").server_kind,
            ServerKind::Medplum
        );
        assert_eq!(
            s("data = \"d\"\n[server]\nkind = \"aidbox\"").server_kind,
            ServerKind::Aidbox
        );
        assert!(toml::from_str::<File>("[server]\nkind = \"fhirbase\"").is_err());
        // --server-kind (or FHIR_PIPES_SERVER_KIND) wins over the file.
        let file: File = toml::from_str("data = \"d\"\n[server]\nkind = \"medplum\"").unwrap();
        let o = Overrides {
            server_kind: Some(ServerKind::Aidbox),
            ..Default::default()
        };
        assert_eq!(
            settings(file, PathBuf::from("/b"), o).unwrap().server_kind,
            ServerKind::Aidbox
        );
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
    fn spark_settings_default_and_need_spark() {
        let s = |text: &str| {
            let file: File = toml::from_str(text).unwrap();
            settings(file, PathBuf::from("/b"), Overrides::default())
        };
        let c = s("data = \"d\"\n[catalog]\n").unwrap().catalog.unwrap();
        assert!(c.spark.is_none());
        let spark = s("data = \"d\"\n[catalog]\nspark = true\n")
            .unwrap()
            .catalog
            .unwrap()
            .spark
            .unwrap();
        assert_eq!(spark.location, None);
        assert_eq!(spark.database, "fhir");
        assert_eq!(spark.extra, None);
        let spark = s("data = \"d\"\n[catalog]\nspark = true\nspark_location = \"s3a://b/w\"\nspark_database = \"analytics\"\nspark_extra = \"x.sql\"\n")
            .unwrap()
            .catalog
            .unwrap()
            .spark
            .unwrap();
        assert_eq!(spark.location.as_deref(), Some("s3a://b/w"));
        assert_eq!(spark.database, "analytics");
        assert_eq!(spark.extra, Some(PathBuf::from("/b/x.sql")));
        let err = s("data = \"d\"\n[catalog]\nspark_location = \"s3a://b/w\"\n").unwrap_err();
        assert!(err.to_string().contains("need spark = true"), "{err}");
    }

    #[test]
    fn google_auth_comes_from_the_file_or_the_flag_and_excludes_the_others() {
        let file: File = toml::from_str("data = \"d\"\n[server]\ngoogle_auth = true").unwrap();
        let s = settings(file, PathBuf::from("/b"), Overrides::default()).unwrap();
        assert!(s.auth.google_auth);
        assert_eq!(s.auth_kind(), "Google application default credentials");

        let file: File = toml::from_str("data = \"d\"").unwrap();
        let o = Overrides {
            google_auth: Some(true),
            ..Default::default()
        };
        assert!(
            settings(file, PathBuf::from("/b"), o)
                .unwrap()
                .auth
                .google_auth
        );

        let file: File = toml::from_str("data = \"d\"\n[server]\ngoogle_auth = true").unwrap();
        let o = Overrides {
            token: Some("t".into()),
            ..Default::default()
        };
        let err = settings(file, PathBuf::from("/b"), o).unwrap_err();
        assert!(
            err.to_string().contains("FHIR_PIPES_TOKEN") && err.to_string().contains("Google"),
            "{err}"
        );
    }

    #[test]
    fn an_explicit_false_turns_off_the_files_google_auth() {
        let file: File = toml::from_str("data = \"d\"\n[server]\ngoogle_auth = true").unwrap();
        let o = Overrides {
            google_auth: Some(false),
            token: Some("t".into()),
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert!(!s.auth.google_auth);
        assert_eq!(s.auth_kind(), "bearer token");
    }

    #[test]
    fn a_store_path_as_the_server_is_expanded() {
        let file: File = toml::from_str(
            "data = \"d\"\n[server]\nbase = \"projects/p/locations/l/datasets/d/fhirStores/s\"",
        )
        .unwrap();
        let s = settings(file, PathBuf::from("/b"), Overrides::default()).unwrap();
        assert_eq!(
            s.server,
            "https://healthcare.googleapis.com/v1/projects/p/locations/l/datasets/d/fhirStores/s/fhir"
        );
    }

    #[test]
    fn a_file_source_comes_from_the_file_or_the_flag() {
        let file: File =
            toml::from_str("data = \"d\"\n[source]\nfiles = [\"../fhir\", \"/abs/*.ndjson\"]")
                .unwrap();
        let s = settings(file, PathBuf::from("/repo/tools"), Overrides::default()).unwrap();
        assert!(s.reads_files());
        assert_eq!(
            s.files,
            [
                PathBuf::from("/repo/tools/../fhir"),
                PathBuf::from("/abs/*.ndjson")
            ]
        );
        assert_eq!(s.server_name, "files");

        // --input replaces the file's list and is taken as given.
        let file: File =
            toml::from_str("data = \"d\"\n[source]\nfiles = [\"x\"]\n[server]\nname = \"synthea\"")
                .unwrap();
        let o = Overrides {
            input: vec![PathBuf::from("in")],
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert_eq!(s.files, [PathBuf::from("in")]);
        assert_eq!(s.server_name, "synthea");

        let file: File = toml::from_str("data = \"d\"").unwrap();
        let s = settings(file, PathBuf::from("/b"), Overrides::default()).unwrap();
        assert!(!s.reads_files());
        assert_eq!(s.server_name, "hapi-local");
    }

    #[test]
    fn a_file_source_leaves_the_server_auth_unchecked() {
        let text = "data = \"d\"\n[source]\nfiles = [\"x\"]\n[server]\nusername = \"u\"\ngoogle_auth = true";
        let file: File = toml::from_str(text).unwrap();
        let o = Overrides {
            client_id: Some("id".into()),
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert_eq!(s.auth_kind(), "none");
    }

    #[test]
    fn a_relative_data_flag_is_made_absolute() {
        let file: File = toml::from_str("").unwrap();
        let o = Overrides {
            data: Some(PathBuf::from("../data")),
            ..Default::default()
        };
        let s = settings(file, PathBuf::from("/b"), o).unwrap();
        assert!(s.data.is_absolute(), "{}", s.data.display());
        // On Windows std::path::absolute resolves `..`; on Unix it keeps it.
        let cwd = std::env::current_dir().unwrap();
        let expected = if cfg!(windows) {
            cwd.parent().unwrap().join("data")
        } else {
            cwd.join("../data")
        };
        assert_eq!(s.data, expected);
    }

    #[test]
    fn a_hooks_after_is_a_step_before_the_hooks() {
        let hook = |after: &str| {
            toml::from_str::<File>(&format!(
                "[[hooks]]\nname = \"h\"\nafter = \"{after}\"\nrun = [\"true\"]"
            ))
            .map(|f| f.hooks[0].after)
        };
        assert_eq!(hook("views").unwrap(), Some(Step::Views));
        assert_eq!(hook("Views").unwrap(), Some(Step::Views));
        assert_eq!(hook("database").unwrap(), Some(Step::Database));
        for bad in ["push", "hooks", "vews"] {
            let err = hook(bad).unwrap_err().to_string();
            assert!(
                err.contains("a hook runs after one of locations, views"),
                "{err}"
            );
        }
        let none: File = toml::from_str("[[hooks]]\nname = \"h\"\nrun = [\"true\"]").unwrap();
        assert_eq!(none.hooks[0].after, None);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<File>("dta = \"typo\"").is_err());
    }
}
