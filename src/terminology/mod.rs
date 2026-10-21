//! `fhir-pipes terminology`: a value set or concept map from FHIR packages as
//! a table SQL can join to, in the shape SQL on FHIR's Terminology in SQL page
//! defines (<https://build.fhir.org/ig/HL7/sql-on-fhir/en/terminology-in-sql.html>):
//!
//!   valueset    system, version, code, display, inactive: one row per member
//!   conceptmap  source and target system, version, code and display, and
//!               the relationship: one row per mapping
//!
//! The content comes from the packages alone; no terminology server is
//! called. A value set is taken from its `expansion` when the package has
//! one, or else expanded from its `compose`: codes listed by the value set,
//! every code of a code system the packages hold in full, `is-a` and
//! `descendent-of` filters over such a code system's hierarchy, and other
//! value sets it includes. Anything else (a code system the packages do not
//! hold, another filter) fails the command, as the page requires, rather than
//! give a table with members missing.

pub mod expand;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use fhir_pipes_sof::{ColumnInfo, ColumnType, ViewResult};
use serde_json::Value;

use crate::error::usage;
use crate::fhir::package::Index;

#[derive(clap::Subcommand, Debug)]
pub enum TerminologyCommand {
    /// The members of a value set: system, version, code, display, inactive
    #[command(name = "valueset")]
    ValueSet(TableArgs),
    /// The mappings of a concept map: source_system, source_version,
    /// source_code, source_display, target_system, target_version,
    /// target_code, target_display, relationship
    #[command(name = "conceptmap")]
    ConceptMap(TableArgs),
}

#[derive(clap::Args, Debug)]
pub struct TableArgs {
    /// The canonical URL, with `|version` to pick one version when the
    /// packages hold several.
    pub url: String,
    /// A FHIR package to read: a package.tgz (path or https URL), an unpacked
    /// package or directory of JSON resources, or name#version in the local
    /// FHIR package cache (~/.fhir/packages). Repeat it for the packages the
    /// content depends on, such as hl7.fhir.r4.core#4.0.1.
    #[arg(long = "package", value_name = "PACKAGE", required = true)]
    pub packages: Vec<String>,
    /// Output format: csv, ndjson, json or parquet.
    #[arg(long, default_value = "csv")]
    pub output: String,
    /// Write to this file instead of stdout. A file ending in `.gz` is
    /// written as gzip, except for Parquet.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(command: &TerminologyCommand) -> Result<()> {
    let (args, table) = match command {
        TerminologyCommand::ValueSet(args) => {
            let content = Content::load(&args.packages)?;
            (
                args,
                value_set_table(&content.value_set_members(&args.url)?),
            )
        }
        TerminologyCommand::ConceptMap(args) => {
            let content = Content::load(&args.packages)?;
            (
                args,
                concept_map_table(&content.concept_map_rows(&args.url)?),
            )
        }
    };
    if args.output.eq_ignore_ascii_case("parquet")
        && args
            .out
            .as_ref()
            .is_some_and(|o| crate::fhir::ndjson::is_gz(o))
    {
        return Err(usage(anyhow!(
            "--out ends in .gz, but Parquet is not written as gzip; name the file .parquet"
        )));
    }
    let writer = crate::views::select_writer(&args.output, "text")?;
    let mut sink = crate::views::open_sink(args.out.as_ref())?;
    writer
        .write(&table, &mut sink)
        .context("writing the table")?;
    crate::views::close_sink(sink, args.out.as_ref())?;
    eprintln!("{} rows", table.data.len());
    Ok(())
}

/// The ValueSets, CodeSystems and ConceptMaps of a set of packages, by
/// canonical URL.
#[derive(Debug)]
pub struct Content(Index);

/// The resource types `Content` keeps.
const TYPES: [&str; 3] = ["ValueSet", "CodeSystem", "ConceptMap"];

impl Default for Content {
    fn default() -> Self {
        Content(Index::new(&TYPES))
    }
}

impl Content {
    pub fn load(packages: &[String]) -> Result<Self> {
        Ok(Content(Index::load(packages, "--package", &TYPES)?))
    }

    /// Keep `resource` if it is a ValueSet, CodeSystem or ConceptMap with a
    /// URL. The same URL and version from two packages is kept once.
    #[cfg(test)]
    pub fn add(&mut self, resource: Value) {
        self.0.add("", resource);
    }

    /// The value set `canonical` (`url` or `url|version`) names.
    fn value_set(&self, canonical: &str) -> Result<&Value> {
        resolve(&self.0, canonical, "ValueSet")
    }

    /// The code system `url` at `version`, if the packages hold it; an error
    /// if they hold several versions and `version` does not pick one.
    fn code_system(&self, url: &str, version: Option<&str>) -> Result<Option<&Value>> {
        let versions = self.0.versions("CodeSystem", url);
        if versions.is_empty() {
            return Ok(None);
        }
        match version {
            Some(v) => Ok(versions.iter().find(|cs| cs["version"].as_str() == Some(v))),
            None if versions.len() == 1 => Ok(versions.first()),
            None => Err(usage(anyhow!(
                "the packages hold CodeSystem {url} in versions {}; the value set does \
                 not say which to use",
                list_versions(versions)
            ))),
        }
    }

    /// The members of the value set `canonical` names.
    pub fn value_set_members(&self, canonical: &str) -> Result<Vec<expand::Member>> {
        expand::members(self, canonical, &mut Vec::new())
    }

    /// The rows of the concept map `canonical` names.
    pub fn concept_map_rows(&self, canonical: &str) -> Result<Vec<MapRow>> {
        let cm = resolve(&self.0, canonical, "ConceptMap")?;
        concept_map(cm, canonical)
    }
}

/// The resource of `kind` that `canonical` (`url` or `url|version`) names.
fn resolve<'a>(index: &'a Index, canonical: &str, kind: &str) -> Result<&'a Value> {
    let (url, version) = split_canonical(canonical);
    let versions = index.versions(kind, url);
    if versions.is_empty() {
        return Err(usage(anyhow!("no {kind} {url} in the packages")));
    }
    match version {
        Some(v) => versions
            .iter()
            .find(|r| r["version"].as_str() == Some(v))
            .ok_or_else(|| {
                usage(anyhow!(
                    "the packages hold {kind} {url} in versions {}, not {v}",
                    list_versions(versions)
                ))
            }),
        None if versions.len() == 1 => Ok(&versions[0]),
        None => Err(usage(anyhow!(
            "the packages hold {kind} {url} in versions {}; name one as {url}|<version>",
            list_versions(versions)
        ))),
    }
}

fn list_versions(versions: &[Value]) -> String {
    versions
        .iter()
        .map(|r| r["version"].as_str().unwrap_or("(none)"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `url|version` as the URL and the version.
fn split_canonical(canonical: &str) -> (&str, Option<&str>) {
    match canonical.split_once('|') {
        Some((url, version)) if !version.is_empty() => (url, Some(version)),
        Some((url, _)) => (url, None),
        None => (canonical, None),
    }
}

fn value_set_table(members: &[expand::Member]) -> ViewResult {
    let columns = [
        ("system", ColumnType::String),
        ("version", ColumnType::String),
        ("code", ColumnType::String),
        ("display", ColumnType::String),
        ("inactive", ColumnType::Boolean),
    ];
    ViewResult {
        columns: columns
            .into_iter()
            .map(|(name, ty)| ColumnInfo::new(name, ty))
            .collect(),
        data: members
            .iter()
            .map(|m| {
                vec![
                    Value::from(m.system.clone()),
                    opt(&m.version),
                    Value::from(m.code.clone()),
                    opt(&m.display),
                    m.inactive.map(Value::from).unwrap_or(Value::Null),
                ]
            })
            .collect(),
    }
}

fn opt(s: &Option<String>) -> Value {
    s.clone().map(Value::from).unwrap_or(Value::Null)
}

/// One row of a concept map's table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MapRow {
    pub source_system: String,
    pub source_version: Option<String>,
    pub source_code: String,
    pub source_display: Option<String>,
    pub target_system: Option<String>,
    pub target_version: Option<String>,
    pub target_code: Option<String>,
    pub target_display: Option<String>,
    pub relationship: Option<String>,
}

fn concept_map_table(rows: &[MapRow]) -> ViewResult {
    let names = [
        "source_system",
        "source_version",
        "source_code",
        "source_display",
        "target_system",
        "target_version",
        "target_code",
        "target_display",
        "relationship",
    ];
    ViewResult {
        columns: names
            .into_iter()
            .map(|name| ColumnInfo::new(name, ColumnType::String))
            .collect(),
        data: rows
            .iter()
            .map(|r| {
                vec![
                    Value::from(r.source_system.clone()),
                    opt(&r.source_version),
                    Value::from(r.source_code.clone()),
                    opt(&r.source_display),
                    opt(&r.target_system),
                    opt(&r.target_version),
                    opt(&r.target_code),
                    opt(&r.target_display),
                    opt(&r.relationship),
                ]
            })
            .collect(),
    }
}

/// An R4 `equivalence` as the R5 `relationship` it became.
fn relationship_from_equivalence(equivalence: &str) -> Option<&'static str> {
    Some(match equivalence {
        "relatedto" | "inexact" => "related-to",
        "equivalent" | "equal" => "equivalent",
        // The target is wider than, or subsumes, the source.
        "wider" | "subsumes" => "source-is-narrower-than-target",
        // The target is narrower than, or specializes, the source.
        "narrower" | "specializes" => "source-is-broader-than-target",
        "disjoint" => "not-related-to",
        _ => return None,
    })
}

/// The rows of a ConceptMap, R4 or R5: one per `group.element.target`, and
/// one per element with no target (`noMap` in R5, `unmatched` in R4).
/// `group.unmapped` gives no rows. A group without a source, a mapping to or
/// from a value set, and a target with `dependsOn` or `product` cannot be
/// written as rows, and fail.
fn concept_map(cm: &Value, canonical: &str) -> Result<Vec<MapRow>> {
    let fail = |why: String| usage(anyhow!("ConceptMap {canonical}: {why}"));
    let mut rows = std::collections::BTreeSet::new();
    for (g, group) in array(&cm["group"]).enumerate() {
        let (source_system, mut source_version) = canonical_parts(&group["source"])
            .ok_or_else(|| fail(format!("group {g} has no source")))?;
        let (target_system, mut target_version) = match canonical_parts(&group["target"]) {
            Some((s, v)) => (Some(s), v),
            None => (None, None),
        };
        // R4 keeps the versions in their own elements.
        if let Some(v) = group["sourceVersion"].as_str() {
            source_version = Some(v.to_string());
        }
        if let Some(v) = group["targetVersion"].as_str() {
            target_version = Some(v.to_string());
        }
        for element in array(&group["element"]) {
            if element.get("valueSet").is_some() {
                return Err(fail(format!("group {g} maps from a value set")));
            }
            let source_code = element["code"]
                .as_str()
                .ok_or_else(|| fail(format!("group {g} has an element with no code")))?
                .to_string();
            let source_display = element["display"].as_str().map(str::to_string);
            let row = |code: Option<String>, display: Option<String>, rel: Option<String>| MapRow {
                source_system: source_system.clone(),
                source_version: source_version.clone(),
                source_code: source_code.clone(),
                source_display: source_display.clone(),
                target_system: target_system.clone(),
                target_version: target_version.clone(),
                target_code: code,
                target_display: display,
                relationship: rel,
            };
            if element["noMap"] == Value::Bool(true) {
                rows.insert(row(None, None, None));
            }
            for target in array(&element["target"]) {
                if target.get("valueSet").is_some() {
                    return Err(fail(format!("{source_code} maps to a value set")));
                }
                if target.get("dependsOn").is_some() || target.get("product").is_some() {
                    return Err(fail(format!(
                        "{source_code} has a mapping with dependsOn or product, which a \
                         row cannot hold"
                    )));
                }
                let relationship = match (
                    target["relationship"].as_str(),
                    target["equivalence"].as_str(),
                ) {
                    (Some(r), _) => Some(r.to_string()),
                    (None, Some("unmatched")) => {
                        rows.insert(row(None, None, None));
                        continue;
                    }
                    (None, Some(e)) => relationship_from_equivalence(e).map(str::to_string),
                    (None, None) => None,
                };
                rows.insert(row(
                    target["code"].as_str().map(str::to_string),
                    target["display"].as_str().map(str::to_string),
                    relationship,
                ));
            }
        }
    }
    Ok(rows.into_iter().collect())
}

/// A canonical element's URL and `|version`.
fn canonical_parts(v: &Value) -> Option<(String, Option<String>)> {
    let (url, version) = split_canonical(v.as_str()?);
    Some((url.to_string(), version.map(str::to_string)))
}

fn array(v: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// Members by (system, version, code), in that order, for the table.
pub(crate) type Members = BTreeMap<(String, Option<String>, String), expand::Member>;

#[cfg(test)]
mod tests;
