//! `fhir-pipes libraries`: SQL on FHIR Libraries (SQLView and SQLQuery) over
//! the Parquet warehouse. This module is the Library model and the `build`
//! command, which turns `.sql` files with a `.toml` metadata file into Library
//! JSON; `source` does the building.

// The model is used by the commands added after `build`.
#![allow(dead_code)]

pub mod builtin;
pub mod resolve;
pub mod source;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

pub const SQLVIEW_PROFILE: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";
pub const SQLQUERY_PROFILE: &str =
    "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";
pub const SQL_TEXT_EXTENSION: &str =
    "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/sql-text";

#[derive(clap::Args, Debug)]
pub struct LibrariesArgs {
    #[command(subcommand)]
    command: LibrariesCommand,
}

#[derive(clap::Subcommand, Debug)]
enum LibrariesCommand {
    /// Build Library JSON from .sql files: each `<name>.sql` with a
    /// `<name>.toml` beside it becomes `Library-<name>.json` in --out.
    /// `<name>.duckdb.sql` and `<name>.postgresql.sql` add dialect content.
    Build {
        /// A directory of .sql and .toml files.
        src: PathBuf,
        /// Directory to write the Library files into.
        #[arg(long)]
        out: PathBuf,
    },
}

pub fn run(args: &LibrariesArgs) -> Result<()> {
    match &args.command {
        LibrariesCommand::Build { src, out } => {
            let n = source::build_dir(src, out)?;
            println!("wrote {n} Library file(s) to {}", out.display());
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SqlView,
    SqlQuery,
}

/// One `relatedArtifact` of type depends-on: the table name the SQL uses and
/// the Library it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub label: String,
    /// The canonical URL, without any `|version`.
    pub canonical: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Library {
    pub url: String,
    pub version: Option<String>,
    pub name: String,
    pub kind: Kind,
    pub depends_on: Vec<Dependency>,
    /// (contentType, SQL) for each content entry.
    pub content: Vec<(String, String)>,
    pub json: Value,
}

impl Library {
    pub fn from_json(v: &Value) -> Result<Library> {
        if v["resourceType"] != "Library" {
            bail!("not a Library resource");
        }
        let profiles = v["meta"]["profile"].as_array().map(Vec::as_slice);
        let has = |p: &str| profiles.is_some_and(|ps| ps.iter().any(|x| x == p));
        let kind = if has(SQLVIEW_PROFILE) {
            Kind::SqlView
        } else if has(SQLQUERY_PROFILE) {
            Kind::SqlQuery
        } else {
            bail!("Library has neither the SQLView nor the SQLQuery profile in meta.profile");
        };
        let text = |key: &str| v[key].as_str().map(str::to_string);
        let url = text("url").ok_or_else(|| anyhow!("Library has no url"))?;
        let name = text("name").ok_or_else(|| anyhow!("Library {url} has no name"))?;

        let mut depends_on = Vec::new();
        for ra in v["relatedArtifact"].as_array().into_iter().flatten() {
            if ra["type"] != "depends-on" {
                continue;
            }
            let label = ra["label"].as_str().ok_or_else(|| {
                anyhow!("Library {url}: a depends-on relatedArtifact has no label")
            })?;
            let resource = ra["resource"].as_str().ok_or_else(|| {
                anyhow!("Library {url}: depends-on {label} has no resource canonical")
            })?;
            let (canonical, version) = split_canonical(resource);
            depends_on.push(Dependency {
                label: label.to_string(),
                canonical,
                version,
            });
        }

        let mut content = Vec::new();
        for c in v["content"].as_array().into_iter().flatten() {
            let content_type = c["contentType"]
                .as_str()
                .ok_or_else(|| anyhow!("Library {url}: a content entry has no contentType"))?;
            let data = c["data"]
                .as_str()
                .ok_or_else(|| anyhow!("Library {url}: content {content_type} has no data"))?;
            let bytes = STANDARD
                .decode(data)
                .with_context(|| format!("Library {url}: content {content_type} is not base64"))?;
            let sql = String::from_utf8(bytes)
                .with_context(|| format!("Library {url}: content {content_type} is not UTF-8"))?;
            content.push((content_type.to_string(), sql));
        }

        Ok(Library {
            url,
            version: text("version"),
            name,
            kind,
            depends_on,
            content,
            json: v.clone(),
        })
    }

    /// The SQL for `dialect`: the content typed `application/sql;dialect=<dialect>`,
    /// else the portable `application/sql`.
    pub fn sql(&self, dialect: &str) -> Option<&str> {
        let find = |ct: &str| {
            self.content
                .iter()
                .find(|(t, _)| t == ct)
                .map(|(_, s)| s.as_str())
        };
        find(&format!("application/sql;dialect={dialect}")).or_else(|| find("application/sql"))
    }

    /// The warehouse table name for this Library.
    pub fn table(&self, strip_prefix: &[String]) -> String {
        crate::views::run::table_name(&self.name, strip_prefix)
    }
}

/// Split `url|version` into the URL and the version.
fn split_canonical(resource: &str) -> (String, Option<String>) {
    match resource.split_once('|') {
        Some((url, version)) => (url.to_string(), Some(version.to_string())),
        None => (resource.to_string(), None),
    }
}
