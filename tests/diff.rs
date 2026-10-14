//! Binary-level tests for `fhir-pipes diff` against a small hand-made snapshot.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use base64::Engine;
use serde_json::{json, Value};

const BOUNDARY_EXT: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson";
const PCODE: &str = "https://icr.healthcampaigns.org/identifiers/pcode";
const NHFR_CODE: &str = "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code";
const FACILITY_TYPE: &str = "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs";
const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[3,6],[9,6],[9,12],[3,12],[3,6]]]}"#;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn decode(s: &str) -> Value {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn ng() -> Value {
    json!({
        "resourceType": "Location", "id": "ng",
        "meta": {"versionId": "1", "lastUpdated": "2026-01-01T00:00:00Z"},
        "name": "Nigeria", "status": "active",
        "type": [{"coding": [{"code": "admin-unit"}]}],
        "identifier": [{"system": PCODE, "value": "NG"}],
        "extension": [
            {"url": "https://example.org/keep", "valueString": "k"},
            {"url": BOUNDARY_EXT, "valueAttachment": {"contentType": "application/geo+json", "data": b64(SQUARE)}}
        ]
    })
}

fn clinic() -> Value {
    json!({
        "resourceType": "Location", "id": "clinic",
        "meta": {"versionId": "2"},
        "name": "Gama Clinic", "status": "active",
        "type": [{"coding": [{"code": "facility"}]}],
        "partOf": {"reference": "Location/ng"},
        "managingOrganization": {"reference": "Organization/org-clinic"},
        "position": {"longitude": 3.25, "latitude": 6.25}
    })
}

fn org_clinic() -> Value {
    json!({
        "resourceType": "Organization", "id": "org-clinic", "active": true, "name": "Gama Clinic",
        "meta": {"versionId": "9"},
        "identifier": [{"system": NHFR_CODE, "value": "05/08/1"}],
        "type": [
            {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/organization-type", "code": "prov"}]},
            {"coding": [{"system": FACILITY_TYPE, "code": "primary"}], "text": "Health Post"}
        ]
    })
}

/// Writes the two-resource snapshot and returns its directory.
fn snapshot(dir: &Path) -> PathBuf {
    let snap = dir.join("snapshot");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(
        snap.join("locations.ndjson"),
        format!("{}\n{}\n", ng(), clinic()),
    )
    .unwrap();
    std::fs::write(
        snap.join("organizations.ndjson"),
        format!("{}\n", org_clinic()),
    )
    .unwrap();
    snap
}

fn feature(props: Value, geometry: Value) -> Value {
    json!({"type": "Feature", "properties": props, "geometry": geometry})
}

fn collection(features: Vec<Value>) -> String {
    json!({"type": "FeatureCollection", "features": features}).to_string()
}

fn square_geometry(noise: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[3.0 + noise, 6], [9, 6], [9, 12], [3, 12], [3.0 + noise, 6]]]})
}

fn point(lon: f64, lat: f64) -> Value {
    json!({"type": "Point", "coordinates": [lon, lat]})
}

/// The rows a GIS export of the snapshot would carry, unedited, with float
/// noise in the geometry and stale derived columns.
fn export() -> Vec<Value> {
    vec![
        feature(
            json!({"id": "ng", "name": "Nigeria", "status": "active", "type": "admin-unit", "pcode": "NG",
                   "identifier": "[{\"system\":\"https://icr.healthcampaigns.org/identifiers/pcode\",\"value\":\"NG\"}]",
                   "admin0_name": "Nigeria", "tier": "0", "version_id": "1", "part_of": null,
                   "position_longitude": null, "position_latitude": null}),
            square_geometry(0.00000001),
        ),
        feature(
            json!({"id": "clinic", "name": "Gama Clinic", "status": "active", "type": "facility",
                   "part_of": "ng", "admin0_name": "Nigeria", "tier": "site", "version_id": "2",
                   "position_longitude": 3.25, "position_latitude": 6.25,
                   "nhfr_code": "05/08/1", "nhfr_uid": null, "facility_level": null,
                   "facility_level_text": "Health Post", "ownership_text": null,
                   "organization_identifier": "[{\"system\":\"https://icr.healthcampaigns.org/identifiers/nga-nhfr-code\",\"value\":\"05/08/1\"}]"}),
            point(3.250000001, 6.25),
        ),
    ]
}

fn write_input(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn diff(snap: &Path, input: &Path, out: &Path) -> Assert {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["diff", "--snapshot"])
        .arg(snap)
        .arg("--in")
        .arg(input)
        .arg("--out")
        .arg(out)
        .arg("--report")
        .arg(out.with_extension("report.json"))
        .assert()
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn changes(out: &Path) -> Vec<Value> {
    std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn report(out: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(out.with_extension("report.json")).unwrap())
        .unwrap()
}

#[test]
fn an_unedited_export_yields_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let input = write_input(dir.path(), "edits.geojson", &collection(export()));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("0 changed, 2 unchanged, 0 new"),
        "{}",
        stdout(&a)
    );
    assert_eq!(changes(&out).len(), 0);
    assert_eq!(report(&out)["counts"], json!({}));
}

#[test]
fn a_rename_emits_one_complete_resource_with_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["properties"]["name"] = json!("Gama PHC");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("1 changed, 1 unchanged, 0 new"),
        "{}",
        stdout(&a)
    );
    assert!(
        stdout(&a).contains("Location: 1, Organization: 1"),
        "{}",
        stdout(&a)
    );
    let got = changes(&out);
    assert_eq!(got.len(), 2, "the Organization first, then the Location");
    assert_eq!(got[0]["resourceType"], "Organization");
    assert_eq!(got[0]["id"], "org-clinic");
    assert_eq!(got[0]["name"], "Gama PHC");
    assert_eq!(got[0]["meta"]["versionId"], "9");
    let r = &got[1];
    assert_eq!(r["id"], "clinic");
    assert_eq!(r["name"], "Gama PHC");
    assert_eq!(r["meta"]["versionId"], "2");
    assert_eq!(r["partOf"]["reference"], "Location/ng");
    assert_eq!(r["position"], json!({"longitude": 3.25, "latitude": 6.25}));
    assert_eq!(r["type"], clinic()["type"]);
}

#[test]
fn stale_derived_columns_never_cause_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["properties"]["admin0_name"] = json!("Wrong");
    rows[1]["properties"]["path"] = json!("/x/y");
    rows[1]["properties"]["tier"] = json!("4");
    rows[1]["properties"]["version_id"] = json!("99");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("0 changed, 2 unchanged, 0 new"),
        "{}",
        stdout(&a)
    );
}

#[test]
fn null_clears_lists_round_trip_and_unknown_content_survives() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[0]["properties"]["status"] = Value::Null;
    rows[0]["properties"]["alias"] = json!(["Naija"]);
    rows[0]["properties"]["identifier"] =
        json!(format!("[{{\"system\":\"{PCODE}\",\"value\":\"NG2\"}}]"));
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    let r = &got[0];
    assert_eq!(r["id"], "ng");
    assert!(r.get("status").is_none());
    assert_eq!(r["alias"], json!(["Naija"]));
    assert_eq!(
        r["identifier"],
        json!([{"system": PCODE, "value": "NG"}]),
        "pcode column, still NG, upserts after the identifier list is replaced"
    );
    assert_eq!(
        r["extension"][0],
        json!({"url": "https://example.org/keep", "valueString": "k"})
    );
    assert_eq!(
        r["extension"][1]["valueAttachment"]["data"],
        b64(SQUARE),
        "boundary bytes untouched"
    );
    assert_eq!(r["meta"]["versionId"], "1");
}

#[test]
fn usage_errors_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let out = dir.path().join("changes.ndjson");
    let csv = write_input(dir.path(), "edits.csv", "id,name\n");
    diff(&snap, &csv, &out)
        .failure()
        .code(2)
        .stderr(predicates::str::contains(".parquet"));
    let input = write_input(dir.path(), "edits.geojson", &collection(export()));
    diff(Path::new("/nonexistent"), &input, &out)
        .failure()
        .code(2)
        .stderr(predicates::str::contains("locations.ndjson"));
    diff(&snap, Path::new("/nonexistent/edits.geojson"), &out)
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--in"));
    diff(&snap, &input, dir.path())
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--out"));
}

#[test]
fn a_failed_diff_leaves_no_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let input = write_input(dir.path(), "edits.geojson", "[1, 2]");
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).failure().code(2);
    assert!(!out.exists());
    assert!(!dir.path().join("changes.ndjson.tmp").exists());
}

#[test]
fn moving_a_point_updates_the_position() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["geometry"] = point(3.3, 6.123456789);
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0]["position"],
        json!({"longitude": 3.3, "latitude": 6.1234568})
    );
    assert!(got[0].get("extension").is_none());
    assert_eq!(
        report(&out)["counts"],
        json!({}),
        "stale position columns are not a disagreement"
    );
}

#[test]
fn drawing_a_polygon_on_a_point_row_adds_a_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["geometry"] =
        json!({"type": "Polygon", "coordinates": [[[3, 6], [4, 6], [4, 7], [3, 7], [3, 6]]]});
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    let r = &got[0];
    assert_eq!(r["position"], json!({"longitude": 3.25, "latitude": 6.25}));
    assert_eq!(r["extension"][0]["url"], BOUNDARY_EXT);
    assert_eq!(
        r["extension"][0]["valueAttachment"]["contentType"],
        "application/geo+json"
    );
    let g = decode(
        r["extension"][0]["valueAttachment"]["data"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(g["type"], "Polygon");
    assert_eq!(g["coordinates"][0].as_array().unwrap().len(), 5);
}

#[test]
fn a_point_on_a_boundary_row_is_reported_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[0]["geometry"] = point(5.0, 8.0);
    rows[0]["properties"]["name"] = json!("Federal Republic of Nigeria");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["name"], "Federal Republic of Nigeria");
    assert_eq!(
        got[0]["extension"][1]["valueAttachment"]["data"],
        b64(SQUARE)
    );
    assert!(got[0].get("position").is_none());
    assert_eq!(report(&out)["counts"]["geometry_kind_changed"], 1);
}

#[test]
fn a_redrawn_boundary_replaces_the_attachment_with_rounded_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[0]["geometry"] = square_geometry(0.123456789);
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    let r = &got[0];
    assert_eq!(r["extension"][0]["url"], "https://example.org/keep");
    assert_eq!(r["extension"][1]["url"], BOUNDARY_EXT);
    let g = decode(
        r["extension"][1]["valueAttachment"]["data"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(g["coordinates"][0][0][0], 3.1234568);
    assert_eq!(r["meta"]["versionId"], "1");
}

#[test]
fn new_rows_become_creates_with_and_without_an_id() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let rows = vec![
        feature(
            json!({"id": "newsite", "name": "New Site", "part_of": "ng", "type": "facility"}),
            point(4.0, 7.0),
        ),
        feature(json!({"name": "Nameless", "pcode": "NG9"}), point(4.5, 7.5)),
    ];
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("0 changed, 0 unchanged, 2 new"),
        "{}",
        stdout(&a)
    );
    let got = changes(&out);
    assert_eq!(got.len(), 3, "a facility creates its Organization pair");
    assert_eq!(got[0]["resourceType"], "Organization");
    assert_eq!(got[0]["id"], "org-newsite");
    assert_eq!(got[0]["active"], true);
    assert_eq!(got[0]["name"], "New Site");
    assert_eq!(got[1]["id"], "newsite");
    assert_eq!(got[1]["resourceType"], "Location");
    assert!(got[1].get("meta").is_none());
    assert_eq!(got[1]["partOf"]["reference"], "Location/ng");
    assert_eq!(
        got[1]["managingOrganization"]["reference"],
        "Organization/org-newsite"
    );
    assert_eq!(got[1]["type"], json!([{"coding": [{"code": "facility"}]}]));
    assert_eq!(
        got[1]["position"],
        json!({"longitude": 4.0, "latitude": 7.0})
    );
    let generated = got[2]["id"].as_str().unwrap();
    assert_eq!(generated.len(), 36, "a UUID: {generated}");
    assert_eq!(
        got[2]["identifier"],
        json!([{"system": PCODE, "value": "NG9"}])
    );
    let counts = &report(&out)["counts"];
    assert_eq!(counts["new_location"], 1);
    assert_eq!(counts["new_location_generated_id"], 1);
}

#[test]
fn duplicate_input_rows_are_reported_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    let mut again = rows[1].clone();
    again["properties"]["name"] = json!("Second");
    rows[1]["properties"]["name"] = json!("First");
    rows.push(again);
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 2);
    assert_eq!(got[1]["resourceType"], "Location");
    assert_eq!(got[1]["name"], "First");
    assert_eq!(report(&out)["counts"]["duplicate_id"], 1);
}

#[test]
fn feature_lines_input_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["properties"]["name"] = json!("Gama PHC");
    let text = rows
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let input = write_input(dir.path(), "edits.geojsonl", &text);
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("1 changed, 1 unchanged, 0 new"),
        "{}",
        stdout(&a)
    );
}

fn parquet_files(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            parquet_files(&path, into);
        } else if path.extension().is_some_and(|e| e == "parquet") {
            into.push(path);
        }
    }
}

#[test]
fn geoparquet_written_by_transform_round_trips_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let dataset = dir.path().join("out");
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(&snap)
        .arg("--out")
        .arg(&dataset)
        .assert()
        .success();
    let mut files = Vec::new();
    parquet_files(&dataset.join("locations"), &mut files);
    assert_eq!(
        files.len(),
        2,
        "one polygon and one point partition: {files:?}"
    );
    for (i, file) in files.iter().enumerate() {
        let out = dir.path().join(format!("changes-{i}.ndjson"));
        let a = diff(&snap, file, &out).success();
        assert!(
            stdout(&a).contains("0 changed, 1 unchanged, 0 new"),
            "{}: {}",
            file.display(),
            stdout(&a)
        );
        assert_eq!(report(&out)["counts"], json!({}), "{}", file.display());
    }
}

#[test]
fn nhfr_code_edits_touch_the_organization_only() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["properties"]["nhfr_code"] = json!("05/08/2");
    rows[1]["properties"]["facility_level_text"] = json!("Clinic");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(
        stdout(&a).contains("Location: 0, Organization: 1"),
        "{}",
        stdout(&a)
    );
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["resourceType"], "Organization");
    assert_eq!(got[0]["identifier"][0]["value"], "05/08/2");
    assert_eq!(got[0]["type"][1]["text"], "Clinic");
    assert_eq!(
        got[0]["type"][1]["coding"][0]["code"], "primary",
        "an unedited facility_level never reaches the Organization"
    );
}

#[test]
fn retiring_a_facility_deactivates_its_organization() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let mut rows = export();
    rows[1]["properties"]["status"] = json!("inactive");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["active"], false);
    assert_eq!(got[1]["status"], "inactive");
}

#[test]
fn a_new_settlement_row_stays_a_single_resource() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    let rows = vec![feature(
        json!({"id": "village", "name": "Village", "type": "settlement", "part_of": "ng"}),
        point(4.0, 7.0),
    )];
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["resourceType"], "Location");
    assert!(got[0].get("managingOrganization").is_none());
}

#[test]
fn without_an_organizations_file_diff_is_location_only() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    std::fs::remove_file(snap.join("organizations.ndjson")).unwrap();
    let mut rows = export();
    rows[1]["properties"]["name"] = json!("Gama PHC");
    let input = write_input(dir.path(), "edits.geojson", &collection(rows));
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &input, &out).success();
    assert!(String::from_utf8_lossy(&a.get_output().stderr).contains("no organizations.ndjson"));
    let got = changes(&out);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["resourceType"], "Location");
}

#[test]
fn a_position_edit_recomputes_the_spatial_index_cells() {
    const SPATIAL_EXT: &str = "https://icr.healthcampaigns.org/StructureDefinition/spatial-index";
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("snapshot");
    std::fs::create_dir_all(&snap).unwrap();
    // The clinic at (3.25, 6.25) with a stale-looking but well-formed zoom-18 cell.
    let mut clinic = clinic();
    clinic["extension"] = json!([{"url": SPATIAL_EXT, "extension": [
        {"url": "system", "valueCode": "quadkey"},
        {"url": "level", "valueUnsignedInt": 18},
        {"url": "cell", "valueString": "122222222222222222"}]}]);
    std::fs::write(
        snap.join("locations.ndjson"),
        format!("{}\n{}\n", ng(), clinic),
    )
    .unwrap();

    // Move the clinic north-east; the cell must follow the point.
    let edits = collection(vec![feature(
        json!({"id": "clinic", "name": "Gama Clinic", "status": "active", "type": "facility",
               "part_of": "ng", "version_id": "2",
               "position_longitude": 3.4, "position_latitude": 6.5}),
        point(3.4, 6.5),
    )]);
    let input = write_input(dir.path(), "moved.geojson", &edits);
    let out = dir.path().join("changes.ndjson");
    diff(&snap, &input, &out).success();
    let (moved,) = match changes(&out).as_slice() {
        [one] => (one.clone(),),
        other => panic!("expected one change, got {}", other.len()),
    };
    assert_eq!(moved["position"]["longitude"], 3.4);
    let ext = moved["extension"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["url"] == SPATIAL_EXT)
        .expect("cell kept");
    let cell = ext["extension"][2]["valueString"].as_str().unwrap();
    assert_ne!(cell, "122222222222222222", "recomputed for the new point");
    assert_eq!(cell.len(), 18);
    assert!(
        cell.starts_with('1'),
        "lon > 0, lat > 0 is quadrant 1 at zoom 1: {cell}"
    );
}

/// WKB for a point, and for a polygon with one ring.
fn wkb_point(x: f64, y: f64) -> Vec<u8> {
    let mut b = vec![1u8];
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&x.to_le_bytes());
    b.extend_from_slice(&y.to_le_bytes());
    b
}

fn wkb_polygon(ring: &[(f64, f64)]) -> Vec<u8> {
    let mut b = vec![1u8];
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&(ring.len() as u32).to_le_bytes());
    for (x, y) in ring {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    b
}

/// Copy `from` to `to`, replacing the geometry of the rows named in `set`,
/// as a GIS edit of the transform output would.
fn set_geometry(from: &Path, to: &Path, set: &[(&str, Vec<u8>)]) {
    use arrow_array::{Array, ArrayRef, BinaryArray, RecordBatch, StringArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let builder =
        ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(from).unwrap()).unwrap();
    let kv = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned();
    let schema = builder.schema().clone();
    let reader = builder.build().unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(to).unwrap(), schema, None).unwrap();
    for batch in reader {
        let batch = batch.unwrap();
        let ids = batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .clone();
        let old = batch
            .column_by_name("geometry")
            .unwrap()
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap()
            .clone();
        let new: BinaryArray = (0..batch.num_rows())
            .map(|r| match set.iter().find(|(id, _)| *id == ids.value(r)) {
                Some((_, wkb)) => Some(wkb.clone()),
                None => (!old.is_null(r)).then(|| old.value(r).to_vec()),
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|v| v.as_deref())
            .collect();
        let (i, _) = batch.schema().column_with_name("geometry").unwrap();
        let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
        columns[i] = Arc::new(new);
        writer
            .write(&RecordBatch::try_new(batch.schema(), columns).unwrap())
            .unwrap();
    }
    for kv in kv.unwrap_or_default() {
        writer.append_key_value_metadata(kv);
    }
    writer.close().unwrap();
}

#[test]
fn geometry_added_to_a_row_written_without_geometry_is_a_geometry_addition() {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(dir.path());
    // Two Locations with no position and no boundary.
    let ward = json!({
        "resourceType": "Location", "id": "ward", "meta": {"versionId": "1"},
        "name": "Ward", "status": "active",
        "type": [{"coding": [{"code": "admin-unit"}]}],
        "partOf": {"reference": "Location/ng"}
    });
    let site = json!({
        "resourceType": "Location", "id": "site", "meta": {"versionId": "1"},
        "name": "Site", "status": "active",
        "type": [{"coding": [{"code": "admin-unit"}]}],
        "partOf": {"reference": "Location/ng"}
    });
    std::fs::write(
        snap.join("locations.ndjson"),
        format!("{}\n{}\n{ward}\n{site}\n", ng(), clinic()),
    )
    .unwrap();
    let dataset = dir.path().join("out");
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["transform", "--keep-without-geometry", "--snapshot"])
        .arg(&snap)
        .arg("--out")
        .arg(&dataset)
        .assert()
        .success();
    let null_file =
        dataset.join("locations/country=NG/geom_type=null/type=admin-unit/part-0.parquet");
    assert!(null_file.exists());

    // Unedited, every file (the null one included) makes no change.
    let mut files = Vec::new();
    parquet_files(&dataset.join("locations"), &mut files);
    assert_eq!(files.len(), 3, "{files:?}");
    for (i, file) in files.iter().enumerate() {
        let out = dir.path().join(format!("unedited-{i}.ndjson"));
        let a = diff(&snap, file, &out).success();
        assert!(
            stdout(&a).contains("0 changed"),
            "{}: {}",
            file.display(),
            stdout(&a)
        );
        assert_eq!(report(&out)["counts"], json!({}), "{}", file.display());
    }

    // A point drawn on one, a polygon on the other.
    let edited = dir.path().join("edited.parquet");
    set_geometry(
        &null_file,
        &edited,
        &[
            ("ward", wkb_point(4.5, 7.5)),
            (
                "site",
                wkb_polygon(&[(4.0, 7.0), (5.0, 7.0), (5.0, 8.0), (4.0, 8.0), (4.0, 7.0)]),
            ),
        ],
    );
    let out = dir.path().join("changes.ndjson");
    let a = diff(&snap, &edited, &out).success();
    assert!(
        stdout(&a).contains("2 changed, 0 unchanged, 0 new"),
        "{}",
        stdout(&a)
    );
    assert_eq!(report(&out)["counts"], json!({}));
    let got = changes(&out);
    let ward = got.iter().find(|r| r["id"] == "ward").unwrap();
    assert_eq!(ward["position"], json!({"longitude": 4.5, "latitude": 7.5}));
    assert_eq!(ward["meta"]["versionId"], "1");
    let site = got.iter().find(|r| r["id"] == "site").unwrap();
    assert!(site.get("position").is_none());
    let boundary = site["extension"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["url"] == BOUNDARY_EXT)
        .expect("boundary added");
    assert_eq!(
        decode(boundary["valueAttachment"]["data"].as_str().unwrap()),
        json!({"type": "Polygon", "coordinates": [[[4.0, 7.0], [5.0, 7.0], [5.0, 8.0], [4.0, 8.0], [4.0, 7.0]]]})
    );
}
