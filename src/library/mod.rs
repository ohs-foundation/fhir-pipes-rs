//! `fhir-pipes libraries`: SQL on FHIR Libraries (SQLView and SQLQuery) over
//! the Parquet warehouse. This module is the Library model and the `build`
//! command, which turns `.sql` files with a `.toml` metadata file into Library
//! JSON; `source` does the building.

// `Library::json`, `Library::table` and `Catalog::libraries` are used by the
// refresh step that comes later.
#![allow(dead_code)]

pub mod builtin;
pub mod compose;
pub mod resolve;
pub mod source;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use crate::error::usage;
use crate::warehouse::sql::sql_path;

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
    /// Print the DuckDB SQL that runs a Library over a warehouse, with each
    /// input read from the warehouse's Parquet, to paste into `duckdb`.
    Sql {
        /// A Library JSON file, or a canonical URL (`url` or `url|version`)
        /// to resolve from the built-ins and --sources.
        library: String,
        /// Where canonicals resolve from, after the built-ins: directories of
        /// JSON, FHIR package .tgz files (a path or https URL), or
        /// `name#version` in the FHIR package cache.
        #[arg(long, num_args = 1..)]
        sources: Vec<String>,
        /// The warehouse directory whose parquet/ holds the input tables.
        #[arg(long, default_value = ".")]
        warehouse: PathBuf,
        /// Prefixes taken off table names, as the config's
        /// `tables.strip_prefix` (e.g. icr_). Repeat for more.
        #[arg(long)]
        strip_prefix: Vec<String>,
    },
}

pub fn run(args: &LibrariesArgs) -> Result<()> {
    match &args.command {
        LibrariesCommand::Build { src, out } => {
            let n = source::build_dir(src, out)?;
            println!("wrote {n} Library file(s) to {}", out.display());
            Ok(())
        }
        LibrariesCommand::Sql {
            library,
            sources,
            warehouse,
            strip_prefix,
        } => {
            println!(
                "{};",
                library_sql(library, sources, warehouse, strip_prefix)?
            );
            Ok(())
        }
    }
}

/// The DuckDB SQL for `library` (a Library JSON file or a canonical), with
/// views, the hierarchy and SQLQuery inputs read from `warehouse`'s Parquet
/// and SQLViews inlined.
fn library_sql(
    library: &str,
    sources: &[String],
    warehouse: &Path,
    strip_prefix: &[String],
) -> Result<String> {
    let base = std::env::current_dir().context("reading the current directory")?;
    let catalog = resolve::Catalog::load(sources, &base, strip_prefix)?;
    let from_file;
    let lib = if Path::new(library).is_file() {
        let text = std::fs::read_to_string(library)
            .with_context(|| format!("reading {library}"))
            .map_err(usage)?;
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {library}"))
            .map_err(usage)?;
        from_file = Library::from_json(&v)
            .with_context(|| format!("reading {library}"))
            .map_err(usage)?;
        &from_file
    } else {
        let (url, version) = split_canonical(library);
        match catalog.get(&url, version.as_deref()).map_err(usage)? {
            resolve::Artifact::Library(l) => l,
            resolve::Artifact::View(_) => {
                return Err(usage(anyhow!(
                    "{library} is a ViewDefinition, not a Library; `views run` runs views"
                )));
            }
        }
    };
    let bind = |a: &resolve::Artifact| -> Result<Option<compose::Bind>> {
        if let resolve::Artifact::Library(l) = a
            && l.kind == Kind::SqlView
            && l.url != resolve::HIERARCHY_URL
        {
            return Ok(None);
        }
        let files = warehouse
            .join("parquet")
            .join(a.table(strip_prefix))
            .join("**/*.parquet");
        Ok(Some(compose::Bind::Relation(format!(
            "read_parquet({}, hive_partitioning=true, union_by_name=true)",
            sql_path(&files)
        ))))
    };
    compose::compose(lib, "duckdb", &catalog, &bind).map_err(usage)
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
            check_label(label).with_context(|| format!("Library {url}"))?;
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

/// A `depends-on` label must be a lowercase SQL name (`[a-z_][a-z0-9_]*`).
/// The SQL names it unquoted, and PostgreSQL folds unquoted names to
/// lowercase, so a mixed-case label would not match its quoted binding.
pub fn check_label(label: &str) -> Result<()> {
    let ok = label.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!(
            "depends-on label {label:?} must be a lowercase SQL name: a-z, 0-9 and _, \
             not starting with a digit"
        );
    }
    Ok(())
}

/// Split `url|version` into the URL and the version.
fn split_canonical(resource: &str) -> (String, Option<String>) {
    match resource.split_once('|') {
        Some((url, version)) => (url.to_string(), Some(version.to_string())),
        None => (resource.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn with_label(label: &str) -> Value {
        json!({
            "resourceType": "Library",
            "meta": {"profile": [SQLQUERY_PROFILE]},
            "url": "https://example.org/Library/Q", "name": "Q",
            "relatedArtifact": [{"type": "depends-on", "label": label,
                "resource": "https://example.org/ViewDefinition/Patient"}],
            "content": [],
        })
    }

    #[test]
    fn from_json_refuses_a_label_that_is_not_a_lowercase_sql_name() {
        let err = Library::from_json(&with_label("Patient")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Library https://example.org/Library/Q")
                && msg.contains("Patient")
                && msg.contains("lowercase"),
            "{msg}"
        );
        let lib = Library::from_json(&with_label("group")).unwrap();
        assert_eq!(lib.depends_on[0].label, "group");
    }

    #[test]
    fn libraries_sql_binds_inputs_to_warehouse_parquet() {
        let d = tempfile::tempdir().unwrap();
        let view = json!({
            "resourceType": "ViewDefinition",
            "url": "https://example.org/ViewDefinition/IcrTask", "name": "IcrTask",
            "resource": "Task", "status": "active",
            "select": [{"column": [{"name": "id", "path": "id"}]}],
        });
        fs::write(d.path().join("v.json"), view.to_string()).unwrap();
        let meta = |name: &str, kind: &str, deps: &str| {
            format!(
                "url = \"https://example.org/Library/{name}\"\nname = \"{name}\"\n\
                 status = \"draft\"\nkind = \"{kind}\"\n{deps}"
            )
        };
        let dep = |label: &str, url: &str| {
            format!("[[depends_on]]\nlabel = \"{label}\"\nresource = \"{url}\"\n")
        };
        let s = source::build_from(
            "SELECT * FROM t",
            &[],
            &meta(
                "ByArea",
                "SQLView",
                &dep("t", "https://example.org/ViewDefinition/IcrTask"),
            ),
        )
        .unwrap();
        fs::write(d.path().join("s.json"), s.to_string()).unwrap();
        let q = source::build_from(
            "SELECT * FROM s JOIN h USING (id);",
            &[],
            &meta(
                "Tasks",
                "SQLQuery",
                &(dep("s", "https://example.org/Library/ByArea")
                    + &dep("h", resolve::HIERARCHY_URL)),
            ),
        )
        .unwrap();
        let q_path = d.path().join("q.json");
        fs::write(&q_path, q.to_string()).unwrap();

        let sources = [d.path().to_str().unwrap().to_string()];
        let strip = ["icr_".to_string()];
        let read = |t: &str| {
            format!(
                "read_parquet('/wh/parquet/{t}/**/*.parquet', hive_partitioning=true, \
                 union_by_name=true)"
            )
        };
        let want = format!(
            "WITH \"s\" AS (WITH \"t\" AS (SELECT * FROM {})\nSELECT * FROM (\n\
             SELECT * FROM t\n) AS q), \"h\" AS (SELECT * FROM {})\n\
             SELECT * FROM (\nSELECT * FROM s JOIN h USING (id)\n) AS q",
            read("task"),
            read("location_hierarchy")
        );
        // From a file, and by canonical from the sources.
        for arg in [
            q_path.to_str().unwrap(),
            "https://example.org/Library/Tasks",
        ] {
            let got = library_sql(arg, &sources, Path::new("/wh"), &strip).unwrap();
            assert_eq!(got, want);
        }
        let err = library_sql(
            "https://example.org/Nope",
            &sources,
            Path::new("/wh"),
            &strip,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Nope"), "{err:#}");
    }
}
