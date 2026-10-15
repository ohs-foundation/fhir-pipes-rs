//! One FHIR Location, flattened into the fields fhir-pipes models as columns, plus
//! the untouched resource JSON for the lossless `fhir_json` column.
//!
//! The fields are base FHIR. An implementation guide's extra columns, overlays
//! and spatial index come from the profile (crate::profile).

use base64::Engine;
use serde_json::Value;

use crate::report::Report;

pub const HL7_BOUNDARY_EXTENSION_URL: &str =
    "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson";
/// The name an implementation guide gives its own copy of the boundary
/// extension, as ICR's `…/StructureDefinition/location-boundary-geojson` does.
const BOUNDARY_EXTENSION_SUFFIX: &str = "/StructureDefinition/location-boundary-geojson";
pub const GEOJSON_CONTENT_TYPE: &str = "application/geo+json";

/// A boundary attachment extension: the HL7 one, one a profile names
/// (`extra`), or any IG's extension named `…/location-boundary-geojson`.
/// extract and the snapshot merge have no profile and pass none.
pub fn is_boundary_extension(url: &str, extra: &[String]) -> bool {
    url == HL7_BOUNDARY_EXTENSION_URL
        || url.ends_with(BOUNDARY_EXTENSION_SUFFIX)
        || extra.iter().any(|e| e == url)
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
    /// that was NOT captured into `boundary`, is kept verbatim.
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
    /// reporting) if it has no usable id.
    pub fn parse(
        resource: &Value,
        profile: &crate::profile::Profile,
        report: &mut Report,
    ) -> Option<Location> {
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

        if let Some(Value::Object(pos)) = obj.get("position") {
            match (
                pos.get("longitude").and_then(Value::as_f64),
                pos.get("latitude").and_then(Value::as_f64),
            ) {
                (Some(lon), Some(lat)) => loc.position = Some((lon, lat)),
                _ if pos.contains_key("longitude") || pos.contains_key("latitude") => {
                    report.add("malformed_field", &id, "position coordinates not numeric")
                }
                _ => {}
            }
        }

        // Index of the extension whose boundary was captured into
        // `loc.boundary` as `Boundary::Inline` -- the only extension entry
        // `fhir_json` removes below. A `Boundary::Url` extension, or a
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
                            Some(cell) => loc.spatial_cells.push(cell),
                            None => report.add(
                                "malformed_field",
                                &id,
                                "spatial-index extension lacks system, level or cell",
                            ),
                        }
                    }
                }
            }
            Some(_) => report.add("malformed_field", &id, "extension is not a list"),
        }

        // fhir_json: the resource minus the one boundary extension actually
        // captured above. Everything else, including any unusable or
        // not-yet-resolved boundary extension, is preserved verbatim.
        let mut stripped = resource.clone();
        if let Some(index) = captured_boundary_index
            && let Some(Value::Array(exts)) = stripped.get_mut("extension")
        {
            exts.remove(index);
            if exts.is_empty() {
                stripped.as_object_mut().unwrap().remove("extension");
            }
        }
        loc.fhir_json = stripped.to_string();
        Some(loc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Report;

    fn parse_str(s: &str, report: &mut Report) -> Option<Location> {
        Location::parse(
            &serde_json::from_str(s).unwrap(),
            &crate::profile::icr_example(),
            report,
        )
    }

    /// A profile column's value by name (ICR example profile).
    fn col<'a>(loc: &'a Location, name: &str) -> Option<&'a str> {
        let i = crate::profile::icr_example()
            .location
            .columns
            .iter()
            .position(|c| c.name == name)
            .expect("an ICR column");
        loc.columns[i].as_deref()
    }

    #[test]
    fn parses_the_common_fields() {
        let mut report = Report::default();
        let loc = parse_str(r#"{
            "resourceType":"Location","id":"clinic","name":"Gama Clinic","status":"active",
            "meta":{"versionId":"3","lastUpdated":"2026-01-02T03:04:05Z"},
            "alias":["GC"],"description":"d",
            "type":[{"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-location-type","code":"facility"}]},
                    {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs","code":"phc"}]},
                    {"coding":[{"system":"https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs","code":"public"}]}],
            "physicalType":{"coding":[{"code":"si"}]},
            "partOf":{"reference":"Location/gama"},
            "managingOrganization":{"reference":"Organization/org-1"},
            "identifier":[{"system":"https://icr.healthcampaigns.org/identifiers/pcode","value":"NG1"},
                          {"system":"https://icr.healthcampaigns.org/identifiers/overture-gers","value":"g1"}],
            "position":{"longitude":3.25,"latitude":6.25},
            "extension":[{"url":"https://icr.healthcampaigns.org/StructureDefinition/settlement-type","valueCode":"urban"},
                         {"url":"https://icr.healthcampaigns.org/StructureDefinition/delivery-strategy","valueCode":"fixed"},
                         {"url":"https://icr.healthcampaigns.org/StructureDefinition/overlays-admin-unit","valueReference":{"reference":"Location/kano"}}]
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
            {"url":"https://icr.healthcampaigns.org/StructureDefinition/overlays-admin-unit",
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
