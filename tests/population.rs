//! `fhir-pipes population`: raster → per-unit Groups, rolled up the hierarchy.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use base64::Engine;
use httptest::{Expectation, Server, matchers::*, responders::*};
use serde_json::{Value, json};
use tiff::encoder::{TiffEncoder, colortype::Gray32Float};
use tiff::tags::Tag;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/raster/pop_small_cog.tif"
);
const BOUNDARY_EXT: &str =
    "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson";
const SD: &str = "https://icr.healthcampaigns.org/StructureDefinition";

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> String {
    format!(
        r#"{{"type":"Polygon","coordinates":[[[{x0},{y0}],[{x1},{y0}],[{x1},{y1}],[{x0},{y1}],[{x0},{y0}]]]}}"#
    )
}

fn admin(id: &str, name: &str, part_of: Option<&str>, boundary: Option<&str>) -> Value {
    let mut r = json!({
        "resourceType": "Location",
        "id": id,
        "meta": {"versionId": "3"},
        "name": name,
        "type": [{"coding": [{
            "system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
            "code": "admin-unit"
        }]}],
    });
    if let Some(p) = part_of {
        r["partOf"] = json!({"reference": format!("Location/{p}")});
    }
    if let Some(b) = boundary {
        let data = base64::engine::general_purpose::STANDARD.encode(b);
        r["extension"] = json!([{
            "url": BOUNDARY_EXT,
            "valueAttachment": {"contentType": "application/geo+json", "data": data}
        }]);
    }
    r
}

fn facility(id: &str, part_of: &str, lon: f64, lat: f64) -> Value {
    json!({
        "resourceType": "Location",
        "id": id,
        "meta": {"versionId": "3"},
        "name": id,
        "type": [{"coding": [{
            "system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
            "code": "facility"
        }]}],
        "partOf": {"reference": format!("Location/{part_of}")},
        "position": {"longitude": lon, "latitude": lat},
    })
}

fn write_snapshot(dir: &Path, resources: &[Value]) {
    let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(dir.join("locations.ndjson"), text).unwrap();
}

fn run(dir: &Path, raster: &str, extra: &[&str]) -> Assert {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["population", "--snapshot"])
        .arg(dir)
        .args(["--raster", raster, "--year", "2026", "--out"])
        .arg(dir.join("groups.ndjson"))
        .arg("--report")
        .arg(dir.join("report.json"))
        .args(extra)
        .assert()
}

fn groups(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("groups.ndjson"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn report(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap()
}

fn ext<'a>(g: &'a Value, name: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    g["extension"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["url"] == format!("{SD}/{name}"))
        .unwrap_or(&NULL)
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

/// ng (level 0, whole raster) with west (lon 3-5) and east (given, so a
/// caller can leave it boundary-less) under it, plus a facility that should
/// never itself be measured.
fn two_state_country(east_boundary: Option<&str>) -> Vec<Value> {
    vec![
        admin("ng", "Nigeria", None, Some(&rect(3.0, 4.0, 7.0, 7.0))),
        admin(
            "west",
            "West State",
            Some("ng"),
            Some(&rect(3.0, 4.0, 5.0, 7.0)),
        ),
        admin("east", "East State", Some("ng"), east_boundary),
        facility("clinic", "west", 3.5, 6.5),
    ]
}

/// A stripped Float32 GeoTIFF, same 40x30 georeferencing as the fixture COG,
/// value (r*100+c+1)+0.4 everywhere (no nodata), for the additivity test.
fn write_frac_tif(path: &Path) {
    let (w, h) = (40u32, 30u32);
    let mut data = Vec::with_capacity((w * h) as usize);
    for r in 0..h {
        for c in 0..w {
            data.push((r * 100 + c + 1) as f32 + 0.4);
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut tiff = TiffEncoder::new(&mut buf).unwrap();
        let mut image = tiff.new_image::<Gray32Float>(w, h).unwrap();
        image.rows_per_strip(8).unwrap();
        {
            let enc = image.encoder();
            enc.write_tag(Tag::ModelPixelScaleTag, &[0.1f64, 0.1, 0.0][..])
                .unwrap();
            enc.write_tag(
                Tag::ModelTiepointTag,
                &[0.0f64, 0.0, 0.0, 3.0, 7.0, 0.0][..],
            )
            .unwrap();
            enc.write_tag(Tag::GdalNodata, "-99999").unwrap();
        }
        image.write_data(&data).unwrap();
    }
    std::fs::write(path, buf.into_inner()).unwrap();
}

#[test]
fn measures_the_requested_level_and_rolls_up_to_the_country() {
    let dir = tempfile::tempdir().unwrap();
    let east = rect(5.0, 4.0, 7.0, 7.0);
    write_snapshot(dir.path(), &two_state_country(Some(&east)));

    let assert = run(
        dir.path(),
        FIXTURE,
        &["--level", "1", "--planning-denominator"],
    )
    .success();
    let out = stdout(&assert);
    assert!(out.contains("Wrote 3 Groups"), "{out}");
    assert!(out.contains("2 measured at level 1, 1 rolled up"), "{out}");
    assert!(
        out.contains("Raster total 1761153; assigned to level-1 units 1761153 (100.0%)"),
        "{out}"
    );
    assert!(
        out.contains("1 admin units at other levels were not measured directly"),
        "{out}"
    );
    let err = stderr(&assert);
    assert!(err.contains("Raster pop_small_cog.tif"), "{err}");
    assert!(!err.contains("fetching"), "{err}");

    let gs = groups(dir.path());
    assert_eq!(gs.len(), 3);
    let by_id = |id: &str| {
        gs.iter()
            .find(|g| g["id"] == id)
            .unwrap_or_else(|| panic!("no group {id} in {gs:?}"))
    };

    let west = by_id("pop-worldpop-2026-west");
    assert_eq!(west["resourceType"], "Group");
    assert_eq!(
        west["meta"]["profile"][0],
        "https://icr.healthcampaigns.org/StructureDefinition/ICRTargetPopulation"
    );
    assert_eq!(west["quantity"], 875793);
    assert_eq!(
        west["characteristic"][0]["valueReference"]["reference"],
        "Location/west"
    );
    assert_eq!(
        west["characteristic"][0]["valueReference"]["display"],
        "West State"
    );
    assert_eq!(ext(west, "is-calculated")["valueBoolean"], false);
    assert_eq!(ext(west, "is-planning-denominator")["valueBoolean"], true);
    assert_eq!(ext(west, "estimate-date")["valueDate"], "2026-01-01");
    let src = ext(west, "denominator-source");
    assert_eq!(src["valueCodeableConcept"]["coding"][0]["code"], "worldpop");
    assert!(
        src["valueCodeableConcept"]["text"]
            .as_str()
            .unwrap()
            .contains("pop_small_cog.tif")
    );
    assert_eq!(
        ext(west, "denominator-type")["valueCode"],
        "total-population"
    );

    let east_g = by_id("pop-worldpop-2026-east");
    assert_eq!(east_g["quantity"], 885360);

    let ng = by_id("pop-worldpop-2026-ng");
    assert_eq!(ng["quantity"], 1761153);
    assert_eq!(ext(ng, "is-calculated")["valueBoolean"], true);

    let rep = report(dir.path());
    assert!(rep["counts"].get("no_boundary").is_none());
}

#[test]
fn a_unit_without_a_boundary_is_reported_and_its_ancestors_are_flagged() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &two_state_country(None));

    let assert = run(dir.path(), FIXTURE, &["--level", "1"]).success();
    let out = stdout(&assert);
    assert!(out.contains("Wrote 2 Groups"), "{out}");
    assert!(out.contains("(49.7%)"), "{out}");

    let gs = groups(dir.path());
    assert_eq!(gs.len(), 2);
    assert!(gs.iter().all(|g| g["id"] != "pop-worldpop-2026-east"));
    let ng = gs
        .iter()
        .find(|g| g["id"] == "pop-worldpop-2026-ng")
        .unwrap();
    assert_eq!(ng["quantity"], 875793);
    for g in &gs {
        assert_eq!(ext(g, "is-planning-denominator")["valueBoolean"], false);
    }

    let rep = report(dir.path());
    assert_eq!(rep["counts"]["no_boundary"], 1);
    let issue = rep["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "no_boundary")
        .unwrap();
    assert_eq!(issue["location_id"], "east");
    assert_eq!(rep["counts"]["rollup_incomplete"], 1);
}

#[test]
fn level_zero_measures_the_country_directly_and_reruns_are_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let east = rect(5.0, 4.0, 7.0, 7.0);
    write_snapshot(dir.path(), &two_state_country(Some(&east)));

    let assert = run(dir.path(), FIXTURE, &["--level", "0"]).success();
    let out = stdout(&assert);
    assert!(
        out.contains("2 admin units at other levels were not measured directly"),
        "{out}"
    );

    let gs = groups(dir.path());
    assert_eq!(gs.len(), 1);
    assert_eq!(gs[0]["id"], "pop-worldpop-2026-ng");
    assert_eq!(gs[0]["quantity"], 1761153);
    assert_eq!(ext(&gs[0], "is-calculated")["valueBoolean"], false);

    let before = std::fs::read(dir.path().join("groups.ndjson")).unwrap();
    run(dir.path(), FIXTURE, &["--level", "0"]).success();
    let after = std::fs::read(dir.path().join("groups.ndjson")).unwrap();
    assert_eq!(before, after);
}

#[test]
fn usage_errors_exit_2_and_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let east = rect(5.0, 4.0, 7.0, 7.0);
    write_snapshot(dir.path(), &two_state_country(Some(&east)));

    run(dir.path(), FIXTURE, &["--level", "3"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no admin units at level 3"));
    run(
        dir.path(),
        FIXTURE,
        &["--level", "1", "--source", "world pop"],
    )
    .failure()
    .code(2)
    .stderr(predicates::str::contains("--source"));
    run(dir.path(), FIXTURE, &["--level", "1", "--year", "1800"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--year"));
    run(
        dir.path(),
        FIXTURE,
        &["--level", "1", "--estimate-date", "2026-1-1"],
    )
    .failure()
    .code(2)
    .stderr(predicates::str::contains("--estimate-date"));

    let far_dir = tempfile::tempdir().unwrap();
    let far = vec![
        admin("ng", "Nigeria", None, Some(&rect(30.0, 4.0, 34.0, 7.0))),
        admin(
            "west",
            "West State",
            Some("ng"),
            Some(&rect(30.0, 4.0, 32.0, 7.0)),
        ),
        admin(
            "east",
            "East State",
            Some("ng"),
            Some(&rect(32.0, 4.0, 34.0, 7.0)),
        ),
    ];
    write_snapshot(far_dir.path(), &far);
    run(far_dir.path(), FIXTURE, &["--level", "1"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("right raster"));

    assert!(!dir.path().join("groups.ndjson").exists());
    assert!(!far_dir.path().join("groups.ndjson").exists());
}

#[test]
fn a_url_raster_is_fetched_once_into_the_snapshot_cache() {
    let server = Server::run();
    let bytes = std::fs::read(FIXTURE).unwrap();
    server.expect(
        Expectation::matching(request::method_path("GET", "/nga_pop_2026_CN_100m_cog.tif"))
            .times(1)
            .respond_with(status_code(200).body(bytes)),
    );
    let url = server.url("/nga_pop_2026_CN_100m_cog.tif").to_string();

    let dir = tempfile::tempdir().unwrap();
    let east = rect(5.0, 4.0, 7.0, 7.0);
    write_snapshot(dir.path(), &two_state_country(Some(&east)));

    let first = run(dir.path(), &url, &["--level", "1"]).success();
    let err = stderr(&first);
    assert!(
        err.contains("raster: fetching nga_pop_2026_CN_100m_cog.tif"),
        "{err}"
    );
    assert!(err.contains("Raster nga_pop_2026_CN_100m_cog.tif"), "{err}");
    assert_eq!(
        std::fs::read_dir(dir.path().join("rasters"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(groups(dir.path()).len(), 3);

    let second = run(dir.path(), &url, &["--level", "1"]).success();
    let err2 = stderr(&second);
    assert!(err2.contains("from cache"), "{err2}");
    assert!(!err2.contains("fetching"), "{err2}");
    assert_eq!(groups(dir.path()).len(), 3);
}

#[test]
fn estimate_date_and_source_overrides_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let east = rect(5.0, 4.0, 7.0, 7.0);
    write_snapshot(dir.path(), &two_state_country(Some(&east)));

    run(
        dir.path(),
        FIXTURE,
        &[
            "--level",
            "1",
            "--estimate-date",
            "2024-06-30",
            "--source",
            "grid3",
        ],
    )
    .success();
    let gs = groups(dir.path());
    assert!(
        gs.iter()
            .all(|g| g["id"].as_str().unwrap().starts_with("pop-grid3-2026-"))
    );
    for g in &gs {
        assert_eq!(ext(g, "estimate-date")["valueDate"], "2024-06-30");
        let src = ext(g, "denominator-source");
        assert_eq!(src["valueCodeableConcept"]["coding"][0]["code"], "grid3");
        assert_eq!(
            src["valueCodeableConcept"]["coding"][0]["display"],
            "GRID3 modelled estimate"
        );
    }

    run(
        dir.path(),
        FIXTURE,
        &["--level", "1", "--source", "made-up"],
    )
    .success();
    let rep = report(dir.path());
    assert_eq!(rep["counts"]["unknown_source_code"], 1);
}

#[test]
fn published_quantities_add_up_exactly_with_fractional_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let frac = dir.path().join("frac.tif");
    write_frac_tif(&frac);

    let snapshot = vec![
        admin("ng", "Nigeria", None, Some(&rect(3.0, 4.0, 7.0, 7.0))),
        admin(
            "west",
            "West State",
            Some("ng"),
            Some(&rect(3.0, 4.0, 5.0, 7.0)),
        ),
        admin(
            "east",
            "East State",
            Some("ng"),
            Some(&rect(5.0, 4.0, 7.0, 7.0)),
        ),
        admin("w1", "W1", Some("west"), Some(&rect(3.0, 4.0, 4.0, 7.0))),
        admin("w2", "W2", Some("west"), Some(&rect(4.0, 4.0, 5.0, 7.0))),
        admin("e1", "E1", Some("east"), Some(&rect(5.0, 4.0, 6.0, 7.0))),
        admin("e2", "E2", Some("east"), Some(&rect(6.0, 4.0, 7.0, 7.0))),
    ];
    write_snapshot(dir.path(), &snapshot);

    let assert = run(dir.path(), frac.to_str().unwrap(), &["--level", "2"]).success();
    let out = stdout(&assert);
    assert!(out.contains("4 measured at level 2, 3 rolled up"), "{out}");

    let gs = groups(dir.path());
    assert_eq!(gs.len(), 7);
    let q = |id: &str| {
        gs.iter()
            .find(|g| g["id"] == format!("pop-worldpop-2026-{id}"))
            .unwrap_or_else(|| panic!("no group for {id} in {gs:?}"))["quantity"]
            .as_i64()
            .unwrap()
    };

    let (w1, w2, e1, e2) = (q("w1"), q("w2"), q("e1"), q("e2"));
    let (west, east, ng) = (q("west"), q("east"), q("ng"));
    assert_eq!(west, w1 + w2);
    assert_eq!(east, e1 + e2);
    assert_eq!(ng, west + east);
    assert!((ng - 1_765_080).abs() <= 2, "ng = {ng}");
}

#[test]
fn a_leaf_whose_group_id_is_too_long_is_skipped_and_its_ancestors_are_flagged() {
    // `pop-worldpop-2026-` is 18 chars; a 50-char Location id pushes the Group id past 64.
    let long_id = "e".repeat(50);
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(
        dir.path(),
        &[
            admin("ng", "Nigeria", None, Some(&rect(3.0, 4.0, 7.0, 7.0))),
            admin(
                "west",
                "West State",
                Some("ng"),
                Some(&rect(3.0, 4.0, 5.0, 7.0)),
            ),
            admin(
                &long_id,
                "East State",
                Some("ng"),
                Some(&rect(5.0, 4.0, 7.0, 7.0)),
            ),
        ],
    );
    let assert = run(dir.path(), FIXTURE, &["--level", "1"]).success();
    let out = stdout(&assert);
    assert!(out.contains("Wrote 2 Groups"), "{out}");

    let gs = groups(dir.path());
    assert_eq!(gs.len(), 2);
    let ng = gs
        .iter()
        .find(|g| g["id"] == "pop-worldpop-2026-ng")
        .unwrap();
    assert_eq!(
        ng["quantity"], 875793,
        "the skipped leaf must not inflate its parent"
    );

    let rep = report(dir.path());
    assert_eq!(rep["counts"]["group_id_too_long"], 1);
    assert_eq!(rep["counts"]["rollup_incomplete"], 1);
}

/// `--type CODE` measures every Location of that type on its own — a catchment tiling
/// is not the admin tree — and rolls nothing up: the admin units' own Groups are not
/// (re)written from a partial set of polygons.
#[test]
fn type_mode_measures_each_polygon_and_never_rolls_up() {
    let dir = tempfile::tempdir().unwrap();
    let mut resources = two_state_country(Some(&rect(5.0, 4.0, 7.0, 7.0)));
    for (id, b) in [
        ("c1", rect(3.0, 4.0, 4.0, 7.0)),
        ("c2", rect(4.0, 4.0, 5.0, 7.0)),
    ] {
        let mut c = admin(id, id, Some("west"), Some(&b));
        c["type"] = json!([{"coding": [{
            "system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
            "code": "facility-catchment"
        }]}]);
        resources.push(c);
    }
    write_snapshot(dir.path(), &resources);

    let assert = run(dir.path(), FIXTURE, &["--type", "facility-catchment"]).success();
    let out = stdout(&assert);
    assert!(
        out.contains("2 measured at type facility-catchment, 0 rolled up"),
        "{out}"
    );
    let gs = groups(dir.path());
    let ids: Vec<&str> = gs.iter().map(|g| g["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["pop-worldpop-2026-c1", "pop-worldpop-2026-c2"]);
    // The two catchments tile West State exactly, so their sums add to its level-1 figure.
    run(dir.path(), FIXTURE, &["--level", "1"]).success();
    let west_total = groups(dir.path())
        .iter()
        .find(|g| g["id"] == "pop-worldpop-2026-west")
        .map(|g| g["quantity"].as_u64().unwrap())
        .unwrap();
    let tiled: u64 = gs.iter().map(|g| g["quantity"].as_u64().unwrap()).sum();
    assert_eq!(tiled, west_total);
    for g in &gs {
        assert_eq!(ext(g, "is-calculated")["valueBoolean"], json!(false));
        assert!(
            ext(g, "denominator-source")["valueCodeableConcept"]["text"]
                .as_str()
                .unwrap()
                .contains("over type facility-catchment")
        );
    }

    run(dir.path(), FIXTURE, &["--type", "school"])
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no Locations of type school"));
    run(
        dir.path(),
        FIXTURE,
        &["--type", "facility-catchment", "--level", "1"],
    )
    .failure()
    .code(2);
}
