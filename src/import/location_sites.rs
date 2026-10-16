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
use std::path::Path;

use serde_json::Value;

use super::{LocationSitesArgs, Shape, Site, TypeConcept, slugify, split_pair};
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::ndjson::NdjsonReader;
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

pub fn build_admin_index(units: &[Value]) -> AdminIndex {
    let ids: HashSet<&str> = units
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .collect();
    let mut index = AdminIndex::new();
    for unit in units {
        let (Some(id), Some(name)) = (
            unit.get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
            unit.get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        let reference = unit
            .pointer("/partOf/reference")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parent = reference.rsplit('/').next().unwrap_or_default();
        let key = if ids.contains(parent) { parent } else { "" };
        let bucket = index.entry(key.to_string()).or_default();
        let aliases = unit.get("alias").and_then(Value::as_array);
        let labels =
            std::iter::once(name).chain(aliases.into_iter().flatten().filter_map(Value::as_str));
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

type Row = HashMap<String, String>;

fn cell<'a>(row: &'a Row, column: &str) -> &'a str {
    row.get(column).map_or("", String::as_str)
}

/// The id of the deepest unit the row's names resolve to; reports
/// `parent_unresolved` when they stop short. None when nothing resolves.
pub fn resolve_parent(
    index: &AdminIndex,
    parents: &[ParentSpec],
    row: &Row,
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
fn row_identifiers(row: &Row, specs: &[(String, String)]) -> Vec<(String, String)> {
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
fn type_concepts(row: &Row, specs: &[TypeCodingSpec]) -> Vec<TypeConcept> {
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
    row: &Row,
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

/// The resources for `rows` (each paired Organization before its Location).
pub fn build(
    rows: &[Row],
    units: &[Value],
    opts: &Options,
    report: &mut Report,
) -> Result<Vec<Value>> {
    let index = build_admin_index(units);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, row) in rows.iter().enumerate() {
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
            out.push(opts.shape.facility_organization(
                org_id,
                name,
                &row_identifiers(row, &opts.org_identifiers),
                &type_concepts(row, &opts.org_type_codings),
            ));
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
        out.push(opts.shape.site(&Site {
            id,
            name,
            type_code: &opts.type_code,
            parent: parent.as_deref(),
            identifiers: row_identifiers(row, &opts.identifiers),
            position,
            managing_organization: org_id.as_deref(),
            extra_types: type_concepts(row, &opts.type_codings),
            extensions,
        }));
    }
    Ok(out)
}

/// The admin units: each line a resource, or a Bundle whose entries are.
/// A line that does not parse is reported and skipped.
fn read_units(path: &Path, report: &mut Report) -> Result<Vec<Value>> {
    let mut units = Vec::new();
    let mut unparsed = 0usize;
    for line in NdjsonReader::open(path)? {
        let line = line?;
        match serde_json::from_str::<Value>(&line.text) {
            Ok(v) if v.get("resourceType").and_then(Value::as_str) == Some("Bundle") => {
                units.extend(
                    v.get("entry")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.get("resource").cloned()),
                );
            }
            Ok(v) => units.push(v),
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

/// The CSV's header and rows, each row keyed by column name.
fn read_csv(path: &Path) -> Result<(Vec<String>, Vec<Row>)> {
    let bad = |e: csv::Error| PipesError::Usage(format!("--in {}: {e}", path.display()));
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(bad)?;
    let header: Vec<String> = reader
        .headers()
        .map_err(bad)?
        .iter()
        .map(str::to_string)
        .collect();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(bad)?;
        rows.push(
            header
                .iter()
                .cloned()
                .zip(record.iter().map(str::to_string))
                .collect(),
        );
    }
    Ok((header, rows))
}

pub fn run(args: &LocationSitesArgs, profile: &Profile) -> Result<()> {
    crate::dhis2::check_out(&args.out)?;
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
    let (header, rows) = read_csv(&args.input)?;
    check_columns(&header, &opts)?;
    let mut report = Report::default();
    let units = read_units(&args.boundaries, &mut report)?;
    let resources = build(&rows, &units, &opts, &mut report)?;
    let n = crate::dhis2::write_ndjson(&args.out, &resources)?;
    let organizations = resources
        .iter()
        .filter(|r| r["resourceType"] == "Organization")
        .count();
    println!(
        "Wrote {n} resources to {} ({} Locations, {organizations} Organizations)",
        args.out.display(),
        n - organizations
    );
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        crate::transform::write_report(path, &report)?;
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
    fn registry() -> Vec<Value> {
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

    fn row(overrides: &[(&str, &str)]) -> Row {
        let mut row: Row = [
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

    fn run_build(rows: &[Row], opts: &Options, report: &mut Report) -> Vec<Value> {
        build(rows, &registry(), opts, report).unwrap()
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
        let err = build(
            &[row(&[]), row(&[])],
            &registry(),
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
}
