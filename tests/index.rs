//! `fhir-pipes index`: backfill spatial-index cells onto a snapshot.

use std::path::Path;

use assert_cmd::prelude::*;
use serde_json::{json, Value};

const EXT: &str = "https://icr.healthcampaigns.org/StructureDefinition/spatial-index";

fn cell_ext(scheme: &str, level: u32, cell: &str) -> Value {
    json!({"url": EXT, "extension": [
        {"url": "system", "valueCode": scheme},
        {"url": "level", "valueUnsignedInt": level},
        {"url": "cell", "valueString": cell}]})
}

fn loc(id: &str, position: Option<(f64, f64)>, exts: Vec<Value>) -> Value {
    let mut r = json!({"resourceType": "Location", "id": id, "meta": {"versionId": "7"}, "name": id,
        "type": [{"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-location-type", "code": "facility"}]}]});
    if let Some((lon, lat)) = position {
        r["position"] = json!({"longitude": lon, "latitude": lat});
    }
    if !exts.is_empty() {
        r["extension"] = Value::Array(exts);
    }
    r
}

fn write_snapshot(dir: &Path, resources: &[Value]) {
    let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(dir.join("locations.ndjson"), text).unwrap();
}

fn cells(resource: &Value) -> Vec<(String, u64, String)> {
    resource["extension"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["url"] == EXT)
        .map(|e| {
            let sub = |name: &str| {
                e["extension"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|x| x["url"] == name)
                    .unwrap()
                    .clone()
            };
            (
                sub("system")["valueCode"].as_str().unwrap().to_string(),
                sub("level")["valueUnsignedInt"].as_u64().unwrap(),
                sub("cell")["valueString"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn index_writes_only_the_positioned_locations_that_lack_a_requested_cell() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(
        dir.path(),
        &[
            loc("needs-cell", Some((9.9, 10.5)), vec![]),
            loc(
                "has-cell",
                Some((9.9, 10.5)),
                vec![cell_ext("quadkey", 18, "123123123123123123")],
            ),
            loc(
                "other-level",
                Some((9.9, 10.5)),
                vec![cell_ext("quadkey", 10, "1231231231")],
            ),
            loc("no-position", None, vec![]),
        ],
    );
    let out = dir.path().join("changes.ndjson");
    let report = dir.path().join("report.json");
    let assert = std::process::Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["index", "--snapshot"])
        .arg(dir.path())
        .args(["--spatial-index", "quadkey:18", "--out"])
        .arg(&out)
        .arg("--report")
        .arg(&report)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(stdout.contains("Wrote 2 of 4 Locations"), "{stdout}");
    assert!(
        stdout.contains("1 already indexed, 1 without a position"),
        "{stdout}"
    );

    let lines: Vec<Value> = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let by_id = |id: &str| lines.iter().find(|r| r["id"] == id).unwrap();

    let needs = by_id("needs-cell");
    assert_eq!(
        needs["meta"]["versionId"], "7",
        "version kept for the if-match PUT"
    );
    assert_eq!(
        needs["name"], "needs-cell",
        "rest of the resource untouched"
    );
    let c = cells(needs);
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].0.as_str(), c[0].1), ("quadkey", 18));
    assert_eq!(c[0].2.len(), 18);
    assert!(c[0].2.chars().all(|ch| ('0'..='3').contains(&ch)));

    let other = by_id("other-level");
    let c = cells(other);
    assert_eq!(
        c.len(),
        2,
        "the zoom-10 cell is kept, the zoom-18 one added"
    );
    assert!(c.iter().any(|x| x.1 == 10) && c.iter().any(|x| x.1 == 18));

    let report: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(report["counts"]["indexed"], 2);
    assert_eq!(report["counts"]["already_indexed"], 1);
    assert_eq!(report["counts"]["no_position"], 1);
}

#[test]
fn refresh_recomputes_a_present_cell_and_a_bad_scheme_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(
        dir.path(),
        &[loc(
            "stale",
            Some((9.9, 10.5)),
            vec![cell_ext("quadkey", 18, "000000000000000000")],
        )],
    );
    let out = dir.path().join("changes.ndjson");
    std::process::Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["index", "--snapshot"])
        .arg(dir.path())
        .args(["--spatial-index", "quadkey:18", "--refresh", "--out"])
        .arg(&out)
        .assert()
        .success()
        .stdout(predicates::str::contains("Wrote 1 of 1"));
    let fixed: Value = serde_json::from_str(
        std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_ne!(cells(&fixed)[0].2, "000000000000000000");

    std::process::Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["index", "--snapshot"])
        .arg(dir.path())
        .args(["--spatial-index", "s2:9", "--out"])
        .arg(&out)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("scheme must be one of"));
}
