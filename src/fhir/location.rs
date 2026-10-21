//! One FHIR Location, flattened into the fields fhir-pipes models as columns, plus
//! the untouched resource JSON for the lossless `fhir_json` column.
//!
//! The fields are base FHIR. An implementation guide's extra columns, overlays
//! and spatial index come from the profile (crate::profile).

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::report::Report;

pub const HL7_BOUNDARY_EXTENSION_URL: &str =
    "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson";
/// The name an implementation guide gives its own copy of the boundary
/// extension, as ICR's `…/StructureDefinition/location-boundary-geojson` does.
const BOUNDARY_EXTENSION_SUFFIX: &str = "/StructureDefinition/location-boundary-geojson";
pub const GEOJSON_CONTENT_TYPE: &str = "application/geo+json";
pub const PHYSICAL_TYPE_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/location-physical-type";

/// A boundary attachment: the GeoJSON inlined as base64.
pub fn boundary_attachment(geojson: &[u8]) -> Value {
    json!({
        "contentType": GEOJSON_CONTENT_TYPE,
        "data": base64::engine::general_purpose::STANDARD.encode(geojson),
    })
}

/// A boundary attachment extension under `url`, the GeoJSON inlined as base64.
pub fn boundary_extension(url: &str, geojson: &[u8]) -> Value {
    json!({"url": url, "valueAttachment": boundary_attachment(geojson)})
}

/// A boundary attachment extension: the HL7 one, one a profile names
/// (`extra`), or any IG's extension named `…/location-boundary-geojson`.
/// extract and the snapshot merge have no profile and pass none.
pub fn is_boundary_extension(url: &str, extra: &[String]) -> bool {
    url == HL7_BOUNDARY_EXTENSION_URL
        || url.ends_with(BOUNDARY_EXTENSION_SUFFIX)
        || extra.iter().any(|e| e == url)
}

/// `{"reference": "<resource_type>/<id>"}`.
pub fn reference(resource_type: &str, id: &str) -> Value {
    json!({"reference": format!("{resource_type}/{id}")})
}

/// `{"system": .., "value": ..}`.
/// A Location's `position` as (longitude, latitude), when both are numbers.
pub fn position_of(obj: &Map<String, Value>) -> Option<(f64, f64)> {
    let p = obj.get("position")?.as_object()?;
    Some((p.get("longitude")?.as_f64()?, p.get("latitude")?.as_f64()?))
}

pub fn identifier(system: &str, value: &str) -> Value {
    json!({"system": system, "value": value})
}

/// The Location physical types fhir-pipes writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalType {
    /// `si`: a facility, school or other point.
    Site,
    /// `jdn`: an admin unit.
    Jurisdiction,
}

impl PhysicalType {
    pub fn concept(self) -> Value {
        let (code, display) = match self {
            PhysicalType::Site => ("si", "Site"),
            PhysicalType::Jurisdiction => ("jdn", "Jurisdiction"),
        };
        json!({"coding": [{"system": PHYSICAL_TYPE_SYSTEM, "code": code, "display": display}]})
    }
}

/// The top-level keys of a `NewLocation` after `resourceType` and `id`.
/// Each writer keeps its own key order, so its output stays byte for byte
/// what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKey {
    Meta,
    Extension,
    Identifier,
    Status,
    Name,
    Alias,
    Type,
    PhysicalType,
    Position,
    ManagingOrganization,
    PartOf,
}

/// A Location fhir-pipes writes from scratch (`import`, `dhis2 locations`).
/// Empty lists and `None` are left out.
#[derive(Debug, Default)]
pub struct NewLocation<'a> {
    pub id: &'a str,
    pub meta: Option<Value>,
    pub extension: Vec<Value>,
    pub identifier: Vec<Value>,
    pub status: &'a str,
    pub name: &'a str,
    pub alias: &'a [String],
    /// The first type: one coding, `type_code` under `type_system`.
    pub type_system: &'a str,
    pub type_code: &'a str,
    /// Further types after the first.
    pub extra_types: Vec<Value>,
    pub physical_type: Option<PhysicalType>,
    /// (longitude, latitude).
    pub position: Option<(f64, f64)>,
    /// An Organization id.
    pub managing_organization: Option<&'a str>,
    /// A Location id.
    pub part_of: Option<&'a str>,
}

impl NewLocation<'_> {
    /// The resource, its keys in `order`.
    pub fn into_json(self, order: &[LocationKey; 11]) -> Value {
        let first_type = json!({"coding": [{"system": self.type_system, "code": self.type_code}]});
        let types = std::iter::once(first_type)
            .chain(self.extra_types)
            .collect();
        let position = self
            .position
            .map(|(longitude, latitude)| json!({"longitude": longitude, "latitude": latitude}));
        let fields = vec![
            (LocationKey::Meta, "meta", self.meta),
            (
                LocationKey::Extension,
                "extension",
                non_empty(self.extension),
            ),
            (
                LocationKey::Identifier,
                "identifier",
                non_empty(self.identifier),
            ),
            (LocationKey::Status, "status", Some(json!(self.status))),
            (LocationKey::Name, "name", Some(json!(self.name))),
            (
                LocationKey::Alias,
                "alias",
                (!self.alias.is_empty()).then(|| json!(self.alias)),
            ),
            (LocationKey::Type, "type", Some(Value::Array(types))),
            (
                LocationKey::PhysicalType,
                "physicalType",
                self.physical_type.map(PhysicalType::concept),
            ),
            (LocationKey::Position, "position", position),
            (
                LocationKey::ManagingOrganization,
                "managingOrganization",
                self.managing_organization
                    .map(|id| reference("Organization", id)),
            ),
            (
                LocationKey::PartOf,
                "partOf",
                self.part_of.map(|id| reference("Location", id)),
            ),
        ];
        resource("Location", self.id, fields, order)
    }
}

/// A list, or `None` when it is empty.
pub(crate) fn non_empty(list: Vec<Value>) -> Option<Value> {
    (!list.is_empty()).then_some(Value::Array(list))
}

/// `resourceType`, `id`, then the fields with a value in `order`. Every
/// field's key must be in `order` once.
pub(crate) fn resource<K: PartialEq + std::fmt::Debug>(
    resource_type: &str,
    id: &str,
    mut fields: Vec<(K, &str, Option<Value>)>,
    order: &[K],
) -> Value {
    let mut r = Map::new();
    r.insert("resourceType".into(), json!(resource_type));
    r.insert("id".into(), json!(id));
    for key in order {
        if let Some(i) = fields.iter().position(|(k, ..)| k == key) {
            let (_, name, value) = fields.swap_remove(i);
            if let Some(value) = value {
                r.insert(name.into(), value);
            }
        }
    }
    debug_assert!(
        fields.is_empty(),
        "{resource_type} keys missing from {order:?}"
    );
    Value::Object(r)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    pub system: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// Decoded GeoJSON bytes from an inline `data` attachment.
    Inline(Vec<u8>),
    /// A `url` attachment that `extract` has not resolved.
    Url(String),
}

#[derive(Debug, Clone, Default)]
pub struct Location {
    pub id: String,
    pub version_id: Option<String>,
    pub last_updated: Option<String>,
    pub name: Option<String>,
    pub alias: Vec<String>,
    pub status: Option<String>,
    pub description: Option<String>,
    pub type_code: Option<String>,
    /// Every code under the profile's location type systems (any system
    /// without a profile), from any concept and coding in `type`, whatever
    /// their order. `population --type` matches these.
    pub location_types: Vec<String>,
    pub physical_type: Option<String>,
    pub part_of: Option<String>,
    pub managing_organization: Option<String>,
    pub identifier: Vec<Identifier>,
    /// The profile's Location columns, in profile order.
    pub columns: Vec<Option<String>>,
    /// The admin code: the profile's `admin_code_column`, if it has one.
    pub admin_code: Option<String>,
    /// From the profile's overlays extension.
    pub overlays_admin_unit_ids: Vec<String>,
    /// Spatial index cells (quadkey / h3 / geohash + level) from the
    /// profile's spatial index extension; derived from `position` by fhir-pipes.
    pub spatial_cells: Vec<crate::fhir::spatial::SpatialCell>,
    /// (longitude, latitude), FHIR order.
    pub position: Option<(f64, f64)>,
    pub boundary: Option<Boundary>,
    /// The resource as JSON (key order preserved) with the captured boundary
    /// extension removed; everything else, including any boundary extension
    /// that was NOT captured into `boundary`, is kept verbatim. Filled only
    /// by [`Location::parse_owned`]; empty after [`Location::parse`].
    pub fhir_json: String,
}

pub(crate) fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `Location/loc-1` -> `loc-1`; bare ids pass through. An empty reference, or
/// one ending in `/`, has no usable id and yields `None`.
pub(crate) fn strip_reference(reference: &str) -> Option<String> {
    let last = reference.rsplit('/').next().unwrap_or(reference);
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

fn reference_id(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    id: &str,
    report: &mut Report,
) -> Option<String> {
    match obj.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::Object(r)) => match r.get("reference").and_then(Value::as_str) {
            Some(reference) => {
                let stripped = strip_reference(reference);
                if stripped.is_none() {
                    report.add("malformed_field", id, &format!("{key}.reference is empty"));
                }
                stripped
            }
            None => None,
        },
        Some(_) => {
            report.add("malformed_field", id, &format!("{key} is not an object"));
            None
        }
    }
}

/// First coding.code of a CodeableConcept, or of the first concept in a list.
fn first_coding_code(node: Option<&Value>) -> Option<String> {
    let concept = match node? {
        Value::Array(items) => items.first()?,
        other => other,
    };
    concept
        .get("coding")?
        .as_array()?
        .first()?
        .get("code")?
        .as_str()
        .map(str::to_string)
}

/// Every coding.code under one of `systems` (under any system when `systems`
/// is empty), across a CodeableConcept or a list of them, in document order.
pub(crate) fn codes_in_systems(node: Option<&Value>, systems: &[String]) -> Vec<String> {
    let concepts: Vec<&Value> = match node {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    };
    concepts
        .into_iter()
        .filter_map(|c| c.get("coding")?.as_array())
        .flatten()
        .filter(|coding| {
            coding
                .get("system")
                .and_then(Value::as_str)
                .is_some_and(|s| systems.is_empty() || systems.iter().any(|x| x == s))
        })
        .filter_map(|coding| coding.get("code")?.as_str().map(str::to_string))
        .collect()
}

/// First coding under `system` across a CodeableConcept or list of them.
pub(crate) fn coding_code_by_system(node: Option<&Value>, system: &str) -> Option<String> {
    let concepts: Vec<&Value> = match node? {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for concept in concepts {
        for coding in concept
            .get("coding")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if coding.get("system").and_then(Value::as_str) == Some(system) {
                return coding
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    None
}

fn read_boundary(
    ext: &serde_json::Map<String, Value>,
    id: &str,
    report: &mut Report,
) -> Option<Boundary> {
    let Some(Value::Object(att)) = ext.get("valueAttachment") else {
        report.add(
            "malformed_field",
            id,
            "boundary extension valueAttachment is not an object",
        );
        return None;
    };
    let content_type = att.get("contentType").and_then(Value::as_str);
    if content_type != Some(GEOJSON_CONTENT_TYPE) {
        report.add(
            "boundary_bad_content_type",
            id,
            &format!("expected {GEOJSON_CONTENT_TYPE}, got {content_type:?}"),
        );
        return None;
    }
    if let Some(data) = att
        .get("data")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return match base64::engine::general_purpose::STANDARD.decode(data) {
            Ok(bytes) => Some(Boundary::Inline(bytes)),
            Err(err) => {
                report.add("boundary_bad_base64", id, &err.to_string());
                None
            }
        };
    }
    if let Some(url) = att
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(Boundary::Url(url.to_string()));
    }
    report.add("boundary_empty", id, "attachment has neither data nor url");
    None
}

impl Location {
    /// Flatten one resource, with `profile`'s columns. Returns None (after
    /// reporting) if it has no usable id. `fhir_json` is left empty: most
    /// readers need only the fields, and building it copies the resource.
    pub fn parse(
        resource: &Value,
        profile: &crate::profile::Profile,
        report: &mut Report,
    ) -> Option<Location> {
        Self::parse_fields(resource, profile, report).map(|(loc, _)| loc)
    }

    /// As [`Location::parse`], with `fhir_json` too. The resource is taken
    /// by value so the captured boundary extension is removed in place
    /// rather than from a copy.
    pub fn parse_owned(
        mut resource: Value,
        profile: &crate::profile::Profile,
        report: &mut Report,
    ) -> Option<Location> {
        let (mut loc, captured_boundary_index) = Self::parse_fields(&resource, profile, report)?;
        // fhir_json: the resource minus the one boundary extension actually
        // captured. Everything else, including any unusable or
        // not-yet-resolved boundary extension, is preserved verbatim.
        if let Some(index) = captured_boundary_index
            && let Value::Object(obj) = &mut resource
            && let Some(Value::Array(exts)) = obj.get_mut("extension")
        {
            exts.remove(index);
            if exts.is_empty() {
                obj.remove("extension");
            }
        }
        loc.fhir_json = resource.to_string();
        Some(loc)
    }

    /// The fields, and the index of the extension whose boundary was
    /// captured as `Boundary::Inline`.
    fn parse_fields(
        resource: &Value,
        profile: &crate::profile::Profile,
        report: &mut Report,
    ) -> Option<(Location, Option<usize>)> {
        let Value::Object(obj) = resource else {
            report.add(
                "malformed_field",
                "<unknown>",
                "Location resource is not an object",
            );
            return None;
        };
        let id = match obj.get("id") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::String(_)) | None | Some(Value::Null) => {
                report.add("missing_id", "<unknown>", "Location resource has no id");
                return None;
            }
            Some(other) => {
                report.add(
                    "malformed_field",
                    "<unknown>",
                    &format!("Location id is not a string: {other}"),
                );
                return None;
            }
        };

        if let Some(rt) = obj.get("resourceType").and_then(Value::as_str)
            && rt != "Location"
        {
            report.add(
                "malformed_field",
                &id,
                &format!("resourceType is {rt}, not Location"),
            );
            return None;
        }

        let mut loc = Location {
            id: id.clone(),
            ..Default::default()
        };
        loc.name = str_field(obj, "name");
        loc.status = str_field(obj, "status");
        loc.description = str_field(obj, "description");
        match obj.get("alias") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(s) => loc.alias.push(s.to_string()),
                        None => report.add(
                            "malformed_field",
                            &id,
                            &format!("alias[{i}] is not a string"),
                        ),
                    }
                }
            }
            Some(_) => report.add("malformed_field", &id, "alias is not a list"),
        }
        loc.type_code = first_coding_code(obj.get("type"));
        loc.location_types = codes_in_systems(obj.get("type"), &profile.location.type_systems);
        loc.physical_type = first_coding_code(obj.get("physicalType"));
        loc.part_of = reference_id(obj, "partOf", &id, report);
        loc.managing_organization = reference_id(obj, "managingOrganization", &id, report);

        match obj.get("meta") {
            None | Some(Value::Null) => {}
            Some(Value::Object(meta)) => {
                loc.version_id = str_field(meta, "versionId");
                loc.last_updated = str_field(meta, "lastUpdated");
            }
            Some(_) => report.add("malformed_field", &id, "meta is not an object"),
        }

        match obj.get("identifier") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Value::Object(ident) => loc.identifier.push(Identifier {
                            system: str_field(ident, "system"),
                            value: str_field(ident, "value"),
                        }),
                        _ => report.add(
                            "malformed_field",
                            &id,
                            &format!("identifier[{i}] is not an object"),
                        ),
                    }
                }
            }
            Some(_) => report.add("malformed_field", &id, "identifier is not a list"),
        }
        loc.columns = profile
            .location
            .columns
            .iter()
            .map(|c| c.read(obj, &loc.identifier))
            .collect();
        loc.admin_code = profile
            .admin_code_index()
            .and_then(|i| loc.columns[i].clone());

        loc.position = position_of(obj);
        if loc.position.is_none()
            && let Some(Value::Object(pos)) = obj.get("position")
            && (pos.contains_key("longitude") || pos.contains_key("latitude"))
        {
            report.add("malformed_field", &id, "position coordinates not numeric")
        }

        // Index of the extension whose boundary was captured into
        // `loc.boundary` as `Boundary::Inline` -- the only extension entry
        // `fhir_json` removes (`parse_owned`). A `Boundary::Url` extension, or a
        // boundary extension that failed to parse, stays in `fhir_json`
        // untouched: the geometry column carries only what was decoded.
        let mut captured_boundary_index: Option<usize> = None;

        match obj.get("extension") {
            None | Some(Value::Null) => {}
            Some(Value::Array(exts)) => {
                for (i, ext) in exts.iter().enumerate() {
                    let Value::Object(e) = ext else {
                        report.add("malformed_field", &id, "extension entry is not an object");
                        continue;
                    };
                    let url = e.get("url").and_then(Value::as_str).unwrap_or("");
                    if is_boundary_extension(url, &profile.location.boundary_extensions) {
                        if loc.boundary.is_none() {
                            let boundary = read_boundary(e, &id, report);
                            if let Some(Boundary::Inline(_)) = &boundary {
                                captured_boundary_index = Some(i);
                            }
                            loc.boundary = boundary;
                        }
                    } else if Some(url) == profile.location.overlays_extension.as_deref() {
                        match e.get("valueReference") {
                            Some(Value::Object(r)) => {
                                if let Some(reference) = r.get("reference").and_then(Value::as_str)
                                {
                                    match strip_reference(reference) {
                                        Some(target) => loc.overlays_admin_unit_ids.push(target),
                                        None => report.add(
                                            "malformed_field",
                                            &id,
                                            "overlays extension reference is empty",
                                        ),
                                    }
                                }
                            }
                            Some(_) => report.add(
                                "malformed_field",
                                &id,
                                "overlays extension valueReference is not an object",
                            ),
                            None => {}
                        }
                    } else if Some(url) == profile.location.spatial_index_extension.as_deref() {
                        match crate::fhir::spatial::cell_from_extension(e, url) {
                            Some(Ok(cell)) => loc.spatial_cells.push(cell),
                            Some(Err(reason)) => report.add("malformed_field", &id, &reason),
                            None => {}
                        }
                    }
                }
            }
            Some(_) => report.add("malformed_field", &id, "extension is not a list"),
        }

        Some((loc, captured_boundary_index))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn location_types_are_read_from_any_coding_under_the_profile_systems() {
        let v = serde_json::json!([
            {"coding": [{"system": "https://example.test/local", "code": "zone"}]},
            {"coding": [
                {"system": "https://example.test/local", "code": "facility-catchment"},
                {"system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs", "code": "settlement-catchment"}
            ]},
            {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type", "code": "school"}]}
        ]);
        let icr = crate::profile::icr_example().location.type_systems;
        assert_eq!(
            codes_in_systems(Some(&v), &icr),
            ["settlement-catchment", "school"]
        );
        assert!(codes_in_systems(None, &icr).is_empty());
        // Without a profile's systems, any system.
        assert_eq!(codes_in_systems(Some(&v), &[]).len(), 4);
    }

    use super::*;

    #[test]
    fn a_new_location_has_its_keys_in_the_given_order_and_leaves_out_empty_ones() {
        use LocationKey::*;
        let full = || NewLocation {
            id: "a",
            meta: Some(json!({"profile": ["p"]})),
            extension: vec![json!({"url": "e"})],
            identifier: vec![identifier("s", "v")],
            status: "active",
            name: "A",
            alias: &[],
            type_system: "t",
            type_code: "site",
            extra_types: vec![json!({"text": "x"})],
            physical_type: Some(super::PhysicalType::Site),
            position: Some((1.5, 2.0)),
            managing_organization: Some("org-a"),
            part_of: Some("b"),
        };
        let order = [
            PartOf,
            Name,
            Meta,
            Status,
            Alias,
            Type,
            PhysicalType,
            Identifier,
            Position,
            ManagingOrganization,
            Extension,
        ];
        assert_eq!(
            full().into_json(&order).to_string(),
            r#"{"resourceType":"Location","id":"a","partOf":{"reference":"Location/b"},"name":"A","meta":{"profile":["p"]},"status":"active","type":[{"coding":[{"system":"t","code":"site"}]},{"text":"x"}],"physicalType":{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/location-physical-type","code":"si","display":"Site"}]},"identifier":[{"system":"s","value":"v"}],"position":{"longitude":1.5,"latitude":2.0},"managingOrganization":{"reference":"Organization/org-a"},"extension":[{"url":"e"}]}"#
        );
        let bare = NewLocation {
            id: "a",
            status: "active",
            name: "A",
            type_system: "t",
            type_code: "site",
            ..Default::default()
        };
        assert_eq!(
            bare.into_json(&order).to_string(),
            r#"{"resourceType":"Location","id":"a","name":"A","status":"active","type":[{"coding":[{"system":"t","code":"site"}]}]}"#
        );
    }
    use crate::report::Report;

    fn parse_str(s: &str, report: &mut Report) -> Option<Location> {
        Location::parse_owned(
            serde_json::from_str(s).unwrap(),
            &crate::profile::test_profile(),
            report,
        )
    }

    /// A profile column's value by name (the test profile).
    fn col<'a>(loc: &'a Location, name: &str) -> Option<&'a str> {
        let i = crate::profile::test_profile()
            .location
            .columns
            .iter()
            .position(|c| c.name == name)
            .expect("a test profile column");
        loc.columns[i].as_deref()
    }

    #[test]
    fn parses_the_common_fields() {
        let mut report = Report::default();
        let loc = parse_str(r#"{
            "resourceType":"Location","id":"clinic","name":"Gama Clinic","status":"active",
            "meta":{"versionId":"3","lastUpdated":"2026-01-02T03:04:05Z"},
            "alias":["GC"],"description":"d",
            "type":[{"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs","code":"facility"}]},
                    {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs","code":"phc"}]},
                    {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs","code":"public"}]}],
            "physicalType":{"coding":[{"code":"si"}]},
            "partOf":{"reference":"Location/gama"},
            "managingOrganization":{"reference":"Organization/org-1"},
            "identifier":[{"system":"https://icr.healthcampaigns.org/identifiers/pcode","value":"NG1"},
                          {"system":"https://icr.healthcampaigns.org/identifiers/overture-gers","value":"g1"}],
            "position":{"longitude":3.25,"latitude":6.25},
            "extension":[{"url":"https://icr.healthcampaigns.org/StructureDefinition/settlement-type",
                          "valueCodeableConcept":{"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs","code":"urban"}]}},
                         {"url":"https://example.org/StructureDefinition/delivery-strategy","valueCode":"fixed"},
                         {"url":"https://example.org/StructureDefinition/overlays-admin-unit","valueReference":{"reference":"Location/kano"}}]
        }"#, &mut report).unwrap();
        assert_eq!(loc.id, "clinic");
        assert_eq!(loc.version_id.as_deref(), Some("3"));
        assert_eq!(loc.last_updated.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(loc.alias, vec!["GC"]);
        assert_eq!(loc.type_code.as_deref(), Some("facility"));
        assert_eq!(col(&loc, "facility_level"), Some("phc"));
        assert_eq!(col(&loc, "ownership"), Some("public"));
        assert_eq!(loc.physical_type.as_deref(), Some("si"));
        assert_eq!(loc.part_of.as_deref(), Some("gama"));
        assert_eq!(loc.managing_organization.as_deref(), Some("org-1"));
        assert_eq!(col(&loc, "pcode"), Some("NG1"));
        assert_eq!(col(&loc, "gers_id"), Some("g1"));
        assert_eq!(loc.identifier.len(), 2);
        assert_eq!(loc.position, Some((3.25, 6.25)));
        assert_eq!(col(&loc, "settlement_type"), Some("urban"));
        assert_eq!(col(&loc, "delivery_strategy"), Some("fixed"));
        assert_eq!(loc.overlays_admin_unit_ids, vec!["kano"]);
        assert!(loc.boundary.is_none());
        assert_eq!(report.counts().len(), 0);
    }

    #[test]
    fn pcode_falls_back_to_the_national_admin_code_system() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"nga","identifier":[
            {"system":"https://icr.healthcampaigns.org/identifiers/national-admin-code","value":"NGA"}]}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(col(&loc, "pcode"), Some("NGA"));

        // Both present: the pcode system wins whatever the order.
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"x","identifier":[
            {"system":"https://icr.healthcampaigns.org/identifiers/national-admin-code","value":"NAC"},
            {"system":"https://icr.healthcampaigns.org/identifiers/pcode","value":"PC"}]}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(col(&loc, "pcode"), Some("PC"));
    }

    #[test]
    fn inline_boundary_is_decoded_and_stripped_from_fhir_json() {
        let mut report = Report::default();
        let geojson = r#"{"type":"Point","coordinates":[1,2]}"#;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, geojson);
        let src = format!(
            r#"{{"resourceType":"Location","id":"a","extension":[
            {{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{{"contentType":"application/geo+json","data":"{b64}"}}}},
            {{"url":"https://example.org/keep","valueString":"x"}}]}}"#
        );
        let loc = parse_str(&src, &mut report).unwrap();
        match &loc.boundary {
            Some(Boundary::Inline(bytes)) => assert_eq!(bytes, geojson.as_bytes()),
            other => panic!("expected inline boundary, got {other:?}"),
        }
        let json: serde_json::Value = serde_json::from_str(&loc.fhir_json).unwrap();
        let exts = json["extension"].as_array().unwrap();
        assert_eq!(exts.len(), 1, "boundary extension removed, other kept");
        assert_eq!(exts[0]["url"], "https://example.org/keep");
    }

    #[test]
    fn parse_gives_the_same_fields_without_fhir_json() {
        let geojson = r#"{"type":"Point","coordinates":[1,2]}"#;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, geojson);
        let src = format!(
            r#"{{"resourceType":"Location","id":"a","name":"A","extension":[
            {{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{{"contentType":"application/geo+json","data":"{b64}"}}}}]}}"#
        );
        let value: Value = serde_json::from_str(&src).unwrap();
        let profile = crate::profile::test_profile();
        let (mut r1, mut r2) = (Report::default(), Report::default());
        let fields = Location::parse(&value, &profile, &mut r1).unwrap();
        let mut owned = Location::parse_owned(value, &profile, &mut r2).unwrap();
        assert!(fields.fhir_json.is_empty());
        assert_eq!(
            owned.fhir_json,
            r#"{"resourceType":"Location","id":"a","name":"A"}"#
        );
        owned.fhir_json.clear();
        assert_eq!(format!("{fields:?}"), format!("{owned:?}"));
        assert_eq!(r1.counts(), r2.counts());
    }

    #[test]
    fn a_spatial_index_level_out_of_range_is_reported_and_skipped() {
        let mut report = Report::default();
        let cell = |level: u64| {
            format!(
                r#"{{"url":"https://icr.healthcampaigns.org/StructureDefinition/spatial-index","extension":[
                {{"url":"system","valueCode":"quadkey"}},{{"url":"level","valueUnsignedInt":{level}}},
                {{"url":"cell","valueString":"0"}}]}}"#
            )
        };
        let src = format!(
            r#"{{"resourceType":"Location","id":"a","extension":[{},{}]}}"#,
            cell(70),
            cell(1)
        );
        let loc = parse_str(&src, &mut report).unwrap();
        assert_eq!(loc.spatial_cells.len(), 1);
        assert_eq!(loc.spatial_cells[0].level, 1);
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn url_boundary_and_bad_shapes_are_reported() {
        let mut report = Report::default();
        let loc = parse_str(r#"{"resourceType":"Location","id":"a","identifier":"nope","partOf":"nope",
            "extension":[{"url":"http://hl7.org/fhir/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{"contentType":"application/geo+json","url":"https://files/x.geojson"}}]}"#, &mut report).unwrap();
        assert_eq!(
            loc.boundary,
            Some(Boundary::Url("https://files/x.geojson".into()))
        );
        assert_eq!(report.count("malformed_field"), 2);
    }

    #[test]
    fn non_location_resources_are_rejected() {
        let mut report = Report::default();
        assert!(parse_str(r#"{"resourceType":"Patient","id":"a"}"#, &mut report).is_none());
        assert_eq!(report.count("malformed_field"), 1);
        let detail = &report.issues[0].detail;
        assert_eq!(detail, "resourceType is Patient, not Location");

        // A missing resourceType is tolerated.
        let mut report = Report::default();
        assert!(parse_str(r#"{"id":"a"}"#, &mut report).is_some());
        assert_eq!(report.count("malformed_field"), 0);
    }

    #[test]
    fn missing_id_is_reported_and_skipped() {
        let mut report = Report::default();
        assert!(parse_str(r#"{"resourceType":"Location","name":"x"}"#, &mut report).is_none());
        assert_eq!(report.count("missing_id"), 1);
        assert!(parse_str(r#"[1,2]"#, &mut report).is_none());
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn bad_content_type_and_bad_base64_are_reported() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"a","extension":[
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
             "valueAttachment":{"contentType":"text/plain","data":"eA=="}}]}"#,
            &mut report,
        )
        .unwrap();
        assert!(loc.boundary.is_none());
        assert_eq!(report.count("boundary_bad_content_type"), 1);
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"b","extension":[
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
             "valueAttachment":{"contentType":"application/geo+json","data":"!!!"}}]}"#,
            &mut report,
        )
        .unwrap();
        assert!(loc.boundary.is_none());
        assert_eq!(report.count("boundary_bad_base64"), 1);
    }

    #[test]
    fn url_boundary_survives_in_fhir_json() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"a","extension":[
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
             "valueAttachment":{"contentType":"application/geo+json","url":"https://files/x.geojson"}}]}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(
            loc.boundary,
            Some(Boundary::Url("https://files/x.geojson".into()))
        );
        let json: serde_json::Value = serde_json::from_str(&loc.fhir_json).unwrap();
        let exts = json["extension"].as_array().unwrap();
        assert_eq!(exts.len(), 1, "url boundary extension is kept in fhir_json");
        assert_eq!(
            exts[0]["url"],
            "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson"
        );
    }

    #[test]
    fn bad_boundary_is_kept_and_second_boundary_is_used() {
        let mut report = Report::default();
        let geojson = r#"{"type":"Point","coordinates":[1,2]}"#;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, geojson);
        let src = format!(
            r#"{{"resourceType":"Location","id":"a","extension":[
            {{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{{"contentType":"application/geo+json","data":"!!!"}}}},
            {{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{{"contentType":"application/geo+json","data":"{b64}"}}}}]}}"#
        );
        let loc = parse_str(&src, &mut report).unwrap();
        match &loc.boundary {
            Some(Boundary::Inline(bytes)) => assert_eq!(bytes, geojson.as_bytes()),
            other => panic!("expected inline boundary, got {other:?}"),
        }
        let json: serde_json::Value = serde_json::from_str(&loc.fhir_json).unwrap();
        let exts = json["extension"].as_array().unwrap();
        assert_eq!(exts.len(), 1, "only the bad boundary extension remains");
        assert_eq!(exts[0]["valueAttachment"]["data"], "!!!");
        assert_eq!(report.count("boundary_bad_base64"), 1);
    }

    #[test]
    fn alias_shapes_are_reported() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"a","alias":"notalist"}"#,
            &mut report,
        )
        .unwrap();
        assert!(loc.alias.is_empty());
        assert_eq!(report.count("malformed_field"), 1);

        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"b","alias":["x",1,null]}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(loc.alias, vec!["x".to_string()]);
        assert_eq!(report.count("malformed_field"), 2);
    }

    #[test]
    fn position_with_string_numbers_is_reported() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"a","position":{"longitude":"3.25","latitude":6.25}}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(loc.position, None);
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn empty_references_are_reported() {
        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"a","partOf":{"reference":""}}"#,
            &mut report,
        )
        .unwrap();
        assert_eq!(loc.part_of, None);
        assert_eq!(report.count("malformed_field"), 1);

        let mut report = Report::default();
        let loc = parse_str(
            r#"{"resourceType":"Location","id":"b","extension":[
            {"url":"https://example.org/StructureDefinition/overlays-admin-unit",
             "valueReference":{"reference":"Location/"}}]}"#,
            &mut report,
        )
        .unwrap();
        assert!(loc.overlays_admin_unit_ids.is_empty());
        assert_eq!(report.count("malformed_field"), 1);
    }

    #[test]
    fn fhir_json_has_no_extension_key_when_only_boundary() {
        let mut report = Report::default();
        let geojson = r#"{"type":"Point","coordinates":[1,2]}"#;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, geojson);
        let src = format!(
            r#"{{"resourceType":"Location","id":"a","extension":[
            {{"url":"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
              "valueAttachment":{{"contentType":"application/geo+json","data":"{b64}"}}}}]}}"#
        );
        let loc = parse_str(&src, &mut report).unwrap();
        let json: serde_json::Value = serde_json::from_str(&loc.fhir_json).unwrap();
        assert!(json.as_object().unwrap().get("extension").is_none());

        let mut report = Report::default();
        let loc = parse_str(r#"{"resourceType":"Location","id":"b"}"#, &mut report).unwrap();
        let json: serde_json::Value = serde_json::from_str(&loc.fhir_json).unwrap();
        assert!(json.as_object().unwrap().get("extension").is_none());
    }
}
