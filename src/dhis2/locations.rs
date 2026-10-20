//! `fhir-pipes dhis2 locations`: the org unit tree as ICR Locations.
//!
//! Levels above the facility level become admin units; the facility level
//! becomes facility Locations, each paired with an Organization the way the
//! ICR models facilities (`managingOrganization` → `org-<location id>`).
//! Every resource carries the org unit UID as an official identifier, which
//! is what `fhir-pipes dhis2 reports` maps analytics rows through.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::api::Dhis2;
use super::{Instance, NdjsonWriter, check_out, valid_name};
use crate::cli::Dhis2LocationsArgs;
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::location::{
    LocationKey, NewLocation, PhysicalType, boundary_extension, identifier,
};
use crate::fhir::organization::{NewOrganization, OrganizationKey};
use crate::report::Report;
use crate::transform::write_report;

/// Location id prefixes share the id budget with the 11-character UID.
pub const MAX_PREFIX_LEN: usize = 32;

/// The key order of the Locations dhis2 locations writes.
const LOCATION_KEYS: [LocationKey; 11] = [
    LocationKey::Meta,
    LocationKey::Extension,
    LocationKey::Identifier,
    LocationKey::Status,
    LocationKey::Name,
    LocationKey::Alias,
    LocationKey::Type,
    LocationKey::PhysicalType,
    LocationKey::Position,
    LocationKey::ManagingOrganization,
    LocationKey::PartOf,
];

/// The key order of the facility Organizations dhis2 locations writes.
const ORGANIZATION_KEYS: [OrganizationKey; 5] = [
    OrganizationKey::Meta,
    OrganizationKey::Identifier,
    OrganizationKey::Active,
    OrganizationKey::Type,
    OrganizationKey::Name,
];

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
        let Some(level) = v
            .get("level")
            .and_then(Value::as_u64)
            .and_then(|l| u32::try_from(l).ok())
        else {
            report.add("malformed_field", &id, "org unit has no level");
            return None;
        };
        Some(OrgUnit {
            name: s("name").unwrap_or_else(|| id.clone()),
            code: s("code"),
            short_name: s("shortName"),
            level,
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
    /// Org unit uid → the groups added to its type.
    pub groups: HashMap<String, Vec<GroupType>>,
}

/// An org unit group on a Location's type: coded under each group set it
/// is given for (`…/dimension/<set uid>`, the system a `--breakdown` by the
/// set uses), or, for a group in no set, under `…/org-unit-group`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupType {
    pub group: String,
    pub name: String,
    pub sets: Vec<String>,
}

impl GroupType {
    fn concept(&self, instance: &Instance) -> Value {
        let coding: Vec<Value> = if self.sets.is_empty() {
            vec![
                json!({"system": instance.group_system("org-unit-group"), "code": self.group, "display": self.name}),
            ]
        } else {
            self.sets
                .iter()
                .map(|set| json!({"system": instance.dimension_system(Some(set)), "code": self.group, "display": self.name}))
                .collect()
        };
        json!({"coding": coding, "text": self.name})
    }
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
        out.push(identifier(&opts.instance.orgunit_code_system(), code));
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
    let mut extension = Vec::new();
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
            extension.push(boundary_extension(
                &opts.instance.boundary_extension,
                geojson.as_bytes(),
            ));
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
                    position = Some((lon, lat));
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
    if is_top && let Some(cc) = opts.country_code {
        ids.push(identifier(&opts.instance.urls.country_code_system, cc));
    }
    // run_locations checked both are in the profile.
    let (type_code, physical) = if facility {
        (
            opts.instance.facility_type.as_deref().unwrap_or_default(),
            PhysicalType::Site,
        )
    } else {
        (
            opts.instance.admin_unit_type.as_deref().unwrap_or_default(),
            PhysicalType::Jurisdiction,
        )
    };
    let group_types: Vec<Value> = opts
        .groups
        .get(&unit.id)
        .into_iter()
        .flatten()
        .map(|g| g.concept(opts.instance))
        .collect();
    let org_id = format!("{}{id}", opts.instance.org_id_prefix);
    let parent = match &unit.parent {
        Some(p) if parent_known => Some(location_id(opts.prefix, p)),
        Some(p) if !is_top => {
            report.add(
                "parent_not_exported",
                &unit.id,
                &format!(
                    "parent org unit {p} is not in the export; Location written without partOf"
                ),
            );
            None
        }
        _ => None,
    };
    let meta = |profile: &str| json!({"profile": [profile], "tag": [opts.instance.tag()]});
    let loc = NewLocation {
        id: &id,
        meta: Some(meta(&opts.instance.urls.location_profile)),
        extension,
        identifier: ids,
        status: if unit.closed_date.is_some() {
            "inactive"
        } else {
            "active"
        },
        name: &unit.name,
        alias: unit
            .short_name
            .as_ref()
            .filter(|s| **s != unit.name)
            .map(std::slice::from_ref)
            .unwrap_or_default(),
        type_system: &opts.instance.urls.location_type_system,
        type_code,
        extra_types: group_types.clone(),
        physical_type: Some(physical),
        position,
        managing_organization: facility.then_some(org_id.as_str()),
        part_of: parent.as_deref(),
    }
    .into_json(&LOCATION_KEYS);
    let mut out = Vec::new();
    if facility {
        out.push(
            NewOrganization {
                id: &org_id,
                meta: Some(meta(&opts.instance.urls.facility_organization_profile)),
                identifier: identifiers(unit, opts),
                active: unit.closed_date.is_none(),
                extra_types: group_types,
                name: Some(&unit.name),
            }
            .into_json(&ORGANIZATION_KEYS),
        );
    }
    out.push(loc);
    out
}

/// A unit's Polygon or MultiPolygon geometry; `None` for any other kind. One
/// that cannot be read (e.g. a position with fewer than 2 numbers) is
/// reported as `bad_geometry` against `id`.
fn polygons(geometry: &Value, id: &str, report: &mut Report) -> Option<geo::MultiPolygon<f64>> {
    if !matches!(
        geometry.get("type").and_then(Value::as_str),
        Some("Polygon" | "MultiPolygon")
    ) {
        return None;
    }
    let parsed = crate::geometry::convert_value(geometry.clone());
    match parsed {
        Ok((geo::Geometry::Polygon(p), _)) => Some(geo::MultiPolygon(vec![p])),
        Ok((geo::Geometry::MultiPolygon(mp), _)) => Some(mp),
        Ok(_) => None,
        Err(e) => {
            report.add(
                "bad_geometry",
                id,
                &format!("unreadable boundary ({e}); not used for its parent's boundary"),
            );
            None
        }
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
            .filter_map(|&k| {
                let g = units[k].geometry.as_ref()?;
                polygons(g, &units[k].id, report)
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        let union = geo::unary_union(parts.iter());
        report.add(
            "boundary_from_children",
            &units[i].id,
            &format!(
                "no geometry in DHIS2; boundary is the union of {} of its {} children",
                parts.len(),
                kids.len()
            ),
        );
        let union = geo::Geometry::MultiPolygon(union);
        units[i].geometry = Some(crate::geometry::to_geojson_value(&union));
    }
}

/// Every Location and Organization for `units`, top level first, then by id.
/// The resources for `units`, parents first, handed to `emit` as each unit
/// is built.
pub fn build(
    units: &[OrgUnit],
    opts: &Options,
    report: &mut Report,
    mut emit: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    let known: HashSet<&str> = units.iter().map(|u| u.id.as_str()).collect();
    let top = units.iter().map(|u| u.level).min().unwrap_or(1);
    let mut sorted: Vec<&OrgUnit> = units.iter().collect();
    sorted.sort_by(|a, b| (a.level, &a.id).cmp(&(b.level, &b.id)));
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
        for resource in resources(unit, opts, parent_known, unit.level == top, report) {
            emit(resource)?;
        }
    }
    Ok(())
}

const GROUP_FIELDS: &str = "id,name,groupSets[id],organisationUnits[id]";

/// `--org-unit-group` and `--all-org-unit-groups`: each group's members get
/// it in their type, unless a `--group-set` already gave it to them.
/// Returns how many groups were read.
fn add_org_unit_groups(
    api: &Dhis2,
    args: &Dhis2LocationsArgs,
    groups: &mut HashMap<String, Vec<GroupType>>,
) -> Result<usize> {
    let list = if args.all_org_unit_groups {
        let mut all = api.paged(
            "organisationUnitGroups.json",
            "organisationUnitGroups",
            &[("fields", GROUP_FIELDS.into())],
        )?;
        all.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        all
    } else {
        let mut list = Vec::new();
        for uid in &args.org_unit_groups {
            match api.try_get(
                &format!("organisationUnitGroups/{uid}.json"),
                &[("fields", GROUP_FIELDS.into())],
            )? {
                Ok(v) => list.push(v),
                Err(crate::extract::client::FetchError::Status { status: 404, .. }) => {
                    return Err(PipesError::Usage(format!(
                        "DHIS2 has no org unit group {uid}"
                    )));
                }
                Err(e) => return Err(api.error("organisationUnitGroups", e)),
            }
        }
        list
    };
    let ids = |v: &Value, key: &str| -> Vec<String> {
        let mut ids: Vec<String> = v
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|x| x.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids
    };
    for g in &list {
        let (Some(gid), Some(name)) = (
            g.get("id").and_then(Value::as_str),
            g.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        let group = GroupType {
            group: gid.to_string(),
            name: name.to_string(),
            sets: ids(g, "groupSets"),
        };
        for ou in ids(g, "organisationUnits") {
            let mine = groups.entry(ou).or_default();
            if !mine.iter().any(|t| t.group == group.group) {
                mine.push(group.clone());
            }
        }
    }
    Ok(list.len())
}

pub fn run_locations(args: &Dhis2LocationsArgs, profile: &crate::profile::Profile) -> Result<()> {
    check_out(&args.out)?;
    let instance = Instance::new(&args.conn.instance, profile)?;
    if instance.admin_unit_type.is_none() || instance.facility_type.is_none() {
        return Err(PipesError::Usage(format!(
            "dhis2 locations needs [location] admin_unit_type and [organization] facility_type \
             in the profile, the type codes of admin units and facilities; {}",
            profile.lacks("[location] admin_unit_type or [organization] facility_type")
        )));
    }
    let prefix = args
        .id_prefix
        .clone()
        .unwrap_or_else(|| instance.name.clone());
    if !valid_name(&prefix, MAX_PREFIX_LEN) {
        return Err(PipesError::Usage(format!(
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
    // Each response object is dropped once parsed.
    let mut units: Vec<OrgUnit> = raw
        .into_iter()
        .filter_map(|v| OrgUnit::parse(&v, &mut report))
        .collect();
    if units.is_empty() {
        return Err(PipesError::Usage(match &args.root {
            Some(r) => format!("DHIS2 returned no org units at or below {r}"),
            None => "DHIS2 returned no org units; does this user have any assigned?".into(),
        }));
    }
    let deepest = units.iter().map(|u| u.level).max().unwrap_or(1);
    let facility_level = args.facility_level.unwrap_or(deepest);
    fill_missing_boundaries(&mut units, facility_level, &mut report);
    let mut groups: HashMap<String, Vec<GroupType>> = HashMap::new();
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
                return Err(PipesError::Usage(format!(
                    "DHIS2 has no org unit group set {set}"
                )));
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
                    // One type per group: a group in two sets (or a set
                    // given twice) adds the set to the group's codings.
                    let types = groups.entry(ou.to_string()).or_default();
                    match types.iter_mut().find(|t| t.group == gid) {
                        Some(t) if t.sets.contains(set) => {}
                        Some(t) => t.sets.push(set.clone()),
                        None => types.push(GroupType {
                            group: gid.to_string(),
                            name: gname.to_string(),
                            sets: vec![set.clone()],
                        }),
                    }
                }
            }
        }
    }
    let n_groups = add_org_unit_groups(&api, args, &mut groups)?;
    if n_groups > 0 {
        eprintln!("{n_groups} org unit groups");
    }
    let opts = Options {
        instance: &instance,
        prefix: &prefix,
        country_code: args.country_code.as_deref(),
        facility_level,
        groups,
    };
    // Written as built; the file is renamed into place at the end.
    let mut out = NdjsonWriter::create(&args.out)?;
    let mut organizations = 0usize;
    build(&units, &opts, &mut report, |resource| {
        if resource["resourceType"] == "Organization" {
            organizations += 1;
        }
        out.write(&resource)
    })?;
    let n = out.finish()?;

    let mut per_level: BTreeMap<u32, usize> = BTreeMap::new();
    for u in &units {
        *per_level.entry(u.level).or_default() += 1;
    }
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
    use base64::Engine;

    fn build_all(units: &[OrgUnit], opts: &Options, report: &mut Report) -> Vec<Value> {
        let mut out = Vec::new();
        build(units, opts, report, |r| {
            out.push(r);
            Ok(())
        })
        .unwrap();
        out
    }

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
            groups: HashMap::from([
                (
                    "f".to_string(),
                    vec![GroupType {
                        group: "uYxK4wmcPqA".to_string(),
                        name: "CHP".to_string(),
                        sets: vec!["J5jldMd8OHv".to_string()],
                    }],
                ),
                (
                    "d".to_string(),
                    vec![GroupType {
                        group: "hSgatV6qHFI".to_string(),
                        name: "Districts".to_string(),
                        sets: vec![],
                    }],
                ),
            ]),
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
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
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
        let out = build_all(&units, &opts(&instance), &mut report);
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
        assert!(
            systems.contains(&"https://icr.healthcampaigns.org/identifiers/national-admin-code")
        );
        assert_eq!(country["identifier"][0]["use"], "official");
        assert_eq!(country["identifier"][0]["value"], "c");
        assert_eq!(
            country["identifier"][0]["system"],
            instance.orgunit_system()
        );

        let district = &out[1];
        assert_eq!(district["partOf"]["reference"], "Location/sle-c");
        let ext = &district["extension"][0];
        assert_eq!(
            ext["url"],
            "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(ext["valueAttachment"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&decoded).unwrap(), poly);
        assert_eq!(district["identifier"].as_array().unwrap().len(), 2);
        // A group in no set is coded under the org-unit-group system.
        assert_eq!(
            district["type"][1],
            json!({"coding": [{"system": instance.group_system("org-unit-group"),
                               "code": "hSgatV6qHFI", "display": "Districts"}],
                   "text": "Districts"})
        );

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
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let mut closed = unit("d", 2, Some("gone"), None);
        closed.closed_date = Some("2020-01-01".into());
        let units = vec![unit("c", 1, None, None), closed];
        let mut report = Report::default();
        let out = build_all(&units, &opts(&instance), &mut report);
        assert_eq!(out[1]["status"], "inactive");
        assert!(out[1].get("partOf").is_none());
        assert_eq!(report.count("parent_not_exported"), 1);
    }

    #[test]
    fn a_subtree_root_has_no_partof_and_no_complaint() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
        let units = vec![unit("d", 2, Some("c"), None), unit("e", 3, Some("d"), None)];
        let mut report = Report::default();
        let out = build_all(&units, &opts(&instance), &mut report);
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
        let mp = polygons(g, "c", &mut report).unwrap();
        use geo::Area;
        assert!((mp.unsigned_area() - 2.0).abs() < 1e-9);
        // A unit whose only child is a facility point stays without a boundary.
        assert!(units[3].geometry.is_none());
        assert_eq!(report.count("boundary_from_children"), 1);
    }

    #[test]
    fn a_child_boundary_with_a_short_position_is_reported_not_a_panic() {
        let square = json!({"type":"Polygon","coordinates":[[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0],[0.0,0.0]]]});
        let short = json!({"type":"Polygon","coordinates":[[[1.0],[2,0],[2,2],[1.0]]]});
        let mut units = vec![
            unit("c", 1, None, None),
            unit("a", 2, Some("c"), Some(square)),
            unit("b", 2, Some("c"), Some(short)),
        ];
        let mut report = Report::default();
        fill_missing_boundaries(&mut units, 3, &mut report);
        let g = units[0].geometry.as_ref().expect("country filled");
        let mut r = Report::default();
        let mp = polygons(g, "c", &mut r).unwrap();
        use geo::Area;
        assert!((mp.unsigned_area() - 1.0).abs() < 1e-9);
        assert_eq!(report.count("bad_geometry"), 1);
        assert_eq!(report.count("boundary_from_children"), 1);
    }

    #[test]
    fn unusable_geometry_is_reported_and_left_out() {
        let instance = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
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
        let out = build_all(&units, &opts(&instance), &mut report);
        assert!(out[0].get("extension").is_none());
        assert!(out[1].get("position").is_none());
        assert_eq!(report.count("bad_geometry"), 2);
    }
}
