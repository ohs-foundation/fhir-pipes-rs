//! Binary-level tests for `fhir-pipes terminology`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;

fn terminology() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.arg("terminology");
    c
}

/// A directory package with a code system, a value set over all of it, one
/// that needs SNOMED CT, and an R4 concept map.
fn package(dir: &Path) {
    let cs = "http://example.org/CodeSystem/colour";
    let resources = [
        json!({"resourceType": "CodeSystem", "url": cs, "version": "1", "content": "complete",
               "concept": [{"code": "red", "display": "Red"}, {"code": "blue", "display": "Blue"}]}),
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/colour",
               "compose": {"include": [{"system": cs}]}}),
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/finding",
               "compose": {"include": [{"system": "http://snomed.info/sct", "filter": [
                   {"property": "concept", "op": "is-a", "value": "404684003"}]}]}}),
        json!({"resourceType": "ConceptMap", "url": "http://example.org/ConceptMap/colour",
               "group": [{"source": cs, "target": "http://example.org/CodeSystem/rgb",
                   "element": [{"code": "red", "target": [{"code": "FF0000", "equivalence": "equal"}]}]}]}),
    ];
    for (i, r) in resources.iter().enumerate() {
        fs::write(dir.join(format!("r{i}.json")), r.to_string()).unwrap();
    }
}

#[test]
fn a_value_set_is_written_as_a_table() {
    let dir = tempfile::tempdir().unwrap();
    package(dir.path());
    terminology()
        .args([
            "valueset",
            "http://example.org/ValueSet/colour",
            "--package",
        ])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(
            "system,version,code,display,inactive\n\
             http://example.org/CodeSystem/colour,1,blue,Blue,\n\
             http://example.org/CodeSystem/colour,1,red,Red,\n",
        )
        .stderr(contains("2 rows"));
}

#[test]
fn a_concept_map_is_written_as_a_table() {
    let dir = tempfile::tempdir().unwrap();
    package(dir.path());
    let out = dir.path().join("map.ndjson");
    terminology()
        .args([
            "conceptmap",
            "http://example.org/ConceptMap/colour",
            "--output",
            "ndjson",
            "--package",
        ])
        .arg(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(&out).unwrap().trim()).unwrap();
    assert_eq!(row["source_code"], "red");
    assert_eq!(row["target_code"], "FF0000");
    assert_eq!(row["relationship"], "equivalent");
}

#[test]
fn a_value_set_the_packages_cannot_expand_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    package(dir.path());
    terminology()
        .args([
            "valueset",
            "http://example.org/ValueSet/finding",
            "--package",
        ])
        .arg(dir.path())
        .assert()
        .code(2)
        .stdout("")
        .stderr(contains(
            "http://snomed.info/sct, which is not in the packages",
        ));
    terminology()
        .args(["valueset", "http://example.org/ValueSet/nope", "--package"])
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(contains("no ValueSet http://example.org/ValueSet/nope"));
}
