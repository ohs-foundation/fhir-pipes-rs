//! `fhir-pipes import location-sites`: rows of points (CSV: facilities,
//! schools, settlements) as site Locations hung under the admin units
//! `import location-boundaries` wrote.
//!
//! Each row's admin unit names (`--parent`, top-down) are matched level by
//! level against the units' names and aliases, as slugs, so `Tama/Daye`
//! matches `Tama Daye`. A row whose names stop matching part way links to
//! the deepest unit that matched and is reported as `parent_unresolved`: a
//! facility under its LGA is more use than one under nothing.
//!
//! Fatal (nothing written, exit 2): a column the options name that the CSV
//! lacks, and two rows with one id. Reported (that row only): no name or no
//! valid id (row skipped), unusable coordinates (written without a
//! position), names that do not resolve.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use super::{LocationSitesArgs, Shape, Site, TypeConcept, slugify, split_pair};
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::ndjson::NdjsonReader;
use crate::fhir::ndjson::NdjsonWriter;
use crate::fhir::spatial::{compute_cell, extension_json, parse_scheme_level};
use crate::profile::Profile;
use crate::report::Report;

/// One `--parent LEVEL=COLUMN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentSpec {
    pub level: String,
    pub column: String,
}

pub fn parse_parent(arg: &str) -> Result<ParentSpec> {
    let (level, column) = split_pair(arg, "--parent", "LEVEL=COLUMN (e.g. ward=ward)")?;
    Ok(ParentSpec {
        level: level.into(),
        column: column.into(),
    })
}

/// `SYSTEM_URI=COLUMN`, split at the last `=` (a URI may hold one).
pub fn parse_identifier(arg: &str, flag: &str) -> Result<(String, String)> {
    match arg.rsplit_once('=') {
        Some((system, column)) if !system.is_empty() && !column.is_empty() => {
            Ok((system.into(), column.into()))
        }
        _ => Err(PipesError::Usage(format!(
            "{flag} must be SYSTEM_URI=COLUMN, got {arg:?}"
        ))),
    }
}

/// One `--type-coding` / `--org-type-coding SYSTEM_URI=CODE_COLUMN[:TEXT_COLUMN]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeCodingSpec {
    pub system: String,
    pub code_column: String,
    pub text_column: Option<String>,
}

pub fn parse_type_coding(arg: &str, flag: &str) -> Result<TypeCodingSpec> {
    let usage = || {
        PipesError::Usage(format!(
            "{flag} must be SYSTEM_URI=CODE_COLUMN[:TEXT_COLUMN], got {arg:?}"
        ))
    };
    let (system, columns) = arg.rsplit_once('=').ok_or_else(usage)?;
    if system.is_empty() || columns.is_empty() {
        return Err(usage());
    }
    let (code, text) = columns.split_once(':').unwrap_or((columns, ""));
    if code.is_empty() {
        return Err(PipesError::Usage(format!(
            "{flag} {arg:?}: empty code column"
        )));
    }
    Ok(TypeCodingSpec {
        system: system.into(),
        code_column: code.into(),
        text_column: (!text.is_empty()).then(|| text.to_string()),
    })
}

/// `--where COLUMN=VALUE`: an exact match on the raw value.
pub fn parse_where(arg: &str) -> Result<(String, String)> {
    let (column, value) = split_pair(arg, "--where", "COLUMN=VALUE (e.g. state=Bauchi)")?;
    Ok((column.into(), value.into()))
}

/// Parent id → { slug of a child's name or alias → child id }; units whose
/// parent is not in the set (the roots) under "". On two children of one
/// parent with one slug, the first keeps it.
pub type AdminIndex = HashMap<String, HashMap<String, String>>;

pub fn build_admin_index(units: &[Unit]) -> AdminIndex {
    let ids: HashSet<&str> = units.iter().filter_map(|r| r.id.as_deref()).collect();
    let mut index = AdminIndex::new();
    for unit in units {
        let (Some(id), Some(name)) = (
            unit.id.as_deref().filter(|s| !s.is_empty()),
            unit.name.as_deref().filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        let reference = unit.part_of.as_deref().unwrap_or_default();
        let parent = reference.rsplit('/').next().unwrap_or_default();
        let key = if ids.contains(parent) { parent } else { "" };
        let bucket = index.entry(key.to_string()).or_default();
        let labels = std::iter::once(name).chain(unit.aliases.iter().map(String::as_str));
        for label in labels {
            let slug = slugify(label);
            if !slug.is_empty() {
                bucket.entry(slug).or_insert_with(|| id.to_string());
            }
        }
    }
    index
}

/// Where the first --parent level is matched: below the root when there is
/// exactly one (the country, which rows do not name), else among the roots.
fn walk_start(index: &AdminIndex) -> String {
    let roots: HashSet<&String> = index.get("").into_iter().flat_map(|b| b.values()).collect();
    match roots.into_iter().collect::<Vec<_>>()[..] {
        [only] => only.clone(),
        _ => String::new(),
    }
}

/// A row's cells by column name; "" for a column the row lacks.
pub trait Row {
    fn cell(&self, column: &str) -> &str;
}

/// Each header name and the positions it is at, in order.
pub fn header_index(header: &[String]) -> HashMap<String, Vec<usize>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, name) in header.iter().enumerate() {
        index.entry(name.clone()).or_default().push(i);
    }
    index
}

/// One CSV record read against its header. A name the header repeats reads
/// the last of its positions that the record has, and a record shorter than
/// the header lacks the columns past its end: as a map built by zipping the
/// header with the record gave.
pub struct CsvRow<'a> {
    pub columns: &'a HashMap<String, Vec<usize>>,
    pub record: csv::StringRecord,
}

impl Row for CsvRow<'_> {
    fn cell(&self, column: &str) -> &str {
        self.columns
            .get(column)
            .and_then(|at| at.iter().rev().find_map(|&i| self.record.get(i)))
            .unwrap_or("")
    }
}

fn cell<'a>(row: &'a impl Row, column: &str) -> &'a str {
    row.cell(column)
}

/// The id of the deepest unit the row's names resolve to; reports
/// `parent_unresolved` when they stop short. None when nothing resolves.
pub fn resolve_parent(
    index: &AdminIndex,
    parents: &[ParentSpec],
    row: &impl Row,
    label: &str,
    report: &mut Report,
) -> Option<String> {
    let mut current = walk_start(index);
    let mut resolved: Option<String> = None;
    for spec in parents {
        let raw = cell(row, &spec.column);
        let name = slugify(raw);
        let child = (!name.is_empty())
            .then(|| index.get(&current).and_then(|b| b.get(&name)))
            .flatten();
        let Some(child) = child else {
            let deepest = match &resolved {
                None => "nothing".to_string(),
                Some(_) => format!("only {current:?}"),
            };
            report.add(
                "parent_unresolved",
                label,
                &format!(
                    "{} {raw:?} not found in the admin registry; resolved {deepest}",
                    spec.level
                ),
            );
            return resolved;
        };
        resolved = Some(child.clone());
        current = child.clone();
    }
    resolved
}

pub struct Options<'a> {
    pub type_code: String,
    pub name_col: String,
    pub lat_col: String,
    pub lon_col: String,
    pub id_col: String,
    pub parents: Vec<ParentSpec>,
    pub identifiers: Vec<(String, String)>,
    pub wheres: Vec<(String, String)>,
    /// Some(id prefix) to pair each Location with a facility Organization.
    pub paired_org: Option<String>,
    pub org_identifiers: Vec<(String, String)>,
    pub org_type_codings: Vec<TypeCodingSpec>,
    pub type_codings: Vec<TypeCodingSpec>,
    /// (scheme, level) pairs, and the extension they go in.
    pub spatial_indexes: Vec<(String, u32)>,
    pub spatial_index_extension: Option<String>,
    pub shape: &'a Shape,
}

impl Options<'_> {
    /// Every column the options name, with the option naming it.
    fn columns(&self) -> Vec<(&str, &str)> {
        let mut out = vec![
            ("--name-col", self.name_col.as_str()),
            ("--lat-col", self.lat_col.as_str()),
            ("--lon-col", self.lon_col.as_str()),
            ("--id-col", self.id_col.as_str()),
        ];
        out.extend(self.parents.iter().map(|p| ("--parent", p.column.as_str())));
        out.extend(
            self.identifiers
                .iter()
                .map(|(_, c)| ("--identifier", c.as_str())),
        );
        out.extend(self.wheres.iter().map(|(c, _)| ("--where", c.as_str())));
        out.extend(
            self.org_identifiers
                .iter()
                .map(|(_, c)| ("--org-identifier", c.as_str())),
        );
        for (flag, specs) in [
            ("--org-type-coding", &self.org_type_codings),
            ("--type-coding", &self.type_codings),
        ] {
            for s in specs {
                out.push((flag, &s.code_column));
                out.extend(s.text_column.as_deref().map(|t| (flag, t)));
            }
        }
        out
    }
}

/// Refuses a CSV header that lacks a column the options name: every row
/// would come out wrong the same way.
pub fn check_columns(header: &[String], opts: &Options) -> Result<()> {
    let missing: Vec<String> = opts
        .columns()
        .into_iter()
        .filter(|(_, c)| !header.iter().any(|h| h == c))
        .map(|(flag, c)| format!("{c:?} ({flag})"))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(PipesError::Usage(format!(
        "the CSV has no column {}; its columns are: {}",
        missing.join(", "),
        header.join(", ")
    )))
}

/// (system, value) pairs from the row; empty cells are left out.
fn row_identifiers(row: &impl Row, specs: &[(String, String)]) -> Vec<(String, String)> {
    specs
        .iter()
        .filter_map(|(system, column)| {
            let value = cell(row, column).trim();
            (!value.is_empty()).then(|| (system.clone(), value.to_string()))
        })
        .collect()
}

/// The cell as a slug is the code (`Primary` → `primary`, the IG's code
/// style), the cell itself the display, the text column the concept text.
fn type_concepts(row: &impl Row, specs: &[TypeCodingSpec]) -> Vec<TypeConcept> {
    specs
        .iter()
        .filter_map(|s| {
            let raw = cell(row, &s.code_column).trim();
            let code = slugify(raw);
            if code.is_empty() {
                return None;
            }
            let text = s
                .text_column
                .as_deref()
                .map(|t| cell(row, t).trim())
                .filter(|t| !t.is_empty());
            Some(TypeConcept {
                system: s.system.clone(),
                code,
                display: raw.to_string(),
                text: text.map(str::to_string),
            })
        })
        .collect()
}

/// (longitude, latitude), or None (reported) when not numbers in range.
fn read_position(
    row: &impl Row,
    opts: &Options,
    label: &str,
    report: &mut Report,
) -> Option<(f64, f64)> {
    let (lat_raw, lon_raw) = (cell(row, &opts.lat_col), cell(row, &opts.lon_col));
    let (Ok(lat), Ok(lon)) = (lat_raw.trim().parse::<f64>(), lon_raw.trim().parse::<f64>()) else {
        report.add(
            "missing_position",
            label,
            &format!("coordinates not numeric: ({lat_raw:?}, {lon_raw:?})"),
        );
        return None;
    };
    if !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)) {
        report.add(
            "missing_position",
            label,
            &format!("coordinates out of range: ({lat}, {lon})"),
        );
        return None;
    }
    Some((lon, lat))
}

/// The resources for `rows` (each paired Organization before its Location),
/// handed to `emit` as each row is built.
pub fn build<R: Row>(
    rows: impl IntoIterator<Item = Result<R>>,
    units: &[Unit],
    opts: &Options,
    report: &mut Report,
    mut emit: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    let index = build_admin_index(units);
    let mut seen: HashSet<String> = HashSet::new();
    for (i, row) in rows.into_iter().enumerate() {
        let row = &row?;
        let number = i + 1;
        if opts.wheres.iter().any(|(c, v)| cell(row, c) != v) {
            continue;
        }
        let id = cell(row, &opts.id_col).trim();
        let name = cell(row, &opts.name_col).trim();
        let label = if id.is_empty() {
            format!("row {number}")
        } else {
            id.to_string()
        };
        if name.is_empty() {
            report.add(
                "missing_field",
                &label,
                &format!("{:?} is empty; row skipped", opts.name_col),
            );
            continue;
        }
        let org_id = opts
            .paired_org
            .as_ref()
            .map(|prefix| format!("{prefix}{id}"));
        if !is_valid_id(id) || org_id.as_deref().is_some_and(|o| !is_valid_id(o)) {
            report.add(
                "invalid_id",
                &format!("row {number}"),
                &format!(
                    "{:?} value {id:?} is not a valid FHIR id; row skipped",
                    opts.id_col
                ),
            );
            continue;
        }
        if !seen.insert(id.to_string()) {
            return Err(PipesError::Usage(format!(
                "duplicate id: {:?} value {id:?} appears more than once",
                opts.id_col
            )));
        }
        if let Some(org_id) = &org_id {
            emit(opts.shape.facility_organization(
                org_id,
                name,
                &row_identifiers(row, &opts.org_identifiers),
                &type_concepts(row, &opts.org_type_codings),
            ))?;
        }
        let position = read_position(row, opts, &label, report);
        let mut extensions = Vec::new();
        if let (Some((lon, lat)), Some(url)) = (position, &opts.spatial_index_extension) {
            for (scheme, level) in &opts.spatial_indexes {
                if let Some(c) = compute_cell(scheme, *level, lon, lat) {
                    extensions.push(extension_json(url, scheme, *level, &c));
                }
            }
        }
        let parent = resolve_parent(&index, &opts.parents, row, &label, report);
        emit(opts.shape.site(&Site {
            id,
            name,
            type_code: &opts.type_code,
            parent: parent.as_deref(),
            identifiers: row_identifiers(row, &opts.identifiers),
            position,
            managing_organization: org_id.as_deref(),
            extra_types: type_concepts(row, &opts.type_codings),
            extensions,
        }))?;
    }
    Ok(())
}

/// What `build_admin_index` reads of an admin unit: `id`, `name`,
/// `partOf.reference` and the string aliases. Everything else, the boundary
/// above all, is skipped over rather than built. A field of another JSON
/// type counts as absent, and a repeated field keeps its last value, as a
/// `Value` read gave.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Unit {
    pub id: Option<String>,
    pub name: Option<String>,
    pub part_of: Option<String>,
    pub aliases: Vec<String>,
}

/// One `--boundaries` line: a unit, or a Bundle whose entries' resources
/// are the units.
#[derive(Default)]
struct UnitLine {
    unit: Unit,
    resource_type: Option<String>,
    entries: Vec<Unit>,
}

fn skip_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> std::result::Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn skip_map<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

fn string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Reads `key`'s value into `unit` if it is a unit field; false otherwise
/// (the value is then not consumed).
fn unit_field<'de, A: MapAccess<'de>>(
    unit: &mut Unit,
    key: &str,
    map: &mut A,
) -> std::result::Result<bool, A::Error> {
    match key {
        "id" => unit.id = string(&map.next_value::<Value>()?),
        "name" => unit.name = string(&map.next_value::<Value>()?),
        "partOf" => unit.part_of = map.next_value::<Value>()?.get("reference").and_then(string),
        "alias" => {
            unit.aliases = map
                .next_value::<Value>()?
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(string)
                .collect()
        }
        _ => return Ok(false),
    }
    Ok(true)
}

impl<'de> Deserialize<'de> for Unit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unit;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            other_json_types_are_default!();
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> std::result::Result<Unit, A::Error> {
                skip_seq(seq).map(|()| Unit::default())
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Unit, A::Error> {
                let mut unit = Unit::default();
                while let Some(key) = map.next_key::<String>()? {
                    if !unit_field(&mut unit, &key, &mut map)? {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(unit)
            }
        }
        d.deserialize_any(V)
    }
}

/// A Bundle entry's `resource`, if it has one.
struct Entry(Option<Unit>);

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Unit>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            other_json_types_are_default!();
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                skip_seq(seq).map(|()| None)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut resource = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "resource" {
                        resource = Some(map.next_value::<Unit>()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(resource)
            }
        }
        d.deserialize_any(V).map(Entry)
    }
}

/// A Bundle's `entry`: the resources of the entries that have one.
struct Entries(Vec<Unit>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<Unit>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            other_json_types_are_default!();
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut units = Vec::new();
                while let Some(Entry(resource)) = seq.next_element::<Entry>()? {
                    units.extend(resource);
                }
                Ok(units)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                skip_map(map).map(|()| Vec::new())
            }
        }
        d.deserialize_any(V).map(Entries)
    }
}

impl<'de> Deserialize<'de> for UnitLine {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UnitLine;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            other_json_types_are_default!();
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                seq: A,
            ) -> std::result::Result<UnitLine, A::Error> {
                skip_seq(seq).map(|()| UnitLine::default())
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<UnitLine, A::Error> {
                let mut line = UnitLine::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "resourceType" => line.resource_type = string(&map.next_value::<Value>()?),
                        "entry" => line.entries = map.next_value::<Entries>()?.0,
                        _ => {
                            if !unit_field(&mut line.unit, &key, &mut map)? {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                }
                Ok(line)
            }
        }
        d.deserialize_any(V)
    }
}

/// The admin units: each line a resource, or a Bundle whose entries are.
/// A line that does not parse is reported and skipped.
fn read_units(path: &Path, report: &mut Report) -> Result<Vec<Unit>> {
    let mut units = Vec::new();
    let mut unparsed = 0usize;
    for line in NdjsonReader::open(path)? {
        let line = line?;
        match serde_json::from_str::<UnitLine>(&line.text) {
            Ok(v) if v.resource_type.as_deref() == Some("Bundle") => units.extend(v.entries),
            Ok(v) => units.push(v.unit),
            Err(e) => {
                unparsed += 1;
                report.add(
                    "malformed_field",
                    &format!("line {}", line.number),
                    &format!("not JSON: {e}"),
                );
            }
        }
    }
    if units.is_empty() && unparsed > 0 {
        return Err(PipesError::Usage(format!(
            "--boundaries {}: no line is a JSON resource; is it NDJSON?",
            path.display()
        )));
    }
    Ok(units)
}

/// The CSV reader, past its header, and the header.
fn open_csv(path: &Path) -> Result<(csv::Reader<std::fs::File>, Vec<String>)> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(|e| csv_error(path, e))?;
    let header: Vec<String> = reader
        .headers()
        .map_err(|e| csv_error(path, e))?
        .iter()
        .map(str::to_string)
        .collect();
    Ok((reader, header))
}

fn csv_error(path: &Path, e: csv::Error) -> PipesError {
    PipesError::Usage(format!("--in {}: {e}", path.display()))
}

pub fn run(args: &LocationSitesArgs, profile: &Profile) -> Result<()> {
    crate::fs::check_out(&args.out)?;
    let shape = Shape::new(profile, "location-sites")?;
    let spatial_indexes: Vec<(String, u32)> = args
        .spatial_indexes
        .iter()
        .map(|a| parse_scheme_level(a))
        .collect::<Result<_>>()?;
    let spatial_index_extension = if spatial_indexes.is_empty() {
        None
    } else {
        Some(profile.spatial_index_extension()?.to_string())
    };
    let opts = Options {
        type_code: args.type_code.clone(),
        name_col: args.name_col.clone(),
        lat_col: args.lat_col.clone(),
        lon_col: args.lon_col.clone(),
        id_col: args.id_col.clone(),
        parents: args
            .parents
            .iter()
            .map(|p| parse_parent(p))
            .collect::<Result<_>>()?,
        identifiers: args
            .identifiers
            .iter()
            .map(|a| parse_identifier(a, "--identifier"))
            .collect::<Result<_>>()?,
        wheres: args
            .wheres
            .iter()
            .map(|w| parse_where(w))
            .collect::<Result<_>>()?,
        paired_org: args
            .paired_org
            .then(|| profile.organization.id_prefix.clone()),
        org_identifiers: args
            .org_identifiers
            .iter()
            .map(|a| parse_identifier(a, "--org-identifier"))
            .collect::<Result<_>>()?,
        org_type_codings: args
            .org_type_codings
            .iter()
            .map(|a| parse_type_coding(a, "--org-type-coding"))
            .collect::<Result<_>>()?,
        type_codings: args
            .type_codings
            .iter()
            .map(|a| parse_type_coding(a, "--type-coding"))
            .collect::<Result<_>>()?,
        spatial_indexes,
        spatial_index_extension,
        shape: &shape,
    };
    for path in [&args.input, &args.boundaries] {
        if !path.is_file() {
            return Err(PipesError::Usage(format!(
                "input file not found: {}",
                path.display()
            )));
        }
    }
    let (reader, header) = open_csv(&args.input)?;
    check_columns(&header, &opts)?;
    let columns = header_index(&header);
    let mut report = Report::default();
    let units = read_units(&args.boundaries, &mut report)?;
    // Each resource is written as its row is built. The file is renamed
    // into place only at the end, so a fatal row (a repeated id) still
    // leaves nothing written.
    let rows = reader.into_records().map(|record| {
        record
            .map(|record| CsvRow {
                columns: &columns,
                record,
            })
            .map_err(|e| csv_error(&args.input, e))
    });
    let mut out = NdjsonWriter::create(&args.out)?;
    let mut organizations = 0usize;
    build(rows, &units, &opts, &mut report, |resource| {
        if resource["resourceType"] == "Organization" {
            organizations += 1;
        }
        out.write(&resource)
    })?;
    let n = out.finish()?;
    println!(
        "Wrote {n} resources to {} ({} Locations, {organizations} Organizations)",
        args.out.display(),
        n - organizations
    );
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        crate::report::write_report(path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fhir::spatial::quadkey;
    use crate::import::icr_shape;
    use serde_json::json;

    const NHFR: &str = "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code";
    const GRID3: &str = "https://icr.healthcampaigns.org/identifiers/grid3-globalid";
    const FACILITY_TYPE_CS: &str =
        "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs";
    const OWNERSHIP_CS: &str = "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs";
    const SPATIAL_INDEX: &str = "https://icr.healthcampaigns.org/StructureDefinition/spatial-index";

    /// country → state → lga → two wards, as location-boundaries writes them.
    fn registry() -> Vec<Unit> {
        registry_values()
            .iter()
            .map(|v| serde_json::from_str(&v.to_string()).unwrap())
            .collect()
    }

    fn registry_values() -> Vec<Value> {
        let shape = icr_shape();
        let ids = [(
            "https://icr.healthcampaigns.org/identifiers/national-admin-code".to_string(),
            "x".to_string(),
        )];
        vec![
            shape.admin_unit("nga", "Nigeria", None, &ids, &[], None),
            shape.admin_unit("nga-ba", "Bauchi", Some("nga"), &ids, &[], None),
            shape.admin_unit("nga-ba-toro", "Toro", Some("nga-ba"), &ids, &[], None),
            shape.admin_unit(
                "nga-ba-toro-tilde",
                "Tilde",
                Some("nga-ba-toro"),
                &ids,
                &["Tilden Fulani".to_string()],
                None,
            ),
            shape.admin_unit(
                "nga-ba-toro-toro",
                "Toro",
                Some("nga-ba-toro"),
                &ids,
                &[],
                None,
            ),
        ]
    }

    type MapRow = HashMap<String, String>;

    impl Row for MapRow {
        fn cell(&self, column: &str) -> &str {
            self.get(column).map_or("", String::as_str)
        }
    }

    fn row(overrides: &[(&str, &str)]) -> MapRow {
        let mut row: MapRow = [
            ("state", "Bauchi"),
            ("lga", "Toro"),
            ("ward", "Tilde"),
            ("facility_name", "Tilde Primary Health Care Center"),
            ("latitude", "10.5"),
            ("longitude", "9.9"),
            ("globalid", "9c2b58dd-1e99-49b9-970e-eabce1c7dd10"),
            ("nhfr_code", "08/07/1/1/1/0030"),
            ("level", "Primary"),
            ("level_detail", "Primary Health Center"),
            ("ownership", "Public"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in overrides {
            row.insert(k.to_string(), v.to_string());
        }
        row
    }

    fn options(shape: &Shape) -> Options<'_> {
        Options {
            type_code: "facility".into(),
            name_col: "facility_name".into(),
            lat_col: "latitude".into(),
            lon_col: "longitude".into(),
            id_col: "globalid".into(),
            parents: ["state=state", "lga=lga", "ward=ward"]
                .iter()
                .map(|p| parse_parent(p).unwrap())
                .collect(),
            identifiers: vec![
                (NHFR.into(), "nhfr_code".into()),
                (GRID3.into(), "globalid".into()),
            ],
            wheres: Vec::new(),
            paired_org: None,
            org_identifiers: Vec::new(),
            org_type_codings: Vec::new(),
            type_codings: Vec::new(),
            spatial_indexes: Vec::new(),
            spatial_index_extension: None,
            shape,
        }
    }

    fn paired(shape: &Shape) -> Options<'_> {
        let mut opts = options(shape);
        opts.identifiers = vec![(GRID3.into(), "globalid".into())];
        opts.paired_org = Some("org-".into());
        opts.org_identifiers = vec![(NHFR.into(), "nhfr_code".into())];
        opts.org_type_codings = vec![
            parse_type_coding(&format!("{FACILITY_TYPE_CS}=level:level_detail"), "t").unwrap(),
            parse_type_coding(&format!("{OWNERSHIP_CS}=ownership"), "t").unwrap(),
        ];
        opts
    }

    fn build_all(rows: &[MapRow], opts: &Options, report: &mut Report) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        build(
            rows.iter().cloned().map(Ok),
            &registry(),
            opts,
            report,
            |r| {
                out.push(r);
                Ok(())
            },
        )?;
        Ok(out)
    }

    fn run_build(rows: &[MapRow], opts: &Options, report: &mut Report) -> Vec<Value> {
        build_all(rows, opts, report).unwrap()
    }

    #[test]
    fn parses_parent_identifier_where_and_type_coding_flags() {
        assert_eq!(
            parse_parent("state=state_name").unwrap(),
            ParentSpec {
                level: "state".into(),
                column: "state_name".into()
            }
        );
        assert!(parse_parent("state").is_err());
        assert_eq!(
            parse_identifier(&format!("{NHFR}=nhfr_code"), "--identifier").unwrap(),
            (NHFR.to_string(), "nhfr_code".to_string())
        );
        assert_eq!(
            parse_identifier("urn:x?a=b=col", "--identifier").unwrap(),
            ("urn:x?a=b".to_string(), "col".to_string())
        );
        assert!(parse_identifier("no-equals", "--identifier").is_err());
        assert_eq!(
            parse_where("state=Bauchi").unwrap(),
            ("state".to_string(), "Bauchi".to_string())
        );
        assert!(parse_where("state").is_err());
        let t = parse_type_coding("https://x/cs=level:detail", "t").unwrap();
        assert_eq!(
            (t.code_column.as_str(), t.text_column.as_deref()),
            ("level", Some("detail"))
        );
        assert_eq!(
            parse_type_coding("https://x/cs=level", "t")
                .unwrap()
                .text_column,
            None
        );
        assert!(parse_type_coding("https://x/cs=:detail", "t").is_err());
    }

    #[test]
    fn the_admin_index_matches_names_and_aliases_as_slugs() {
        let index = build_admin_index(&registry());
        assert_eq!(index[""]["nigeria"], "nga");
        assert_eq!(index["nga"]["bauchi"], "nga-ba");
        assert_eq!(index["nga-ba-toro"]["tilde"], "nga-ba-toro-tilde");
        assert_eq!(index["nga-ba-toro"]["tilden-fulani"], "nga-ba-toro-tilde");
    }

    #[test]
    fn builds_a_profiled_facility() {
        let shape = icr_shape();
        let mut report = Report::default();
        let out = run_build(&[row(&[])], &options(&shape), &mut report);
        let [r] = &out[..] else { panic!("{out:?}") };
        assert_eq!(r["id"], "9c2b58dd-1e99-49b9-970e-eabce1c7dd10");
        assert_eq!(r["name"], "Tilde Primary Health Care Center");
        assert_eq!(r["status"], "active");
        assert_eq!(
            r["meta"]["profile"][0],
            "https://icr.healthcampaigns.org/StructureDefinition/ICRLocation"
        );
        assert_eq!(r["type"][0]["coding"][0]["code"], "facility");
        assert_eq!(r["physicalType"]["coding"][0]["code"], "si");
        assert_eq!(
            r["partOf"],
            json!({"reference": "Location/nga-ba-toro-tilde"})
        );
        assert_eq!(r["position"], json!({"longitude": 9.9, "latitude": 10.5}));
        assert_eq!(
            r["identifier"],
            json!([
                {"system": NHFR, "value": "08/07/1/1/1/0030"},
                {"system": GRID3, "value": "9c2b58dd-1e99-49b9-970e-eabce1c7dd10"}
            ])
        );
        assert!(r.get("managingOrganization").is_none());
        assert!(report.counts().is_empty());
    }

    #[test]
    fn an_unresolved_name_falls_back_to_the_deepest_match() {
        let shape = icr_shape();
        let mut report = Report::default();
        let out = run_build(
            &[row(&[("ward", "Nowhere")])],
            &options(&shape),
            &mut report,
        );
        assert_eq!(
            out[0]["partOf"],
            json!({"reference": "Location/nga-ba-toro"})
        );
        assert_eq!(report.count("parent_unresolved"), 1);

        let mut report = Report::default();
        let out = run_build(
            &[row(&[("state", "Elsewhere")])],
            &options(&shape),
            &mut report,
        );
        assert!(out[0].get("partOf").is_none());
        assert_eq!(report.count("parent_unresolved"), 1);
    }

    #[test]
    fn aliases_and_slugs_resolve() {
        let shape = icr_shape();
        let mut report = Report::default();
        let out = run_build(
            &[row(&[("ward", "TILDEN  fulani"), ("state", "bauchi")])],
            &options(&shape),
            &mut report,
        );
        assert_eq!(out[0]["partOf"]["reference"], "Location/nga-ba-toro-tilde");
        assert!(report.counts().is_empty());
    }

    #[test]
    fn spatial_index_cells_go_on_positioned_rows() {
        let shape = icr_shape();
        let mut opts = options(&shape);
        opts.identifiers = vec![(GRID3.into(), "globalid".into())];
        opts.spatial_indexes = vec![("quadkey".into(), 18), ("quadkey".into(), 10)];
        opts.spatial_index_extension = Some(SPATIAL_INDEX.into());
        let mut report = Report::default();
        let out = run_build(
            &[
                row(&[]),
                row(&[
                    ("globalid", "no-position"),
                    ("latitude", ""),
                    ("longitude", ""),
                ]),
            ],
            &opts,
            &mut report,
        );
        let cells: Vec<(String, u64, String)> = out[0]["extension"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["url"] == SPATIAL_INDEX)
            .map(|e| {
                (
                    e["extension"][0]["valueCode"].as_str().unwrap().to_string(),
                    e["extension"][1]["valueUnsignedInt"].as_u64().unwrap(),
                    e["extension"][2]["valueString"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            cells,
            [
                ("quadkey".to_string(), 18, quadkey(9.9, 10.5, 18)),
                ("quadkey".to_string(), 10, quadkey(9.9, 10.5, 10))
            ]
        );
        assert!(cells[0].2.starts_with(&cells[1].2));
        assert!(out[1].get("extension").is_none());
        assert_eq!(report.count("missing_position"), 1);
    }

    #[test]
    fn where_filters_rows() {
        let shape = icr_shape();
        let mut opts = options(&shape);
        opts.wheres = vec![("state".into(), "Bauchi".into())];
        let out = run_build(
            &[
                row(&[]),
                row(&[("state", "Kano"), ("globalid", "other-id")]),
            ],
            &opts,
            &mut Report::default(),
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn bad_coordinates_leave_the_position_out() {
        let shape = icr_shape();
        for lat in ["", "95.0", "north"] {
            let mut report = Report::default();
            let out = run_build(&[row(&[("latitude", lat)])], &options(&shape), &mut report);
            assert!(out[0].get("position").is_none(), "{lat}");
            assert_eq!(report.count("missing_position"), 1);
        }
    }

    #[test]
    fn rows_without_a_name_or_a_usable_id_are_skipped() {
        let shape = icr_shape();
        let mut report = Report::default();
        let out = run_build(
            &[
                row(&[("facility_name", "")]),
                row(&[("globalid", "bad id!")]),
            ],
            &options(&shape),
            &mut report,
        );
        assert!(out.is_empty());
        assert_eq!(report.count("missing_field"), 1);
        assert_eq!(report.count("invalid_id"), 1);
    }

    #[test]
    fn an_empty_identifier_cell_is_left_out() {
        let shape = icr_shape();
        let out = run_build(
            &[row(&[("nhfr_code", "")])],
            &options(&shape),
            &mut Report::default(),
        );
        assert_eq!(
            out[0]["identifier"],
            json!([{"system": GRID3, "value": "9c2b58dd-1e99-49b9-970e-eabce1c7dd10"}])
        );
    }

    #[test]
    fn duplicate_ids_are_fatal() {
        let shape = icr_shape();
        let err = build_all(
            &[row(&[]), row(&[])],
            &options(&shape),
            &mut Report::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn a_paired_organization_comes_first_and_holds_the_registry_codes() {
        let shape = icr_shape();
        let out = run_build(&[row(&[])], &paired(&shape), &mut Report::default());
        let [org, location] = &out[..] else {
            panic!("{out:?}")
        };
        assert_eq!(org["resourceType"], "Organization");
        assert_eq!(org["id"], "org-9c2b58dd-1e99-49b9-970e-eabce1c7dd10");
        assert_eq!(
            org["meta"]["profile"][0],
            "https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization"
        );
        assert_eq!(org["name"], "Tilde Primary Health Care Center");
        assert_eq!(org["active"], true);
        assert_eq!(org["type"][0]["coding"][0]["code"], "prov");
        assert_eq!(
            org["type"][1],
            json!({"coding": [{"system": FACILITY_TYPE_CS, "code": "primary", "display": "Primary"}],
                   "text": "Primary Health Center"})
        );
        assert_eq!(org["type"][2]["coding"][0]["code"], "public");
        assert_eq!(
            org["identifier"],
            json!([{"system": NHFR, "value": "08/07/1/1/1/0030"}])
        );
        assert_eq!(
            location["managingOrganization"],
            json!({"reference": "Organization/org-9c2b58dd-1e99-49b9-970e-eabce1c7dd10"})
        );
        assert_eq!(
            location["identifier"],
            json!([{"system": GRID3, "value": "9c2b58dd-1e99-49b9-970e-eabce1c7dd10"}])
        );
    }

    #[test]
    fn empty_type_cells_add_no_type() {
        let shape = icr_shape();
        let out = run_build(
            &[row(&[("level", ""), ("ownership", "Unknown")])],
            &paired(&shape),
            &mut Report::default(),
        );
        let codes: Vec<&str> = out[0]["type"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["coding"][0]["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, ["prov", "unknown"]);
    }

    #[test]
    fn type_codings_copy_the_classification_onto_the_location() {
        let shape = icr_shape();
        let mut opts = paired(&shape);
        opts.type_codings = opts.org_type_codings.clone();
        let out = run_build(&[row(&[])], &opts, &mut Report::default());
        let location = &out[1];
        assert_eq!(location["type"][0]["coding"][0]["code"], "facility");
        assert_eq!(
            location["type"][1],
            json!({"coding": [{"system": FACILITY_TYPE_CS, "code": "primary", "display": "Primary"}],
                   "text": "Primary Health Center"})
        );
        assert_eq!(location["type"][2]["coding"][0]["code"], "public");
    }

    #[test]
    fn a_column_the_csv_lacks_is_fatal() {
        let shape = icr_shape();
        let opts = options(&shape);
        let mut header: Vec<String> = row(&[]).into_keys().collect();
        assert!(check_columns(&header, &opts).is_ok());
        header.retain(|h| h != "ward");
        let err = check_columns(&header, &opts).unwrap_err().to_string();
        assert!(err.contains("\"ward\" (--parent)"), "{err}");
    }

    #[test]
    fn a_csv_row_reads_as_the_header_zipped_into_a_map_did() {
        // A repeated name, a short record, a long one, an empty cell.
        let text = "a,b,a,c\n1,2,3,4\n5,6\n7,8,9,10,11\n,x,,\n";
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(text.as_bytes());
        let header: Vec<String> = reader
            .headers()
            .unwrap()
            .iter()
            .map(str::to_string)
            .collect();
        let columns = header_index(&header);
        for record in reader.records() {
            let record = record.unwrap();
            let map: MapRow = header
                .iter()
                .cloned()
                .zip(record.iter().map(str::to_string))
                .collect();
            let row = CsvRow {
                columns: &columns,
                record: record.clone(),
            };
            for name in ["a", "b", "c", "missing"] {
                assert_eq!(row.cell(name), map.cell(name), "{name} in {record:?}");
            }
        }
    }

    #[test]
    fn units_are_read_without_the_rest_of_the_resource() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("units.ndjson");
        let mut lines: Vec<String> = registry_values().iter().map(Value::to_string).collect();
        lines.extend([
            // Another JSON type counts as absent; the last of a repeated
            // field wins; only string aliases count.
            r#"{"id":"u1","name":5,"partOf":["x"],"alias":["A",1,null,"B"]}"#.to_string(),
            r#"{"id":"u2","name":"First","name":"Second","partOf":{"reference":"Location/nga"},"alias":"x"}"#.to_string(),
            // A Bundle: entries with a resource (any value) are units.
            r#"{"entry":[{"resource":{"id":"b1","name":"B1"}},{"fullUrl":"x"},{"resource":null},7],"resourceType":"Bundle"}"#.to_string(),
            r#"{"resourceType":"Bundle","entry":{"resource":{"id":"b2"}}}"#.to_string(),
            "[1,2]".to_string(),
            "not json".to_string(),
            // A unit with its boundary.
            icr_shape()
                .admin_unit(
                    "nga-x",
                    "X",
                    Some("nga"),
                    &[(
                        "https://icr.healthcampaigns.org/identifiers/national-admin-code".into(),
                        "x".into(),
                    )],
                    &[],
                    Some(br#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#),
                )
                .to_string(),
        ]);
        std::fs::write(&path, lines.join("\n")).unwrap();
        let mut report = Report::default();
        let units = read_units(&path, &mut report).unwrap();
        assert_eq!(report.count("malformed_field"), 1);
        let n = registry_values().len();
        assert_eq!(&units[..n], &registry()[..]);
        assert_eq!(
            units[n],
            Unit {
                id: Some("u1".into()),
                name: None,
                part_of: None,
                aliases: vec!["A".into(), "B".into()],
            }
        );
        assert_eq!(
            units[n + 1],
            Unit {
                id: Some("u2".into()),
                name: Some("Second".into()),
                part_of: Some("Location/nga".into()),
                aliases: Vec::new(),
            }
        );
        assert_eq!(units[n + 2].id.as_deref(), Some("b1"));
        assert_eq!(units[n + 3], Unit::default());
        // The bundle whose entry is not a list adds nothing; the array line
        // is a unit with nothing in it.
        assert_eq!(units[n + 4], Unit::default());
        assert!(lines[n + 6].contains("valueAttachment"));
        assert_eq!(
            units[n + 5],
            Unit {
                id: Some("nga-x".into()),
                name: Some("X".into()),
                part_of: Some("Location/nga".into()),
                aliases: Vec::new(),
            }
        );
        assert_eq!(units.len(), n + 6);
    }
}
