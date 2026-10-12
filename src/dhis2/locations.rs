//! `kiln dhis2 locations`: the org unit tree as ICR Locations.
//!
//! Levels above the facility level become admin units; the facility level
//! becomes facility Locations, each paired with an Organization the way the
//! ICR models facilities (`managingOrganization` → `org-<location id>`).
//! Every resource carries the org unit UID as an official identifier, which
//! is what `kiln dhis2 reports` maps analytics rows through.

use std::collections::{BTreeMap, HashMap, HashSet};

use base64::Engine;
use serde_json::{json, Value};

use super::api::Dhis2;
use super::{check_out, valid_name, write_ndjson, Instance};
use crate::cli::Dhis2LocationsArgs;
use crate::error::{KilnError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::location::{
    ADMIN_UNIT_TYPE, BOUNDARY_EXTENSION_URL, GEOJSON_CONTENT_TYPE, NATIONAL_ADMIN_CODE_SYSTEM,
};
use crate::fhir::organization::ORGANIZATION_TYPE_SYSTEM;
use crate::report::Report;
use crate::transform::write_report;

pub const LOCATION_PROFILE: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/ICRLocation";
pub const FACILITY_ORGANIZATION_PROFILE: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization";
pub const LOCATION_TYPE_SYSTEM: &str =
    "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs";
pub const PHYSICAL_TYPE_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/location-physical-type";
pub const FACILITY_TYPE: &str = "facility";
/// Location id prefixes share the id budget with the 11-character UID.
pub const MAX_PREFIX_LEN: usize = 32;

const ORG_UNIT_FIELDS: &str =
    "id,code,name,shortName,level,parent[id],openingDate,closedDate,geometry";

#[derive(Debug, Clone, Default)]
pub struct OrgUnit {
    pub id: String,
    pub code: Option<String>,
    pub name: String,
    pub short_name: Option<String>,
    pub level: u32,
    pub parent: Option<String>,
    pub closed_date: Option<String>,
    pub geometry: Option<Value>,
}

impl OrgUnit {
    pub fn parse(v: &Value, report: &mut Report) -> Option<OrgUnit> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let Some(id) = s("id") else {
            report.add("missing_id", "<unknown>", "org unit has no id");
            return None;
        };
        let Some(level) = v.get("level").and_then(Value::as_u64) else {
            report.add("malformed_field", &id, "org unit has no level");
            return None;
        };
        Some(OrgUnit {
            name: s("name").unwrap_or_else(|| id.clone()),
            code: s("code"),
            short_name: s("shortName"),
            level: level as u32,
            parent: v
                .pointer("/parent/id")
                .and_then(Value::as_str)
                .map(str::to_string),
            closed_date: s("closedDate"),
            geometry: v.get("geometry").filter(|g| g.is_object()).cloned(),
            id,
        })
    }
}

pub struct Options<'a> {
    pub instance: &'a Instance,
    pub prefix: &'a str,
    pub country_code: Option<&'a str>,
    pub facility_level: u32,
    /// Org unit uid → its groups in the requested group sets, as
    /// (group set uid, group uid, group name).
    pub groups: HashMap<String, Vec<(String, String, String)>>,
}

pub fn location_id(prefix: &str, uid: &str) -> String {
    format!("{prefix}-{uid}")
}

fn identifiers(unit: &OrgUnit, opts: &Options) -> Vec<Value> {
    let mut out = vec![json!({
        "use": "official",
        "system": opts.instance.orgunit_system(),
        "value": unit.id,
    })];
    if let Some(code) = &unit.code {
        out.push(json!({"system": opts.instance.orgunit_code_system(), "value": code}));
    }
    out
}

/// The Location (and, at the facility level, its Organization) for one org
/// unit. `parent_known` says whether the parent is in the export.
pub fn resources(
    unit: &OrgUnit,
    opts: &Options,
    parent_known: bool,
    is_top: bool,
    report: &mut Report,
) -> Vec<Value> {
    let id = location_id(opts.prefix, &unit.id);
    if !is_valid_id(&id) {
        report.add(
            "invalid_id",
            &unit.id,
            &format!("{id:?} is not a valid FHIR id; org unit skipped"),
        );
        return Vec::new();
    }
    let facility = unit.level == opts.facility_level;
    let mut loc = json!({
        "resourceType": "Location",
        "id": id,
        "meta": {"profile": [LOCATION_PROFILE], "tag": [opts.instance.tag()]},
    });
    let mut position = None;
    match unit
        .geometry
        .as_ref()
        .and_then(|g| g.get("type"))
        .and_then(Value::as_str)
    {
        None => {}
        Some("Polygon" | "MultiPolygon") => {
            let geojson = unit
                .geometry
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default();
            loc["extension"] = json!([{
                "url": BOUNDARY_EXTENSION_URL,
                "valueAttachment": {
                    "contentType": GEOJSON_CONTENT_TYPE,
                    "data": base64::engine::general_purpose::STANDARD.encode(geojson),
                },
            }]);
        }
        Some("Point") => {
            let c = unit
                .geometry
                .as_ref()
                .and_then(|g| g.get("coordinates"))
                .and_then(Value::as_array);
            match c.and_then(|c| Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))) {
                Some((lon, lat))
                    if (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) =>
                {
                    position = Some(json!({"longitude": lon, "latitude": lat}));
                }
                _ => report.add(
                    "bad_geometry",
                    &unit.id,
                    "Point has no usable coordinates; left out",
                ),
            }
        }
        Some(other) => report.add(
            "bad_geometry",
            &unit.id,
            &format!("{other} geometry is not a boundary or a point; left out"),
        ),
    }
    let mut ids = identifiers(unit, opts);
    if is_top {
        if let Some(cc) = opts.country_code {
            ids.push(json!({"system": NATIONAL_ADMIN_CODE_SYSTEM, "value": cc}));
        }
    }
    loc["identifier"] = Value::Array(ids);
    loc["status"] = json!(if unit.closed_date.is_some() {
        "inactive"
    } else {
        "active"
    });
    loc["name"] = json!(unit.name);
    if let Some(short) = unit.short_name.as_ref().filter(|s| **s != unit.name) {
        loc["alias"] = json!([short]);
    }
    let (type_code, physical, physical_display) = if facility {
        (FACILITY_TYPE, "si", "Site")
    } else {
        (ADMIN_UNIT_TYPE, "jdn", "Jurisdiction")
    };
    let group_types: Vec<Value> = opts
        .groups
        .get(&unit.id)
        .into_iter()
        .flatten()
        .map(|(set, code, name)| {
            json!({"coding": [{"system": opts.instance.dimension_system(Some(set)), "code": code, "display": name}], "text": name})
        })
        .collect();
    let mut types = vec![json!({"coding": [{"system": LOCATION_TYPE_SYSTEM, "code": type_code}]})];
    types.extend(group_types.iter().cloned());
    loc["type"] = Value::Array(types);
    loc["physicalType"] = json!({"coding": [{"system": PHYSICAL_TYPE_SYSTEM, "code": physical, "display": physical_display}]});
    if let Some(p) = position {
        loc["position"] = p;
    }
    let org_id = format!("org-{id}");
    if facility {
        loc["managingOrganization"] = json!({"reference": format!("Organization/{org_id}")});
    }
    match &unit.parent {
        Some(p) if parent_known => {
            loc["partOf"] =
                json!({"reference": format!("Location/{}", location_id(opts.prefix, p))});
        }
        Some(p) if !is_top => report.add(
            "parent_not_exported",
            &unit.id,
            &format!("parent org unit {p} is not in the export; Location written without partOf"),
        ),
        _ => {}
    }
    let mut out = Vec::new();
    if facility {
        out.push(json!({
            "resourceType": "Organization",
            "id": org_id,
            "meta": {"profile": [FACILITY_ORGANIZATION_PROFILE], "tag": [opts.instance.tag()]},
            "identifier": identifiers(unit, opts),
            "active": unit.closed_date.is_none(),
            "type": std::iter::once(json!({"coding": [{"system": ORGANIZATION_TYPE_SYSTEM, "code": "prov", "display": "Healthcare Provider"}]}))
                .chain(group_types)
                .collect::<Vec<_>>(),
            "name": unit.name,
        }));
    }
    out.push(loc);
    out
}

fn polygons(geometry: &Value) -> Option<geo::MultiPolygon<f64>> {
    let g: geojson::Geometry = serde_json::from_value(geometry.clone()).ok()?;
    match geo::Geometry::<f64>::try_from(g).ok()? {
        geo::Geometry::Polygon(p) => Some(geo::MultiPolygon(vec![p])),
        geo::Geometry::MultiPolygon(mp) => Some(mp),
        _ => None,
    }
}

/// An admin unit DHIS2 has no geometry for (often the country itself) gets
/// the union of its children's polygons, so it still has a boundary and a
/// row in the dataset. Deepest first, so a unit filled this way can in turn
/// fill its parent.
pub fn fill_missing_boundaries(units: &mut [OrgUnit], facility_level: u32, report: &mut Report) {
    let mut children: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, u) in units.iter().enumerate() {
        if let Some(p) = &u.parent {
            children.entry(p.clone()).or_default().push(i);
        }
    }
    let mut order: Vec<usize> = (0..units.len())
        .filter(|&i| units[i].level < facility_level && units[i].geometry.is_none())
        .collect();
    order.sort_by_key(|&i| std::cmp::Reverse(units[i].level));
    for i in order {
        let Some(kids) = children.get(&units[i].id) else {
            continue;
        };
        let parts: Vec<geo::MultiPolygon<f64>> = kids
            .iter()
            .filter_map(|&k| units[k].geometry.as_ref().and_then(polygons))
            .collect();
        if parts.is_empty() {
            continue;
        }
        let union = geo::unary_union(parts.iter());
        let value = geojson::Geometry::new(geojson::GeometryValue::from(&union));
        if let Ok(v) = serde_json::to_value(value) {
            report.add(
                "boundary_from_children",
                &units[i].id,
                &format!(
                    "no geometry in DHIS2; boundary is the union of {} of its {} children",
                    parts.len(),
                    kids.len()
                ),
            );
            units[i].geometry = Some(v);
        }
    }
}

/// Every Location and Organization for `units`, top level first, then by id.
pub fn build(units: &[OrgUnit], opts: &Options, report: &mut Report) -> Vec<Value> {
    let known: HashSet<&str> = units.iter().map(|u| u.id.as_str()).collect();
    let top = units.iter().map(|u| u.level).min().unwrap_or(1);
    let mut sorted: Vec<&OrgUnit> = units.iter().collect();
    sorted.sort_by(|a, b| (a.level, &a.id).cmp(&(b.level, &b.id)));
    let mut out = Vec::new();
    for unit in sorted {
        if unit.level > opts.facility_level {
            report.add(
                "below_facility_level",
                &unit.id,
                &format!(
                    "level {} is below the facility level {}; skipped",
                    unit.level, opts.facility_level
                ),
            );
            continue;
        }
        let parent_known = unit.parent.as_deref().is_some_and(|p| known.contains(p));
        out.extend(resources(
            unit,
            opts,
            parent_known,
            unit.level == top,
            report,
        ));
    }
    out
}

pub fn run_locations(args: &Dhis2LocationsArgs) -> Result<()> {
    check_out(&args.out)?;
    let instance = Instance::new(&args.conn.instance)?;
    let prefix = args
        .id_prefix
        .clone()
        .unwrap_or_else(|| instance.name.clone());
    if !valid_name(&prefix, MAX_PREFIX_LEN) {
        return Err(KilnError::Usage(format!(
            "--id-prefix must be letters, digits and hyphens, at most {MAX_PREFIX_LEN} characters, got {prefix:?}"
        )));
    }
    let api = Dhis2::connect(&args.conn)?;
    eprintln!(
        "DHIS2 {} ({})",
        api.info.system_name.as_deref().unwrap_or("?"),
        api.info.version.as_deref().unwrap_or("unknown version")
    );

    let levels = api.get(
        "organisationUnitLevels.json",
        &[("paging", "false".into()), ("fields", "level,name".into())],
    )?;
    let level_names: BTreeMap<u64, String> = levels
        .get("organisationUnitLevels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some((
                l.get("level")?.as_u64()?,
                l.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect();

    let mut query = vec![("fields", ORG_UNIT_FIELDS.to_string())];
    if let Some(root) = &args.root {
        query.push(("filter", format!("path:like:{root}")));
    }
    let raw = api.paged("organisationUnits.json", "organisationUnits", &query)?;
    let mut report = Report::default();
    let mut units: Vec<OrgUnit> = raw
        .iter()
        .filter_map(|v| OrgUnit::parse(v, &mut report))
        .collect();
    if units.is_empty() {
        return Err(KilnError::Usage(match &args.root {
            Some(r) => format!("DHIS2 returned no org units at or below {r}"),
            None => "DHIS2 returned no org units; does this user have any assigned?".into(),
        }));
    }
    let deepest = units.iter().map(|u| u.level).max().unwrap_or(1);
    let facility_level = args.facility_level.unwrap_or(deepest);
    fill_missing_boundaries(&mut units, facility_level, &mut report);
    let mut groups: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for set in &args.group_sets {
        let v = match api.try_get(
            &format!("organisationUnitGroupSets/{set}.json"),
            &[(
                "fields",
                "id,name,organisationUnitGroups[id,name,organisationUnits[id]]".into(),
            )],
        )? {
            Ok(v) => v,
            Err(crate::extract::client::FetchError::Status { status: 404, .. }) => {
                return Err(KilnError::Usage(format!(
                    "DHIS2 has no org unit group set {set}"
                )))
            }
            Err(e) => return Err(api.error("organisationUnitGroupSets", e)),
        };
        for g in v
            .get("organisationUnitGroups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(gid), Some(gname)) = (
                g.get("id").and_then(Value::as_str),
                g.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            for ou in g
                .get("organisationUnits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(ou) = ou.get("id").and_then(Value::as_str) {
                    groups.entry(ou.to_string()).or_default().push((
                        set.clone(),
                        gid.to_string(),
                        gname.to_string(),
                    ));
                }
            }
        }
    }
    let opts = Options {
        instance: &instance,
        prefix: &prefix,
        country_code: args.country_code.as_deref(),
        facility_level,
        groups,
    };
    let resources = build(&units, &opts, &mut report);
    let n = write_ndjson(&args.out, &resources)?;

    let mut per_level: BTreeMap<u32, usize> = BTreeMap::new();
    for u in &units {
        *per_level.entry(u.level).or_default() += 1;
    }
    let organizations = resources
        .iter()
        .filter(|r| r["resourceType"] == "Organization")
        .count();
    println!(
        "Wrote {n} resources to {} ({} Locations, {organizations} Organizations)",
        args.out.display(),
        n - organizations
    );
    for (level, count) in &per_level {
        let name = level_names
            .get(&u64::from(*level))
            .map(String::as_str)
            .unwrap_or("?");
        let role = match level.cmp(&facility_level) {
            std::cmp::Ordering::Less => "admin units",
            std::cmp::Ordering::Equal => "facilities",
            std::cmp::Ordering::Greater => "skipped",
        };
        println!("  level {level} {name}: {count} ({role})");
    }
    println!("Org unit identifier system: {}", instance.orgunit_system());
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: &str, level: u32, parent: Option<&str>, geometry: Option<Value>) -> OrgUnit {
        OrgUnit {
            id: id.into(),
            code: Some(format!("OU_{id}")),
            name: format!("Unit {id}"),
            short_name: None,
            level,
            parent: parent.map(str::to_string),
            closed_date: None,
            geometry,
        }
    }

    fn opts(instance: &Instance) -> Options<'_> {
        Options {
            instance,
            prefix: "sle",
            country_code: Some("SLE"),
            facility_level: 3,
            groups: HashMap::from([(
                "f".to_string(),
                vec![(
                    "J5jldMd8OHv".to_string(),
                    "uYxK4wmcPqA".to_string(),
                    "CHP".to_string(),
                )],
            )]),
        }
    }

    #[test]
    fn org_units_parse_from_the_api_shape() {
        let mut report = Report::default();
        let v = json!({"id":"O6uvpzGd5pu","code":"OU_264","name":"Bo","shortName":"Bo","level":2,
                       "parent":{"id":"ImspTQPwCqd"},"geometry":{"type":"Polygon","coordinates":[]}});
        let u = OrgUnit::parse(&v, &mut report).unwrap();
        assert_eq!(u.id, "O6uvpzGd5pu");
        assert_eq!(u.level, 2);
        assert_eq!(u.parent.as_deref(), Some("ImspTQPwCqd"));
        assert!(u.geometry.is_some());
        assert!(OrgUnit::parse(&json!({"name": "x"}), &mut report).is_none());
        assert!(OrgUnit::parse(&json!({"id": "x"}), &mut report).is_none());
        assert_eq!(report.count("missing_id"), 1);
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn levels_become_admin_units_then_facilities_with_organizations() {
        let instance = Instance::new("sl-demo").unwrap();
        let poly =
            json!({"type":"Polygon","coordinates":[[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,0.0]]]});
        let units = vec![
            unit("c", 1, None, None),
            unit("d", 2, Some("c"), Some(poly.clone())),
            unit(
                "f",
                3,
                Some("d"),
                Some(json!({"type":"Point","coordinates":[-11.7,8.4]})),
            ),
            unit("x", 4, Some("f"), None),
        ];
        let mut report = Report::default();
        let out = build(&units, &opts(&instance), &mut report);
        let ids: Vec<&str> = out.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["sle-c", "sle-d", "org-sle-f", "sle-f"]);
        assert_eq!(report.count("below_facility_level"), 1);

        let country = &out[0];
        assert_eq!(country["type"][0]["coding"][0]["code"], "admin-unit");
        assert!(country.get("partOf").is_none());
        let systems: Vec<&str> = country["identifier"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["system"].as_str().unwrap())
            .collect();
        assert!(systems.contains(&NATIONAL_ADMIN_CODE_SYSTEM));
        assert_eq!(country["identifier"][0]["use"], "official");
        assert_eq!(country["identifier"][0]["value"], "c");
        assert_eq!(
            country["identifier"][0]["system"],
            instance.orgunit_system()
        );

        let district = &out[1];
        assert_eq!(district["partOf"]["reference"], "Location/sle-c");
        let ext = &district["extension"][0];
        assert_eq!(ext["url"], BOUNDARY_EXTENSION_URL);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(ext["valueAttachment"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&decoded).unwrap(), poly);
        assert_eq!(district["identifier"].as_array().unwrap().len(), 2);

        let org = &out[2];
        assert_eq!(org["resourceType"], "Organization");
        assert_eq!(org["identifier"][0]["value"], "f");
        let facility = &out[3];
        assert_eq!(facility["type"][0]["coding"][0]["code"], "facility");
        assert_eq!(facility["physicalType"]["coding"][0]["code"], "si");
        assert_eq!(
            facility["managingOrganization"]["reference"],
            "Organization/org-sle-f"
        );
        assert_eq!(facility["position"]["longitude"], -11.7);
        assert_eq!(facility["type"][1]["text"], "CHP");
        assert_eq!(
            facility["type"][1]["coding"][0]["system"],
            instance.dimension_system(Some("J5jldMd8OHv"))
        );
        assert_eq!(org["type"][1]["coding"][0]["code"], "uYxK4wmcPqA");
        assert_eq!(facility["partOf"]["reference"], "Location/sle-d");
    }

    #[test]
    fn closed_units_are_inactive_and_missing_parents_are_reported() {
        let instance = Instance::new("sl-demo").unwrap();
        let mut closed = unit("d", 2, Some("gone"), None);
        closed.closed_date = Some("2020-01-01".into());
        let units = vec![unit("c", 1, None, None), closed];
        let mut report = Report::default();
        let out = build(&units, &opts(&instance), &mut report);
        assert_eq!(out[1]["status"], "inactive");
        assert!(out[1].get("partOf").is_none());
        assert_eq!(report.count("parent_not_exported"), 1);
    }

    #[test]
    fn a_subtree_root_has_no_partof_and_no_complaint() {
        let instance = Instance::new("sl-demo").unwrap();
        let units = vec![unit("d", 2, Some("c"), None), unit("e", 3, Some("d"), None)];
        let mut report = Report::default();
        let out = build(&units, &opts(&instance), &mut report);
        assert!(out[0].get("partOf").is_none());
        assert_eq!(report.count("parent_not_exported"), 0);
    }

    #[test]
    fn a_unit_without_geometry_gets_the_union_of_its_children() {
        let square = |x: f64| json!({"type":"Polygon","coordinates":[[[x,0.0],[x+1.0,0.0],[x+1.0,1.0],[x,1.0],[x,0.0]]]});
        let mut units = vec![
            unit("c", 1, None, None),
            unit("a", 2, Some("c"), Some(square(0.0))),
            unit("b", 2, Some("c"), Some(square(1.0))),
            unit("e", 2, Some("c"), None),
            unit(
                "f",
                3,
                Some("e"),
                Some(json!({"type":"Point","coordinates":[0.5,0.5]})),
            ),
        ];
        let mut report = Report::default();
        fill_missing_boundaries(&mut units, 3, &mut report);
        let g = units[0].geometry.as_ref().expect("country filled");
        let mp = polygons(g).unwrap();
        use geo::Area;
        assert!((mp.unsigned_area() - 2.0).abs() < 1e-9);
        // A unit whose only child is a facility point stays without a boundary.
        assert!(units[3].geometry.is_none());
        assert_eq!(report.count("boundary_from_children"), 1);
    }

    #[test]
    fn unusable_geometry_is_reported_and_left_out() {
        let instance = Instance::new("sl-demo").unwrap();
        let units = vec![
            unit(
                "c",
                1,
                None,
                Some(json!({"type":"LineString","coordinates":[]})),
            ),
            unit(
                "d",
                2,
                Some("c"),
                Some(json!({"type":"Point","coordinates":[500.0, 1.0]})),
            ),
        ];
        let mut report = Report::default();
        let out = build(&units, &opts(&instance), &mut report);
        assert!(out[0].get("extension").is_none());
        assert!(out[1].get("position").is_none());
        assert_eq!(report.count("bad_geometry"), 2);
    }
}
