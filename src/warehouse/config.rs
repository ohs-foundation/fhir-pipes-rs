//! Settings: the config file, overridden by command-line flags and their
//! environment variables, with defaults for anything neither sets.
//!
//! Relative paths in the config file are resolved against the file's own
//! directory, so one file can be run from anywhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The config file (`fhir-warehouse.toml`). Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// The warehouse directory (raw/, parquet/, views/, tiles/, manifest.json).
    pub data: Option<PathBuf>,
    /// Where the ViewDefinitions are: a directory of JSON files, such as an IG
    /// build's fsh-generated/resources.
    pub views: Option<PathBuf>,
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
    #[serde(default)]
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    pub base: Option<String>,
    pub name: Option<String>,
    pub token_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsFile {
    pub kiln: Option<String>,
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
    pub auth: Option<Auth>,
    pub kiln: String,
    pub duckdb: String,
    pub tippecanoe: String,
    pub rclone: String,
    pub strip_prefix: Vec<String>,
    pub partition_column: String,
    pub lookups: Vec<Lookup>,
    pub tile_countries: Vec<String>,
    pub push: Option<PushFile>,
    pub hooks: Vec<Hook>,
    /// The config file's directory, for hook programs given as paths.
    pub base_dir: PathBuf,
    pub threads: usize,
    pub reuse_raw: bool,
    pub full: bool,
    pub dry_run: bool,
}

/// Client credentials for a server behind an OAuth gateway, passed on to kiln.
#[derive(Debug, Clone)]
pub struct Auth {
    pub client_id: String,
    pub client_secret: String,
    pub token_url: String,
}

impl Settings {
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
        None => {
            let default = PathBuf::from("fhir-warehouse.toml");
            default.exists().then_some(default)
        }
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
    pub data: Option<PathBuf>,
    pub views: Option<PathBuf>,
    pub server: Option<String>,
    pub server_name: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token_url: Option<String>,
    pub kiln: Option<String>,
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
            bail!("no warehouse directory: pass --data, set DATA, or set `data` in the config")
        }
    };
    let views = o.views.or_else(|| file.views.map(|p| resolve(&base, &p)));
    let auth = match (o.client_id, o.client_secret) {
        (Some(id), Some(secret)) => Some(Auth {
            client_id: id,
            client_secret: secret,
            token_url: o.token_url.or(file.server.token_url).context(
                "FHIR_CLIENT_ID is set but no token URL: pass --token-url or set server.token_url",
            )?,
        }),
        (Some(_), None) => bail!("set FHIR_CLIENT_SECRET with FHIR_CLIENT_ID"),
        _ => None,
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
        kiln: o.kiln.or(file.tools.kiln).unwrap_or_else(|| "kiln".into()),
        duckdb: file.tools.duckdb.unwrap_or_else(|| "duckdb".into()),
        tippecanoe: file.tools.tippecanoe.unwrap_or_else(|| "tippecanoe".into()),
        rclone: file.tools.rclone.unwrap_or_else(|| "rclone".into()),
        strip_prefix: file.tables.strip_prefix,
        partition_column: file.partition.column.unwrap_or_else(|| "country".into()),
        lookups: file.partition.lookup,
        tile_countries: countries,
        push: file.push,
        hooks: file.hooks,
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
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<File>("dta = \"typo\"").is_err());
    }
}
