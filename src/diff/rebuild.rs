//! Apply an edited row to a snapshot resource. Only the writable columns
//! touch the JSON; everything else on the resource is left exactly as the
//! snapshot had it, which is what makes the round trip lossless.

use geo::{CoordsIter, Geometry};
use serde_json::{Value, json};

use crate::diff::compare::{round_coord, round_geometry, wkb_key};
use crate::diff::input::{ColumnValue, IDENTIFIER_COLUMN, Identifier, InputRow, out_of_range};
use crate::fhir::edit::{
    Object, array_mut, identifier_system_of, set_concept_text, set_first_type_code,
    set_physical_type, set_reference, set_string, upsert_extension_code, upsert_identifier,
    upsert_type_coding, write_position,
};
use crate::fhir::location::{self, HL7_BOUNDARY_EXTENSION_URL, is_boundary_extension, position_of};
use crate::fhir::spatial::{refresh_cells, remove_all_cells};
use crate::fhir::{Boundary, Location};
use crate::geometry::{kind_name, parse_boundary, to_geojson_bytes, validity};
use crate::profile::{Column, Kind, Profile};
use crate::report::Report;

/// The resource as it should now be. `base` is the raw snapshot line, or
/// `{"resourceType":"Location","id":..}` for a create; `snapshot` is its
/// parsed form, used for the old geometry and position.
pub fn rebuild(
    base: &Value,
    snapshot: Option<&Location>,
    row: &InputRow,
    profile: &Profile,
    report: &mut Report,
) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_string();
    // The whole identifier list first, so identifier columns upsert into it.
    if let Some(v) = row.columns.get(IDENTIFIER_COLUMN) {
        apply_identifier_list(&mut obj, v);
    }
    for (name, value) in &row.columns {
        apply_column(&mut obj, name, value);
    }
    for column in &profile.location.columns {
        if let Some(value) = row.columns.get(&column.name) {
            apply_profile_column(&mut obj, column, text_of(value), &id, report);
        }
    }
    let position_before = position_of(&obj);
    apply_position(&mut obj, row, &id, report);
    let position_after = position_of(&obj);
    if position_after != position_before
        && let Some(url) = profile.location.spatial_index_extension.as_deref()
    {
        // Spatial-index cells are a function of the position: recompute the
        // ones already on the resource, or drop them all if it was cleared.
        match position_after {
            Some((lon, lat)) => {
                refresh_cells(&mut obj, url, lon, lat, &id, report);
            }
            None => remove_all_cells(&mut obj, url),
        }
    }
    if let Some(geom) = &row.geometry {
        apply_geometry(&mut obj, &id, snapshot, geom, profile, report);
    }
    Value::Object(obj)
}

pub(super) fn text_of(v: &ColumnValue) -> Option<&str> {
    match v {
        ColumnValue::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn apply_column(obj: &mut Object, name: &str, value: &ColumnValue) {
    let t = text_of(value);
    match name {
        "name" | "status" | "description" => set_string(obj, name, t),
        "type" => set_first_type_code(obj, t),
        "physical_type" => set_physical_type(obj, t),
        "part_of" => set_reference(obj, "partOf", "Location", t),
        "managing_organization" => set_reference(obj, "managingOrganization", "Organization", t),
        "alias" => match value {
            ColumnValue::TextList(items) => {
                obj.insert("alias".into(), json!(items));
            }
            ColumnValue::Null => {
                obj.remove("alias");
            }
            _ => {}
        },
        // identifier and position_* have their own passes in `rebuild`;
        // profile columns are applied by `apply_profile_column`.
        _ => {}
    }
}

/// Write one profile column's value back where the column reads it from.
pub(super) fn apply_profile_column(
    obj: &mut Object,
    column: &Column,
    value: Option<&str>,
    id: &str,
    report: &mut Report,
) {
    match column.kind {
        Kind::Identifier => {
            let system = identifier_system_of(obj, &column.systems);
            upsert_identifier(obj, &system, value)
        }
        Kind::ExtensionCode => upsert_extension_code(
            obj,
            column.url.as_deref().unwrap_or_default(),
            column.system.as_deref(),
            value,
        ),
        Kind::TypeCoding => {
            upsert_type_coding(obj, column.system.as_deref().unwrap_or_default(), value)
        }
        Kind::TypeText => set_concept_text(
            obj,
            column.system.as_deref().unwrap_or_default(),
            value,
            id,
            &column.name,
            report,
        ),
    }
}

pub(super) fn apply_identifier_list(obj: &mut Object, value: &ColumnValue) {
    match value {
        ColumnValue::Identifiers(items) => {
            let list: Vec<Value> = items.iter().map(identifier_json).collect();
            obj.insert("identifier".into(), Value::Array(list));
        }
        ColumnValue::Null => {
            obj.remove("identifier");
        }
        _ => {}
    }
}

fn identifier_json(i: &Identifier) -> Value {
    let mut o = Object::new();
    if let Some(s) = &i.system {
        o.insert("system".into(), json!(s));
    }
    if let Some(v) = &i.value {
        o.insert("value".into(), json!(v));
    }
    Value::Object(o)
}

/// Writes the rounded position unless it rounds to what is already there,
/// so a float round trip leaves the original numbers untouched.
fn set_position(obj: &mut Object, lon: f64, lat: f64) {
    let (lon, lat) = (round_coord(lon), round_coord(lat));
    if let Some((cx, cy)) = position_of(obj)
        && round_coord(cx) == lon
        && round_coord(cy) == lat
    {
        return;
    }
    write_position(obj, lon, lat);
}

/// `position_longitude` / `position_latitude`: a missing column keeps the
/// existing coordinate; both null clears the position; a single coordinate
/// with nothing to pair it with is reported and skipped.
fn apply_position(obj: &mut Object, row: &InputRow, id: &str, report: &mut Report) {
    let lon = row.columns.get("position_longitude");
    let lat = row.columns.get("position_latitude");
    if lon.is_none() && lat.is_none() {
        return;
    }
    let current = position_of(obj);
    let pick = |col: Option<&ColumnValue>, existing: Option<f64>| match col {
        None => existing,
        Some(ColumnValue::Number(n)) => Some(*n),
        Some(_) => None,
    };
    match (
        pick(lon, current.map(|p| p.0)),
        pick(lat, current.map(|p| p.1)),
    ) {
        (Some(x), Some(y)) => match out_of_range(x, y) {
            Some(why) => report.add(
                "input_column_type",
                id,
                &format!("position_longitude/position_latitude {why}; position not changed"),
            ),
            None => set_position(obj, x, y),
        },
        (None, None) => {
            obj.remove("position");
        }
        _ => report.add(
            "input_column_type",
            id,
            "position needs both position_longitude and position_latitude",
        ),
    }
}

/// The attachment as diff writes it: inline base64 of the bare GeoJSON
/// geometry with rounded coordinates, the shape the fixture and the
/// Python wrote.
pub fn boundary_attachment(geom: &Geometry<f64>) -> Value {
    location::boundary_attachment(&to_geojson_bytes(&round_geometry(geom)))
}

/// Replace the boundary attachment, or add one under the profile's first
/// boundary extension (the HL7 one without a profile).
fn replace_boundary(obj: &mut Object, geom: &Geometry<f64>, profile: &Profile) {
    let attachment = boundary_attachment(geom);
    let extra = &profile.location.boundary_extensions;
    let exts = array_mut(obj, "extension");
    let existing = exts.iter_mut().find(|e| {
        e.get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| is_boundary_extension(u, extra))
    });
    match existing {
        Some(Value::Object(e)) => {
            e.insert("valueAttachment".into(), attachment);
        }
        _ => {
            let url = extra
                .first()
                .map(String::as_str)
                .unwrap_or(HL7_BOUNDARY_EXTENSION_URL);
            exts.push(json!({"url": url, "valueAttachment": attachment}))
        }
    }
}

fn check_validity(geom: &Geometry<f64>, id: &str, report: &mut Report) {
    if let Err(reason) = validity::check(geom) {
        report.add("geometry_invalid", id, &format!("{reason}; written as is"));
    }
}

/// The geometry edits what it came from: the boundary attachment when the
/// snapshot resource has a decoded boundary, the position otherwise.
fn apply_geometry(
    obj: &mut Object,
    id: &str,
    snapshot: Option<&Location>,
    geom: &Geometry<f64>,
    profile: &Profile,
    report: &mut Report,
) {
    if let Some(why) = geom.coords_iter().find_map(|c| out_of_range(c.x, c.y)) {
        report.add(
            "geometry_out_of_range",
            id,
            &format!("{why}; geometry edit skipped"),
        );
        return;
    }
    let is_point = matches!(geom, Geometry::Point(_));
    let is_polygon = matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
    if !is_point && !is_polygon {
        report.add(
            "geometry_unparseable",
            id,
            &format!("unsupported geometry type {}", kind_name(geom)),
        );
        return;
    }
    let old_boundary = snapshot.and_then(|l| match &l.boundary {
        Some(Boundary::Inline(bytes)) => parse_boundary(bytes, id, &mut Report::default()),
        _ => None,
    });
    if let Some(old) = old_boundary {
        if is_point {
            report.add(
                "geometry_kind_changed",
                id,
                "a point was supplied for a Location with a boundary; geometry edit skipped",
            );
            return;
        }
        if wkb_key(&old) == wkb_key(geom) {
            return;
        }
        check_validity(geom, id, report);
        replace_boundary(obj, geom, profile);
        return;
    }
    if is_polygon {
        check_validity(geom, id, report);
        replace_boundary(obj, geom, profile);
        return;
    }
    let Geometry::Point(p) = geom else {
        unreachable!("is_point checked above")
    };
    let (x, y) = (round_coord(p.x()), round_coord(p.y()));
    // `apply_position` has already run. An edit is a change from the
    // snapshot: the geometry is applied only when it moved, so columns
    // edited next to an untouched point keep their edit. Stale but
    // unchanged columns (every GIS export carries them) are not an edit
    // and never report.
    let original = snapshot.and_then(|l| l.position);
    let rounded = |p: Option<(f64, f64)>| p.map(|(a, b)| (round_coord(a), round_coord(b)));
    if rounded(original) == Some((x, y)) {
        return;
    }
    let after_columns = position_of(obj);
    if rounded(after_columns) != rounded(original) && rounded(after_columns) != Some((x, y)) {
        report.add(
            "position_geometry_disagree",
            id,
            "position columns and the geometry both changed and differ; the geometry was used",
        );
    }
    write_position(obj, x, y);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use base64::Engine;
    use geo::{line_string, polygon};
    use serde_json::json;

    // ICR's URLs, as test data: the tests rebuild ICR-shaped resources with
    // the example ICR profile.
    const BOUNDARY_EXTENSION_URL: &str =
        "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson";

    const PCODE_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/pcode";

    const GERS_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/overture-gers";

    pub(crate) const NHFR_CODE_SYSTEM: &str =
        "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code";

    pub(crate) const NHFR_UID_SYSTEM: &str =
        "https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid";

    pub(crate) const FACILITY_TYPE_SYSTEM: &str =
        "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs";

    const OWNERSHIP_SYSTEM: &str = "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs";

    const SETTLEMENT_TYPE_EXTENSION_URL: &str =
        "https://icr.healthcampaigns.org/StructureDefinition/settlement-type";

    const SETTLEMENT_TYPE_SYSTEM: &str =
        "https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs";

    const DELIVERY_STRATEGY_EXTENSION_URL: &str = crate::profile::TEST_DELIVERY_STRATEGY_EXTENSION;

    pub(crate) fn icr() -> Profile {
        crate::profile::test_profile()
    }

    pub(crate) fn row(cols: &[(&str, ColumnValue)]) -> InputRow {
        InputRow {
            id: Some("a".into()),
            columns: cols
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            geometry: None,
            line: 1,
            unread: Vec::new(),
        }
    }

    pub(crate) fn text(s: &str) -> ColumnValue {
        ColumnValue::Text(s.into())
    }

    fn apply(base: Value, cols: &[(&str, ColumnValue)]) -> Value {
        let mut report = Report::default();
        let loc = Location::parse(&base, &icr(), &mut report);
        rebuild(&base, loc.as_ref(), &row(cols), &icr(), &mut report)
    }

    #[test]
    fn strings_set_and_null_clears_and_unknown_fields_survive() {
        let base = json!({"resourceType":"Location","id":"a","name":"Old","status":"active","meta":{"versionId":"3"},"mystery":{"k":1}});
        let out = apply(
            base,
            &[
                ("name", text("New")),
                ("status", ColumnValue::Null),
                ("description", text("d")),
            ],
        );
        assert_eq!(out["name"], "New");
        assert!(out.get("status").is_none());
        assert_eq!(out["description"], "d");
        assert_eq!(out["meta"]["versionId"], "3");
        assert_eq!(out["mystery"]["k"], 1);
    }

    #[test]
    fn alias_and_identifier_lists_are_replaced_whole() {
        let base = json!({"resourceType":"Location","id":"a","alias":["x"],"identifier":[{"system":"s","value":"1"},{"system":"t","value":"2"}]});
        let out = apply(
            base.clone(),
            &[
                ("alias", ColumnValue::TextList(vec!["y".into(), "z".into()])),
                (
                    "identifier",
                    ColumnValue::Identifiers(vec![Identifier {
                        system: Some("u".into()),
                        value: Some("3".into()),
                    }]),
                ),
            ],
        );
        assert_eq!(out["alias"], json!(["y", "z"]));
        assert_eq!(out["identifier"], json!([{"system":"u","value":"3"}]));
        let out = apply(
            base,
            &[
                ("alias", ColumnValue::Null),
                ("identifier", ColumnValue::Null),
            ],
        );
        assert!(out.get("alias").is_none());
        assert!(out.get("identifier").is_none());
    }

    #[test]
    fn pcode_upserts_into_the_identifier_list_after_it_is_replaced() {
        let base = json!({"resourceType":"Location","id":"a","identifier":[{"system":PCODE_SYSTEM,"value":"OLD"}]});
        let out = apply(base.clone(), &[("pcode", text("NG1"))]);
        assert_eq!(
            out["identifier"],
            json!([{"system":PCODE_SYSTEM,"value":"NG1"}])
        );
        let out = apply(
            base.clone(),
            &[
                (
                    "identifier",
                    ColumnValue::Identifiers(vec![Identifier {
                        system: Some("s".into()),
                        value: Some("v".into()),
                    }]),
                ),
                ("pcode", text("NG2")),
            ],
        );
        assert_eq!(
            out["identifier"],
            json!([{"system":"s","value":"v"},{"system":PCODE_SYSTEM,"value":"NG2"}])
        );
        let out = apply(
            base,
            &[("pcode", ColumnValue::Null), ("gers_id", text("g"))],
        );
        assert_eq!(
            out["identifier"],
            json!([{"system":GERS_SYSTEM,"value":"g"}])
        );
        let out = apply(
            json!({"resourceType":"Location","id":"a"}),
            &[("gers_id", ColumnValue::Null)],
        );
        assert!(out.get("identifier").is_none());
    }

    #[test]
    fn pcode_writes_back_to_the_national_admin_code_entry_when_that_is_what_the_row_has() {
        let nac = "https://icr.healthcampaigns.org/identifiers/national-admin-code";
        let base = json!({"resourceType":"Location","id":"nga","identifier":[{"system":nac,"value":"NGA"}]});
        let out = apply(base.clone(), &[("pcode", text("NG"))]);
        assert_eq!(out["identifier"], json!([{"system":nac,"value":"NG"}]));
        let out = apply(base, &[("pcode", ColumnValue::Null)]);
        assert!(out.get("identifier").is_none());
    }

    #[test]
    fn references_keep_their_prefix_and_default_to_the_type() {
        let base = json!({"resourceType":"Location","id":"a","partOf":{"reference":"Location/old","display":"Old"},"managingOrganization":{"reference":"http://h/fhir/Organization/o1"}});
        let out = apply(
            base,
            &[
                ("part_of", text("new")),
                ("managing_organization", text("o2")),
            ],
        );
        assert_eq!(out["partOf"]["reference"], "Location/new");
        assert_eq!(out["partOf"]["display"], "Old");
        assert_eq!(
            out["managingOrganization"]["reference"],
            "http://h/fhir/Organization/o2"
        );
        let out = apply(
            json!({"resourceType":"Location","id":"a"}),
            &[("part_of", text("p")), ("managing_organization", text("o"))],
        );
        assert_eq!(out["partOf"]["reference"], "Location/p");
        assert_eq!(out["managingOrganization"]["reference"], "Organization/o");
        let out = apply(
            json!({"resourceType":"Location","id":"a","partOf":{"reference":"Location/p"}}),
            &[("part_of", ColumnValue::Null)],
        );
        assert!(out.get("partOf").is_none());
    }

    #[test]
    fn type_edits_the_first_coding_code_and_keeps_its_system() {
        let base = json!({"resourceType":"Location","id":"a","type":[{"coding":[{"system":"sys","code":"facility"}]},{"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"phc"}]}]});
        let out = apply(base.clone(), &[("type", text("admin-unit"))]);
        assert_eq!(
            out["type"][0]["coding"][0],
            json!({"system":"sys","code":"admin-unit"})
        );
        assert_eq!(out["type"][1]["coding"][0]["code"], "phc");
        let out = apply(
            json!({"resourceType":"Location","id":"a"}),
            &[("type", text("site"))],
        );
        assert_eq!(out["type"], json!([{"coding":[{"code":"site"}]}]));
        let out = apply(base, &[("type", ColumnValue::Null)]);
        assert_eq!(
            out["type"],
            json!([{"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"phc"}]}])
        );
    }

    #[test]
    fn physical_type_sets_the_first_coding_or_removes_the_concept() {
        let out = apply(
            json!({"resourceType":"Location","id":"a"}),
            &[("physical_type", text("si"))],
        );
        assert_eq!(out["physicalType"], json!({"coding":[{"code":"si"}]}));
        let out = apply(out, &[("physical_type", ColumnValue::Null)]);
        assert!(out.get("physicalType").is_none());
    }

    #[test]
    fn an_unchanged_codeable_concept_extension_is_left_alone() {
        let ext = json!({"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCodeableConcept":{
            "coding":[{"system":"https://example.org/local","code":"village"},
                      {"system":SETTLEMENT_TYPE_SYSTEM,"code":"rural","display":"Rural"}],
            "text":"Rural village"}});
        let base = json!({"resourceType":"Location","id":"a","extension":[ext.clone()]});
        // The exported value written back as it was: nothing changes.
        let out = apply(base.clone(), &[("settlement_type", text("rural"))]);
        assert_eq!(out["extension"], json!([ext]));

        // Only another system's coding: the column reads null, and a null
        // written back does not delete the extension.
        let other = json!({"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCodeableConcept":{
            "coding":[{"system":"https://example.org/local","code":"village"}]}});
        let only_other = json!({"resourceType":"Location","id":"a","extension":[other.clone()]});
        let out = apply(only_other, &[("settlement_type", ColumnValue::Null)]);
        assert_eq!(out["extension"], json!([other]));

        // A changed code replaces only that system's coding; the text and
        // display described the old code and go.
        let out = apply(base.clone(), &[("settlement_type", text("hard-to-reach"))]);
        assert_eq!(
            out["extension"][0]["valueCodeableConcept"],
            json!({"coding":[{"system":"https://example.org/local","code":"village"},
                             {"system":SETTLEMENT_TYPE_SYSTEM,"code":"hard-to-reach"}]})
        );

        // Clearing removes that system's coding and keeps the rest.
        let out = apply(base, &[("settlement_type", ColumnValue::Null)]);
        assert_eq!(
            out["extension"][0]["valueCodeableConcept"]["coding"],
            json!([{"system":"https://example.org/local","code":"village"}])
        );
    }

    #[test]
    fn profile_extensions_upsert_by_url_and_clear() {
        // settlement_type is a CodeableConcept extension (ICR's IG);
        // delivery_strategy a plain valueCode one (the test profile).
        let base = json!({"resourceType":"Location","id":"a","extension":[
            {"url":"https://example.org/keep","valueString":"k"},
            {"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCodeableConcept":{
                "coding":[{"system":SETTLEMENT_TYPE_SYSTEM,"code":"rural","display":"Rural"}]}}]});
        let out = apply(
            base,
            &[
                ("settlement_type", text("urban")),
                ("delivery_strategy", text("fixed")),
            ],
        );
        // The stale display goes with the old code.
        assert_eq!(
            out["extension"][1],
            json!({"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCodeableConcept":{
                "coding":[{"system":SETTLEMENT_TYPE_SYSTEM,"code":"urban"}]}})
        );
        assert_eq!(
            out["extension"][2],
            json!({"url":DELIVERY_STRATEGY_EXTENSION_URL,"valueCode":"fixed"})
        );
        // A valueCode written by an older tool becomes the IG's shape.
        let old = json!({"resourceType":"Location","id":"a","extension":[
            {"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCode":"rural"}]});
        let out2 = apply(old, &[("settlement_type", text("urban"))]);
        assert_eq!(
            out2["extension"][0],
            json!({"url":SETTLEMENT_TYPE_EXTENSION_URL,"valueCodeableConcept":{
                "coding":[{"system":SETTLEMENT_TYPE_SYSTEM,"code":"urban"}]}})
        );
        let out = apply(
            out,
            &[
                ("settlement_type", ColumnValue::Null),
                ("delivery_strategy", ColumnValue::Null),
            ],
        );
        assert_eq!(
            out["extension"],
            json!([{"url":"https://example.org/keep","valueString":"k"}])
        );
        let out = apply(
            json!({"resourceType":"Location","id":"a"}),
            &[("settlement_type", ColumnValue::Null)],
        );
        assert!(out.get("extension").is_none());
    }

    #[test]
    fn facility_level_and_ownership_upsert_codings_by_system() {
        let base = json!({"resourceType":"Location","id":"a","type":[{"coding":[{"code":"facility"}]},{"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"phc","display":"PHC"}],"text":"Primary Health Centre"}]});
        // An unchanged code keeps its display.
        let same = apply(base.clone(), &[("facility_level", text("phc"))]);
        assert_eq!(same["type"][1]["coding"][0]["display"], "PHC");
        let out = apply(
            base,
            &[
                ("facility_level", text("hospital")),
                ("ownership", text("public")),
            ],
        );
        // A new code drops the old code's display; the concept text is its
        // own column (type_text) and stays.
        assert_eq!(
            out["type"][1],
            json!({"coding":[{"system":FACILITY_TYPE_SYSTEM,"code":"hospital"}],"text":"Primary Health Centre"})
        );
        assert_eq!(
            out["type"][2],
            json!({"coding":[{"system":OWNERSHIP_SYSTEM,"code":"public"}]})
        );
        let out = apply(
            out,
            &[
                ("facility_level", ColumnValue::Null),
                ("ownership", ColumnValue::Null),
            ],
        );
        assert_eq!(
            out["type"],
            json!([{"coding":[{"code":"facility"}]},{"text":"Primary Health Centre"}])
        );
    }

    #[test]
    fn position_columns_set_clear_and_ignore_float_noise() {
        let base = json!({"resourceType":"Location","id":"a","position":{"longitude":3.25,"latitude":6.25}});
        let out = apply(
            base.clone(),
            &[
                ("position_longitude", ColumnValue::Number(3.250000004)),
                ("position_latitude", ColumnValue::Number(6.25)),
            ],
        );
        assert_eq!(out["position"], json!({"longitude":3.25,"latitude":6.25}));
        let out = apply(
            base.clone(),
            &[("position_longitude", ColumnValue::Number(4.123456789))],
        );
        assert_eq!(
            out["position"],
            json!({"longitude":4.1234568,"latitude":6.25})
        );
        let out = apply(
            base.clone(),
            &[
                ("position_longitude", ColumnValue::Null),
                ("position_latitude", ColumnValue::Null),
            ],
        );
        assert!(out.get("position").is_none());

        let mut report = Report::default();
        let bare = json!({"resourceType":"Location","id":"a"});
        let out = rebuild(
            &bare,
            None,
            &row(&[("position_longitude", ColumnValue::Number(1.0))]),
            &icr(),
            &mut report,
        );
        assert!(out.get("position").is_none());
        assert_eq!(report.count("input_column_type"), 1);
        let out = rebuild(
            &bare,
            None,
            &row(&[
                ("position_longitude", ColumnValue::Number(1.0)),
                ("position_latitude", ColumnValue::Number(2.0)),
            ]),
            &icr(),
            &mut report,
        );
        assert_eq!(out["position"], json!({"longitude":1.0,"latitude":2.0}));
    }

    #[test]
    fn a_moved_position_skips_spatial_index_cells_with_an_out_of_range_level() {
        let url = icr().location.spatial_index_extension.unwrap();
        let cell = |scheme: &str, level: u64, cell: &str| {
            json!({"url": url, "extension": [
                {"url": "system", "valueCode": scheme},
                {"url": "level", "valueUnsignedInt": level},
                {"url": "cell", "valueString": cell},
            ]})
        };
        let bad = vec![
            cell("quadkey", 70, "0"),
            cell("quadkey", u64::from(u32::MAX), "0"),
            cell("quadkey", 0, ""),
            cell("geohash", 13, "s"),
        ];
        let mut extension = bad.clone();
        extension.push(cell(
            "quadkey",
            18,
            &crate::fhir::spatial::quadkey(3.25, 6.25, 18),
        ));
        let base = json!({"resourceType":"Location","id":"a","extension":extension,
            "position":{"longitude":3.25,"latitude":6.25}});
        let mut report = Report::default();
        let out = rebuild(
            &base,
            None,
            &row(&[
                ("position_longitude", ColumnValue::Number(9.9)),
                ("position_latitude", ColumnValue::Number(10.5)),
            ]),
            &icr(),
            &mut report,
        );
        let exts = out["extension"].as_array().unwrap();
        assert_eq!(
            &exts[..4],
            &bad[..],
            "malformed cells are left as they were"
        );
        assert_eq!(
            exts[4],
            cell("quadkey", 18, &crate::fhir::spatial::quadkey(9.9, 10.5, 18))
        );
        assert_eq!(report.count("malformed_field"), 4);
    }

    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[3,6],[4,6],[4,7],[3,7],[3,6]]]}"#;

    fn with_boundary(url: &str, geojson: &str) -> Value {
        let data = base64::engine::general_purpose::STANDARD.encode(geojson);
        json!({"resourceType":"Location","id":"a","name":"A","extension":[
            {"url":"https://example.org/keep","valueString":"k"},
            {"url":url,"valueAttachment":{"contentType":"application/geo+json","data":data}}]})
    }

    fn decoded_boundary(out: &Value) -> Value {
        let ext = out["extension"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| is_boundary_extension(e["url"].as_str().unwrap(), &[]))
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(ext["valueAttachment"]["data"].as_str().unwrap())
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn geom_row(geom: geo::Geometry<f64>, cols: &[(&str, ColumnValue)]) -> InputRow {
        let mut r = row(cols);
        r.geometry = Some(geom);
        r
    }

    fn square(dx: f64) -> geo::Geometry<f64> {
        geo::Geometry::Polygon(
            polygon![(x: 3.0 + dx, y: 6.0), (x: 4.0 + dx, y: 6.0), (x: 4.0 + dx, y: 7.0), (x: 3.0 + dx, y: 7.0), (x: 3.0 + dx, y: 6.0)],
        )
    }

    fn apply_geom(base: Value, r: &InputRow, report: &mut Report) -> Value {
        let loc = Location::parse(&base, &icr(), &mut Report::default());
        rebuild(&base, loc.as_ref(), r, &icr(), report)
    }

    #[test]
    fn an_unchanged_boundary_with_float_noise_is_not_an_edit() {
        let base = with_boundary(BOUNDARY_EXTENSION_URL, SQUARE);
        let mut report = Report::default();
        let out = apply_geom(
            base.clone(),
            &geom_row(square(0.00000001), &[]),
            &mut report,
        );
        assert_eq!(out, base);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn a_redrawn_boundary_replaces_the_attachment_and_keeps_the_extension_url() {
        let base = with_boundary(
            "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson",
            SQUARE,
        );
        let mut report = Report::default();
        let out = apply_geom(
            base,
            &geom_row(square(0.123456789), &[("name", text("B"))]),
            &mut report,
        );
        assert_eq!(out["name"], "B");
        assert_eq!(out["extension"][0]["url"], "https://example.org/keep");
        assert_eq!(
            out["extension"][1]["url"],
            "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson"
        );
        assert_eq!(
            out["extension"][1]["valueAttachment"]["contentType"],
            "application/geo+json"
        );
        let g = decoded_boundary(&out);
        assert_eq!(g["type"], "Polygon");
        assert_eq!(g["coordinates"][0][0][0], 3.1234568);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn a_point_on_a_boundary_row_is_reported_and_other_edits_still_apply() {
        let base = with_boundary(BOUNDARY_EXTENSION_URL, SQUARE);
        let mut report = Report::default();
        let out = apply_geom(
            base.clone(),
            &geom_row(
                geo::Geometry::Point(geo::Point::new(3.5, 6.5)),
                &[("name", text("B"))],
            ),
            &mut report,
        );
        assert_eq!(out["name"], "B");
        assert_eq!(out["extension"], base["extension"]);
        assert!(out.get("position").is_none());
        assert_eq!(report.count("geometry_kind_changed"), 1);
    }

    #[test]
    fn a_moved_point_updates_the_position_and_an_equal_one_keeps_the_original_numbers() {
        let base = json!({"resourceType":"Location","id":"a","position":{"longitude":3.25,"latitude":6.25}});
        let mut report = Report::default();
        let out = apply_geom(
            base.clone(),
            &geom_row(geo::Geometry::Point(geo::Point::new(3.3, 6.123456789)), &[]),
            &mut report,
        );
        assert_eq!(
            out["position"],
            json!({"longitude":3.3,"latitude":6.1234568})
        );
        assert!(out.get("extension").is_none());
        let out = apply_geom(
            base.clone(),
            &geom_row(
                geo::Geometry::Point(geo::Point::new(3.250000004, 6.25)),
                &[],
            ),
            &mut report,
        );
        assert_eq!(out, base);
        assert!(report.counts().is_empty());
    }

    #[test]
    fn the_geometry_wins_only_when_it_was_edited_too() {
        let base = json!({"resourceType":"Location","id":"a","position":{"longitude":3.25,"latitude":6.25}});
        let mut report = Report::default();
        let cols = [
            ("position_longitude", ColumnValue::Number(9.0)),
            ("position_latitude", ColumnValue::Number(9.0)),
        ];
        // Only the columns were edited: the untouched point does not undo it.
        let out = apply_geom(
            base.clone(),
            &geom_row(geo::Geometry::Point(geo::Point::new(3.25, 6.25)), &cols),
            &mut report,
        );
        assert_eq!(out["position"], json!({"longitude":9.0,"latitude":9.0}));
        assert_eq!(report.count("position_geometry_disagree"), 0);
        // Both edited, to different places: the geometry wins.
        let out = apply_geom(
            base.clone(),
            &geom_row(geo::Geometry::Point(geo::Point::new(4.0, 5.0)), &cols),
            &mut report,
        );
        assert_eq!(out["position"], json!({"longitude":4.0,"latitude":5.0}));
        assert_eq!(report.count("position_geometry_disagree"), 1);
        // Both edited, to the same place: no report.
        let same = [
            ("position_longitude", ColumnValue::Number(4.0)),
            ("position_latitude", ColumnValue::Number(5.0)),
        ];
        let out = apply_geom(
            base.clone(),
            &geom_row(geo::Geometry::Point(geo::Point::new(4.0, 5.0)), &same),
            &mut report,
        );
        assert_eq!(out["position"], json!({"longitude":4.0,"latitude":5.0}));
        assert_eq!(report.count("position_geometry_disagree"), 1);
        // Nothing edited: the snapshot's exact numbers stay.
        let out = apply_geom(
            base.clone(),
            &geom_row(geo::Geometry::Point(geo::Point::new(3.25, 6.25)), &[]),
            &mut report,
        );
        assert_eq!(out, base);

        // Stale but unchanged columns next to a moved geometry: a plain move, no report.
        let stale = [
            ("position_longitude", ColumnValue::Number(3.25)),
            ("position_latitude", ColumnValue::Number(6.25)),
        ];
        let out = apply_geom(
            base,
            &geom_row(geo::Geometry::Point(geo::Point::new(4.0, 5.0)), &stale),
            &mut report,
        );
        assert_eq!(out["position"], json!({"longitude":4.0,"latitude":5.0}));
        assert_eq!(report.count("position_geometry_disagree"), 1);
    }

    #[test]
    fn coordinates_that_are_not_degrees_are_reported_and_skipped() {
        let base = json!({"resourceType":"Location","id":"a","position":{"longitude":3.25,"latitude":6.25}});
        let mut report = Report::default();
        // Web Mercator metres in the geometry.
        let out = apply_geom(
            base.clone(),
            &geom_row(
                geo::Geometry::Point(geo::Point::new(361_793.0, 699_440.0)),
                &[],
            ),
            &mut report,
        );
        assert_eq!(out, base);
        assert_eq!(report.count("geometry_out_of_range"), 1);
        // And in the position columns.
        let cols = [
            ("position_longitude", ColumnValue::Number(361_793.0)),
            ("position_latitude", ColumnValue::Number(699_440.0)),
        ];
        let out = rebuild(&base, None, &row(&cols), &icr(), &mut report);
        assert_eq!(out["position"], base["position"]);
        assert_eq!(report.count("input_column_type"), 1);
    }

    #[test]
    fn a_polygon_on_a_point_row_adds_a_boundary() {
        let base =
            json!({"resourceType":"Location","id":"a","position":{"longitude":3.5,"latitude":6.5}});
        let mut report = Report::default();
        let out = apply_geom(base, &geom_row(square(0.0), &[]), &mut report);
        assert_eq!(out["position"], json!({"longitude":3.5,"latitude":6.5}));
        assert_eq!(out["extension"][0]["url"], BOUNDARY_EXTENSION_URL);
        assert_eq!(decoded_boundary(&out)["type"], "Polygon");
        assert!(report.counts().is_empty());
    }

    #[test]
    fn unsupported_and_invalid_geometries_are_reported() {
        let base = json!({"resourceType":"Location","id":"a"});
        let mut report = Report::default();
        let line = geo::Geometry::LineString(line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)]);
        let out = apply_geom(base.clone(), &geom_row(line, &[]), &mut report);
        assert!(out.get("extension").is_none());
        assert_eq!(report.count("geometry_unparseable"), 1);

        let bowtie = geo::Geometry::Polygon(
            polygon![(x: 0.0, y: 0.0), (x: 2.0, y: 2.0), (x: 2.0, y: 0.0), (x: 0.0, y: 2.0), (x: 0.0, y: 0.0)],
        );
        let out = apply_geom(base, &geom_row(bowtie, &[]), &mut report);
        assert_eq!(decoded_boundary(&out)["type"], "Polygon", "written as is");
        assert_eq!(report.count("geometry_invalid"), 1);
    }
}
