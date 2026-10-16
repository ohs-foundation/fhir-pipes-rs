//! `fhir-pipes import location-boundaries`: one level of admin boundaries
//! (a GeoJSON FeatureCollection, such as GRID3's wards) as the admin unit
//! hierarchy, parents first.
//!
//! `--country` mints the root. Each `--level` names the feature property
//! holding that level's name (and optionally its code); the levels above the
//! last are minted once each, the first time a feature names them, and the
//! last level carries the feature's boundary. Ids are slugs of the path:
//! `nga`, `nga-ba`, `nga-ba-alkaleri`, `nga-ba-alkaleri-alkaleri-east`.
//!
//! Fatal (nothing written, exit 2): not a FeatureCollection, a CRS other than
//! WGS84, a level property empty on every feature (a mapping typo), two
//! different names with one slug under one parent, a duplicate last-level
//! unit. Reported (that feature only): a missing name, an unusable geometry
//! (the unit is written without a boundary).

use std::collections::HashMap;

use geo::orient::{Direction, Orient};
use geo::{Geometry, LineString, MultiPolygon, Polygon};
use serde_json::{Value, json};

use super::{LocationBoundariesArgs, Shape, slugify, split_pair, text_of};
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::profile::Profile;
use crate::report::Report;

/// One `--level LEVEL=NAME_PROP[:CODE_PROP]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelSpec {
    pub name: String,
    pub name_prop: String,
    pub code_prop: Option<String>,
}

/// `Nigeria=NGA` → (name, code).
pub fn parse_country(arg: &str) -> Result<(String, String)> {
    let (name, code) = split_pair(arg, "--country", "NAME=CODE (e.g. Nigeria=NGA)")?;
    Ok((name.to_string(), code.to_string()))
}

/// `state=state:statecode` or `ward=ward`.
pub fn parse_level(arg: &str) -> Result<LevelSpec> {
    let (name, props) = split_pair(
        arg,
        "--level",
        "LEVEL=NAME_PROP[:CODE_PROP] (e.g. state=state:statecode)",
    )?;
    let parts: Vec<&str> = props.split(':').collect();
    match parts[..] {
        [name_prop] => Ok(LevelSpec {
            name: name.into(),
            name_prop: name_prop.into(),
            code_prop: None,
        }),
        [name_prop, code_prop] if !name_prop.is_empty() && !code_prop.is_empty() => Ok(LevelSpec {
            name: name.into(),
            name_prop: name_prop.into(),
            code_prop: Some(code_prop.into()),
        }),
        _ => Err(PipesError::Usage(format!(
            "--level {arg:?}: expected at most one ':' separating NAME_PROP:CODE_PROP"
        ))),
    }
}

/// `lga=lga_alt_names` → level → property.
pub fn parse_aliases(args: &[String]) -> Result<HashMap<String, String>> {
    args.iter()
        .map(|arg| {
            let (level, prop) = split_pair(arg, "--alias", "LEVEL=PROPERTY")?;
            Ok((level.to_string(), prop.to_string()))
        })
        .collect()
}

/// The spellings of WGS84 seen in a GeoJSON `crs` member (RFC 7946 dropped
/// it; WGS84 is the default). Anything else is projected coordinates.
const ACCEPTED_CRS_NAMES: [&str; 5] = [
    "EPSG:4326",
    "urn:ogc:def:crs:EPSG::4326",
    "urn:ogc:def:crs:OGC:1.3:CRS84",
    "CRS84",
    "OGC:CRS84",
];

/// Refuses a collection whose `crs` is not WGS84; none at all is WGS84.
pub fn check_crs(collection: &Value) -> Result<()> {
    let Some(crs) = collection.get("crs").filter(|c| !c.is_null()) else {
        return Ok(());
    };
    let name = crs
        .pointer("/properties/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if ACCEPTED_CRS_NAMES.contains(&name) {
        return Ok(());
    }
    Err(PipesError::Usage(format!(
        "input CRS {name:?} is not WGS84 (EPSG:4326); reproject the file first, e.g.: \
         ogr2ogr -t_srs EPSG:4326 out.geojson in.geojson"
    )))
}

/// A feature's geometry as an oriented Polygon or MultiPolygon, or None
/// (reported as `geometry_invalid`) if it is not one or is not valid.
fn feature_shape(geometry: Option<&Value>, id: &str, report: &mut Report) -> Option<Geometry> {
    let kind = geometry.and_then(|g| g.get("type")).and_then(Value::as_str);
    if !matches!(kind, Some("Polygon" | "MultiPolygon")) {
        let shown = kind.map_or("None".to_string(), |k| format!("{k:?}"));
        report.add(
            "geometry_invalid",
            id,
            &format!("geometry is {shown}, expected Polygon or MultiPolygon"),
        );
        return None;
    }
    let parsed = serde_json::from_value::<geojson::Geometry>(geometry.cloned().unwrap_or_default())
        .map_err(|e| e.to_string())
        .and_then(crate::geometry::geojson::convert);
    let (shape, z_dropped) = match parsed {
        Ok(p) => p,
        Err(e) => {
            report.add(
                "geometry_invalid",
                id,
                &format!("unparseable geometry: {e}"),
            );
            return None;
        }
    };
    if z_dropped {
        report.add("boundary_z_dropped", id, "3D coordinates flattened to 2D");
    }
    let empty = match &shape {
        Geometry::Polygon(p) => p.exterior().0.is_empty(),
        Geometry::MultiPolygon(mp) => mp.0.iter().all(|p| p.exterior().0.is_empty()),
        _ => true,
    };
    if empty {
        report.add("geometry_invalid", id, "empty geometry");
        return None;
    }
    if let Err(reason) = crate::geometry::validity::check(&shape) {
        report.add("geometry_invalid", id, &reason);
        return None;
    }
    Some(orient(shape))
}

/// RFC 7946 winding: exterior rings counterclockwise, holes clockwise.
fn orient(shape: Geometry) -> Geometry {
    match shape {
        Geometry::Polygon(p) => Geometry::Polygon(p.orient(Direction::Default)),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(mp.orient(Direction::Default)),
        other => other,
    }
}

fn ring_json(ring: &LineString) -> Value {
    ring.coords().map(|c| json!([c.x, c.y])).collect()
}

fn polygon_json(p: &Polygon) -> Value {
    std::iter::once(p.exterior())
        .chain(p.interiors())
        .map(ring_json)
        .collect()
}

/// Compact GeoJSON of a Polygon or MultiPolygon.
pub fn geometry_bytes(shape: &Geometry) -> Vec<u8> {
    let value = match shape {
        Geometry::Polygon(p) => json!({"type": "Polygon", "coordinates": polygon_json(p)}),
        Geometry::MultiPolygon(mp) => json!({
            "type": "MultiPolygon",
            "coordinates": mp.0.iter().map(polygon_json).collect::<Vec<_>>(),
        }),
        _ => unreachable!("only polygons are boundaries"),
    };
    value.to_string().into_bytes()
}

/// A feature geometry checked, rewound and serialized; None if unusable.
#[cfg(test)]
pub fn normalize_geometry(
    geometry: Option<&Value>,
    id: &str,
    report: &mut Report,
) -> Option<Vec<u8>> {
    feature_shape(geometry, id, report).map(|s| geometry_bytes(&s))
}

fn polygons(shape: Geometry) -> Vec<Polygon> {
    match shape {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0,
        _ => Vec::new(),
    }
}

pub struct Options<'a> {
    pub country: (String, String),
    pub levels: Vec<LevelSpec>,
    /// Level name → the property holding its aliases.
    pub aliases: HashMap<String, String>,
    pub code_system: String,
    pub dissolve_parents: bool,
    pub shape: &'a Shape,
}

/// The units of one level, in the order features first name them.
#[derive(Default)]
struct Level {
    resources: Vec<Value>,
    index: HashMap<String, usize>,
}

/// `properties[alias_prop]` split on `;`.
fn read_aliases(properties: &serde_json::Map<String, Value>, prop: Option<&String>) -> Vec<String> {
    let Some(prop) = prop else {
        return Vec::new();
    };
    text_of(properties.get(prop))
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The Locations for `collection`: the country, then each level in order.
pub fn build(collection: &Value, opts: &Options, report: &mut Report) -> Result<Vec<Value>> {
    if collection.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
        return Err(PipesError::Usage(
            "input is not a GeoJSON FeatureCollection".into(),
        ));
    }
    check_crs(collection)?;
    let features: Vec<&Value> = collection
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|f| f.is_object())
        .collect();
    for level in &opts.levels {
        let used = features.iter().any(|f| {
            !text_of(f.get("properties").and_then(|p| p.get(&level.name_prop))).is_empty()
        });
        if !used {
            return Err(PipesError::Usage(format!(
                "--level {}: property {:?} is empty on every feature -- likely a mapping typo",
                level.name, level.name_prop
            )));
        }
    }

    let shape = opts.shape;
    let (country_name, country_code) = &opts.country;
    let root = slugify(country_code);
    if !is_valid_id(&root) {
        return Err(PipesError::Usage(format!(
            "--country code {country_code:?} does not make a FHIR id"
        )));
    }
    let mut country = shape.admin_unit(
        &root,
        country_name,
        None,
        &[(opts.code_system.clone(), country_code.clone())],
        &[],
        None,
    );
    // slug → the name that claimed it, to refuse two names on one slug.
    let mut claimed: HashMap<String, String> = HashMap::new();
    let mut levels: Vec<Level> = opts.levels.iter().map(|_| Level::default()).collect();
    let leaf = opts.levels.len() - 1;
    // With --dissolve-parents: every last-level polygon, and for each unit
    // above the last level the indexes of the polygons below it.
    let mut leaf_polygons: Vec<Polygon> = Vec::new();
    let mut below: HashMap<String, Vec<usize>> = HashMap::new();

    for feature in features {
        let Some(properties) = feature.get("properties").and_then(Value::as_object) else {
            report.add(
                "missing_field",
                "<unknown>",
                "feature has no properties object",
            );
            continue;
        };
        let names: Vec<String> = opts
            .levels
            .iter()
            .map(|l| text_of(properties.get(&l.name_prop)).trim().to_string())
            .collect();
        if let Some(i) = names.iter().position(String::is_empty) {
            report.add(
                "missing_field",
                "<unknown>",
                &format!(
                    "feature is missing {:?}; skipped: {names:?}",
                    opts.levels[i].name
                ),
            );
            continue;
        }

        let mut parent = root.clone();
        // The feature's ancestors, root first. Not recoverable from the leaf
        // slug: "-" both joins segments and appears inside them.
        let mut lineage = vec![root.clone()];
        for (index, (level, name)) in opts.levels.iter().zip(&names).enumerate() {
            let code = level
                .code_prop
                .as_ref()
                .map(|p| text_of(properties.get(p)).trim().to_string())
                .filter(|c| !c.is_empty());
            let segment = slugify(code.as_deref().unwrap_or(name));
            if segment.is_empty() {
                report.add(
                    "missing_field",
                    "<unknown>",
                    &format!("{} name slugs to nothing", level.name),
                );
                break;
            }
            let slug = format!("{parent}-{segment}");
            if !is_valid_id(&slug) {
                report.add(
                    "invalid_id",
                    &slug,
                    &format!(
                        "{} {name:?}: the id is longer than 64 characters; skipped",
                        level.name
                    ),
                );
                break;
            }
            match claimed.get(&slug) {
                Some(already) if already != name => {
                    return Err(PipesError::Usage(format!(
                        "slug collision: {} {name:?} and {already:?} both slug to {slug:?} \
                         -- disambiguate the source data",
                        level.name
                    )));
                }
                Some(_) => {}
                None => {
                    claimed.insert(slug.clone(), name.clone());
                }
            }
            let identifiers = [(
                opts.code_system.clone(),
                code.unwrap_or_else(|| slug.clone()),
            )];
            let aliases = read_aliases(properties, opts.aliases.get(&level.name));
            let units = &mut levels[index];
            if index == leaf {
                if units.index.contains_key(&slug) {
                    return Err(PipesError::Usage(format!(
                        "duplicate feature: {} {name:?} ({slug:?}) appears more than once",
                        level.name
                    )));
                }
                let geometry = feature_shape(feature.get("geometry"), &slug, report);
                let boundary = geometry.as_ref().map(geometry_bytes);
                units.index.insert(slug.clone(), units.resources.len());
                units.resources.push(shape.admin_unit(
                    &slug,
                    name,
                    Some(&parent),
                    &identifiers,
                    &aliases,
                    boundary.as_deref(),
                ));
                if opts.dissolve_parents
                    && let Some(g) = geometry
                {
                    let start = leaf_polygons.len();
                    leaf_polygons.extend(polygons(g));
                    for ancestor in &lineage {
                        below
                            .entry(ancestor.clone())
                            .or_default()
                            .extend(start..leaf_polygons.len());
                    }
                }
            } else if !units.index.contains_key(&slug) {
                units.index.insert(slug.clone(), units.resources.len());
                units.resources.push(shape.admin_unit(
                    &slug,
                    name,
                    Some(&parent),
                    &identifiers,
                    &aliases,
                    None,
                ));
            }
            lineage.push(slug.clone());
            parent = slug;
        }
    }

    if opts.dissolve_parents {
        // Each unit's boundary is the union of the last-level polygons below
        // it, not of its children's unions: unions of unions leave sliver
        // gaps where neighbours' shared edges were rounded differently.
        let mut attach = |slug: &str, resource: &mut Value| {
            let Some(indexes) = below.get(slug) else {
                return;
            };
            let parts: Vec<&Polygon> = indexes.iter().map(|&i| &leaf_polygons[i]).collect();
            dissolve(slug, &parts, shape, resource, report);
        };
        attach(&root, &mut country);
        for level in &mut levels[..leaf] {
            for resource in &mut level.resources {
                let slug = resource["id"].as_str().unwrap_or_default().to_string();
                attach(&slug, resource);
            }
        }
    }

    let mut out = vec![country];
    for level in levels {
        out.extend(level.resources);
    }
    Ok(out)
}

/// The union of `parts`, attached to `resource` as its boundary.
fn dissolve(
    slug: &str,
    parts: &[&Polygon],
    shape: &Shape,
    resource: &mut Value,
    report: &mut Report,
) {
    let union: MultiPolygon = geo::unary_union(parts.iter().copied());
    let boundary = match union.0.len() {
        0 => {
            report.add(
                "geometry_invalid",
                slug,
                "dissolved boundary is empty, expected Polygon or MultiPolygon; parent left \
                 boundary-less",
            );
            return;
        }
        1 => Geometry::Polygon(union.0[0].clone()),
        _ => Geometry::MultiPolygon(union),
    };
    shape.attach_boundary(resource, &geometry_bytes(&orient(boundary)));
}

pub fn run(args: &LocationBoundariesArgs, profile: &Profile) -> Result<()> {
    crate::dhis2::check_out(&args.out)?;
    let shape = Shape::new(profile, "location-boundaries")?;
    if shape.admin_unit_type.is_none() {
        return Err(PipesError::Usage(format!(
            "import location-boundaries needs [location] admin_unit_type in the profile, the \
             type code of an admin unit; {} has none. Pass --profile; see docs/profiles.md",
            profile.source
        )));
    }
    let code_system = match args.code_system.as_deref().or(profile.admin_code_system()) {
        Some(s) => s.to_string(),
        None => {
            return Err(PipesError::Usage(format!(
                "import location-boundaries needs the identifier system of the admin codes: \
                 --code-system, or [location] admin_code_system in the profile ({} has none)",
                profile.source
            )));
        }
    };
    let opts = Options {
        country: parse_country(&args.country)?,
        levels: args
            .levels
            .iter()
            .map(|l| parse_level(l))
            .collect::<Result<_>>()?,
        aliases: parse_aliases(&args.aliases)?,
        code_system,
        dissolve_parents: args.dissolve_parents,
        shape: &shape,
    };
    let file = std::fs::File::open(&args.input)
        .map_err(|e| PipesError::Usage(format!("--in {}: {e}", args.input.display())))?;
    let collection: Value =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(|e| {
            PipesError::Usage(format!("{} is not valid JSON: {e}", args.input.display()))
        })?;
    let mut report = Report::default();
    let resources = build(&collection, &opts, &mut report)?;
    let n = crate::dhis2::write_ndjson(&args.out, &resources)?;
    println!("Wrote {n} Locations to {}", args.out.display());
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        crate::transform::write_report(path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::icr_shape;
    use base64::Engine;
    use geo::Area;

    const CODE_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/national-admin-code";

    fn square(x: f64) -> Value {
        json!({"type": "Polygon", "coordinates": [[[x, 6.0], [x + 1.0, 6.0], [x + 1.0, 7.0], [x, 7.0], [x, 6.0]]]})
    }

    /// Wound clockwise on purpose: RFC 7946 wants counterclockwise.
    fn cw_square(x: f64) -> Value {
        json!({"type": "Polygon", "coordinates": [[[x, 6.0], [x, 7.0], [x + 1.0, 7.0], [x + 1.0, 6.0], [x, 6.0]]]})
    }

    fn levels() -> Vec<LevelSpec> {
        ["state=state:statecode", "lga=lga", "ward=ward"]
            .iter()
            .map(|l| parse_level(l).unwrap())
            .collect()
    }

    fn feature(overrides: Value) -> Value {
        let mut properties = json!({"state": "Bauchi", "statecode": "BA", "lga": "Alkaleri", "ward": "Alkaleri East"});
        let mut geometry = square(3.0);
        for (k, v) in overrides.as_object().unwrap() {
            if k == "geometry" {
                geometry = v.clone();
            } else {
                properties[k] = v.clone();
            }
        }
        json!({"type": "Feature", "properties": properties, "geometry": geometry})
    }

    fn collection(features: Vec<Value>) -> Value {
        json!({"type": "FeatureCollection", "features": features})
    }

    fn run_build(
        features: Vec<Value>,
        levels: Vec<LevelSpec>,
        aliases: &[&str],
        dissolve: bool,
        report: &mut Report,
    ) -> Result<Vec<Value>> {
        let shape = icr_shape();
        let opts = Options {
            country: ("Nigeria".into(), "NGA".into()),
            levels,
            aliases: parse_aliases(&aliases.iter().map(|a| a.to_string()).collect::<Vec<_>>())
                .unwrap(),
            code_system: CODE_SYSTEM.into(),
            dissolve_parents: dissolve,
            shape: &shape,
        };
        build(&collection(features), &opts, report)
    }

    fn by_id(resources: &[Value]) -> HashMap<String, Value> {
        resources
            .iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r.clone()))
            .collect()
    }

    /// Shoelace sum: positive for a counterclockwise ring.
    fn signed_area2(ring: &Value) -> f64 {
        let pts: Vec<(f64, f64)> = ring
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect();
        pts.windows(2)
            .map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1)
            .sum()
    }

    fn boundary(resource: &Value) -> Geometry {
        let data = resource["extension"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|e| e["valueAttachment"]["data"].as_str())
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        let g: geojson::Geometry = serde_json::from_slice(&bytes).unwrap();
        crate::geometry::convert(g).unwrap().0
    }

    #[test]
    fn parses_country_level_and_alias_flags() {
        assert_eq!(
            parse_country("Nigeria=NGA").unwrap(),
            ("Nigeria".into(), "NGA".into())
        );
        assert!(parse_country("Nigeria").is_err());
        assert_eq!(
            parse_level("state=state:statecode").unwrap(),
            LevelSpec {
                name: "state".into(),
                name_prop: "state".into(),
                code_prop: Some("statecode".into())
            }
        );
        assert_eq!(parse_level("ward=ward").unwrap().code_prop, None);
        for bad in ["ward", "ward=", "=ward", "ward=a:b:c", "ward=a:"] {
            assert!(parse_level(bad).is_err(), "{bad}");
        }
        let aliases =
            parse_aliases(&["lga=lga_alt_names".into(), "ward=ward_alt_names".into()]).unwrap();
        assert_eq!(aliases["lga"], "lga_alt_names");
        assert!(parse_aliases(&["lga".into()]).is_err());
    }

    #[test]
    fn crs_must_be_wgs84_or_absent() {
        assert!(check_crs(&json!({"type": "FeatureCollection"})).is_ok());
        for name in [
            "EPSG:4326",
            "urn:ogc:def:crs:EPSG::4326",
            "urn:ogc:def:crs:OGC:1.3:CRS84",
            "CRS84",
        ] {
            let c = json!({"crs": {"type": "name", "properties": {"name": name}}});
            assert!(check_crs(&c).is_ok(), "{name}");
        }
        let c = json!({"crs": {"type": "name", "properties": {"name": "EPSG:3857"}}});
        assert!(check_crs(&c).is_err());
    }

    #[test]
    fn geometry_is_rewound_to_rfc7946() {
        let mut report = Report::default();
        let bytes = normalize_geometry(Some(&cw_square(3.0)), "w1", &mut report).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(signed_area2(&v["coordinates"][0]) > 0.0);

        let multi = json!({"type": "MultiPolygon", "coordinates": [
            cw_square(3.0)["coordinates"], cw_square(13.0)["coordinates"]]});
        let bytes = normalize_geometry(Some(&multi), "w1", &mut report).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "MultiPolygon");
        for polygon in v["coordinates"].as_array().unwrap() {
            assert!(signed_area2(&polygon[0]) > 0.0);
        }
        assert!(report.counts().is_empty());
    }

    #[test]
    fn unusable_geometry_is_reported() {
        let mut report = Report::default();
        let bowtie =
            json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 1], [1, 0], [0, 1], [0, 0]]]});
        assert!(normalize_geometry(Some(&bowtie), "w1", &mut report).is_none());
        let point = json!({"type": "Point", "coordinates": [3.0, 6.0]});
        assert!(normalize_geometry(Some(&point), "w2", &mut report).is_none());
        assert!(normalize_geometry(None, "w3", &mut report).is_none());
        let incomplete = json!({"type": "Polygon"});
        assert!(normalize_geometry(Some(&incomplete), "w4", &mut report).is_none());
        assert_eq!(report.count("geometry_invalid"), 4);
    }

    #[test]
    fn mints_the_full_hierarchy_parents_first() {
        let mut report = Report::default();
        let out = run_build(
            vec![
                feature(json!({})),
                feature(json!({"ward": "Alkaleri West"})),
            ],
            levels(),
            &[],
            false,
            &mut report,
        )
        .unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "nga",
                "nga-ba",
                "nga-ba-alkaleri",
                "nga-ba-alkaleri-alkaleri-east",
                "nga-ba-alkaleri-alkaleri-west"
            ]
        );
        let by = by_id(&out);
        assert_eq!(by["nga-ba"]["partOf"], json!({"reference": "Location/nga"}));
        assert_eq!(by["nga-ba"]["name"], "Bauchi");
        // The state's code comes from statecode; a level without one gets its slug.
        assert_eq!(
            by["nga-ba"]["identifier"],
            json!([{"system": CODE_SYSTEM, "value": "BA"}])
        );
        assert_eq!(
            by["nga-ba-alkaleri"]["identifier"],
            json!([{"system": CODE_SYSTEM, "value": "nga-ba-alkaleri"}])
        );
        assert_eq!(
            by["nga"]["identifier"],
            json!([{"system": CODE_SYSTEM, "value": "NGA"}])
        );
        // Only the last level carries a boundary.
        assert!(by["nga-ba-alkaleri"].get("extension").is_none());
        assert!(
            by["nga-ba-alkaleri-alkaleri-east"]
                .get("extension")
                .is_some()
        );
        assert!(report.counts().is_empty());
    }

    #[test]
    fn aliases_are_split_on_semicolons() {
        let out = run_build(
            vec![feature(json!({"ward_alt_names": "Alkaleri E.; The East"}))],
            levels(),
            &["ward=ward_alt_names"],
            false,
            &mut Report::default(),
        )
        .unwrap();
        assert_eq!(
            out.last().unwrap()["alias"],
            json!(["Alkaleri E.", "The East"])
        );
    }

    #[test]
    fn a_feature_missing_a_name_is_skipped_and_reported() {
        let mut report = Report::default();
        let out = run_build(
            vec![feature(json!({})), feature(json!({"ward": ""}))],
            levels(),
            &[],
            false,
            &mut report,
        )
        .unwrap();
        assert_eq!(
            out.iter().filter(|r| r.get("extension").is_some()).count(),
            1
        );
        assert_eq!(report.count("missing_field"), 1);
    }

    #[test]
    fn invalid_geometry_leaves_a_unit_without_a_boundary() {
        let mut report = Report::default();
        let out = run_build(
            vec![feature(json!({"geometry": null}))],
            levels(),
            &[],
            false,
            &mut report,
        )
        .unwrap();
        let ward = out.last().unwrap();
        assert_eq!(ward["id"], "nga-ba-alkaleri-alkaleri-east");
        assert!(ward.get("extension").is_none());
        assert_eq!(report.count("geometry_invalid"), 1);
    }

    #[test]
    fn mapping_problems_are_fatal() {
        let err = |features: Vec<Value>, levels: Vec<LevelSpec>| {
            run_build(features, levels, &[], false, &mut Report::default())
                .unwrap_err()
                .to_string()
        };
        assert!(
            err(
                vec![
                    feature(json!({"ward": "Alkaleri East"})),
                    feature(json!({"ward": "Alkaleri  East!"}))
                ],
                levels()
            )
            .contains("collision")
        );
        assert!(err(vec![feature(json!({})), feature(json!({}))], levels()).contains("duplicate"));
        let mut typo = levels();
        typo[2] = parse_level("ward=wardd").unwrap();
        assert!(err(vec![feature(json!({}))], typo).contains("wardd"));

        let shape = icr_shape();
        let opts = Options {
            country: ("Nigeria".into(), "NGA".into()),
            levels: levels(),
            aliases: HashMap::new(),
            code_system: CODE_SYSTEM.into(),
            dissolve_parents: false,
            shape: &shape,
        };
        assert!(build(&json!({"type": "Feature"}), &opts, &mut Report::default()).is_err());
    }

    #[test]
    fn a_too_long_id_is_reported_not_written() {
        let mut report = Report::default();
        let long = "x".repeat(60);
        let out = run_build(
            vec![feature(json!({})), feature(json!({"ward": long}))],
            levels(),
            &[],
            false,
            &mut report,
        )
        .unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(report.count("invalid_id"), 1);
    }

    #[test]
    fn dissolve_parents_unions_children_at_every_level() {
        let mut report = Report::default();
        let out = run_build(
            vec![
                feature(json!({})),
                feature(json!({"ward": "Alkaleri West", "geometry": square(4.0)})),
            ],
            levels(),
            &[],
            true,
            &mut report,
        )
        .unwrap();
        let by = by_id(&out);
        for parent in ["nga", "nga-ba", "nga-ba-alkaleri"] {
            match boundary(&by[parent]) {
                Geometry::Polygon(p) => assert!((p.unsigned_area() - 2.0).abs() < 1e-9),
                other => panic!("{parent}: {other:?}"),
            }
        }
        let leaf = boundary(&by["nga-ba-alkaleri-alkaleri-east"]);
        assert!((leaf.unsigned_area() - 1.0).abs() < 1e-9);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn dissolve_parents_is_off_by_default() {
        let out = run_build(
            vec![feature(json!({}))],
            levels(),
            &[],
            false,
            &mut Report::default(),
        )
        .unwrap();
        let by = by_id(&out);
        assert!(by["nga"].get("extension").is_none());
        assert!(by["nga-ba-alkaleri"].get("extension").is_none());
    }

    #[test]
    fn dissolve_skips_invalid_children() {
        let mut report = Report::default();
        let out = run_build(
            vec![
                feature(json!({})),
                feature(json!({"ward": "Alkaleri West", "geometry": null})),
            ],
            levels(),
            &[],
            true,
            &mut report,
        )
        .unwrap();
        let by = by_id(&out);
        assert!((boundary(&by["nga-ba-alkaleri"]).unsigned_area() - 1.0).abs() < 1e-9);
        assert!(
            by["nga-ba-alkaleri-alkaleri-west"]
                .get("extension")
                .is_none()
        );
        assert_eq!(report.count("geometry_invalid"), 1);
    }

    #[test]
    fn dissolve_keeps_separate_parents_separate() {
        let out = run_build(
            vec![
                feature(json!({})),
                feature(
                    json!({"lga": "Bogoro", "ward": "Bogoro Central", "geometry": square(4.0)}),
                ),
            ],
            levels(),
            &[],
            true,
            &mut Report::default(),
        )
        .unwrap();
        let by = by_id(&out);
        assert!((boundary(&by["nga-ba-alkaleri"]).unsigned_area() - 1.0).abs() < 1e-9);
        assert!((boundary(&by["nga-ba-bogoro"]).unsigned_area() - 1.0).abs() < 1e-9);
        assert!((boundary(&by["nga-ba"]).unsigned_area() - 2.0).abs() < 1e-9);
    }
}
