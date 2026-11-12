//! Binary-level tests for `fhir-pipes map run`.

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;

const FACILITY: &str = r#"{"resourceType":"StructureDefinition","id":"Facility",
    "url":"https://example.org/StructureDefinition/Facility","name":"Facility","status":"draft",
    "fhirVersion":"4.0.1","kind":"logical","abstract":false,
    "type":"https://example.org/StructureDefinition/Facility",
    "baseDefinition":"http://hl7.org/fhir/StructureDefinition/Element","derivation":"specialization",
    "snapshot":{"element":[
      {"id":"Facility","path":"Facility","min":0,"max":"*"},
      {"id":"Facility.code","path":"Facility.code","min":1,"max":"1","type":[{"code":"string"}]},
      {"id":"Facility.name","path":"Facility.name","min":1,"max":"1","type":[{"code":"string"}]},
      {"id":"Facility.kind","path":"Facility.kind","min":0,"max":"1","type":[{"code":"code"}]}]}}"#;

const KINDS: &str = r#"{"resourceType":"ConceptMap","url":"https://example.org/ConceptMap/kinds","status":"draft",
    "group":[{"source":"https://example.org/kinds","target":"http://terminology.hl7.org/CodeSystem/location-physical-type",
      "element":[{"code":"site","target":[{"code":"si","display":"Site","equivalence":"equivalent"}]}]}]}"#;

/// A code ending in `x` fails the check; a kind other than `site` is a
/// warning.
const MAP: &str = "/// url = 'https://example.org/StructureMap/FacilityToLocation'
/// name = 'FacilityToLocation'

uses \"https://example.org/StructureDefinition/Facility\" alias Facility as source
uses \"http://hl7.org/fhir/StructureDefinition/Location\" alias Location as target

group Facility(source src : Facility, target loc : Location) {
  src.code as c check (c.endsWith('x').not()) -> loc.id = c \"id\";
  src.name as n -> loc.name = n \"name\";
  src.kind as k -> loc.physicalType = translate(k, 'https://example.org/ConceptMap/kinds', 'CodeableConcept') \"kind\";
}
";

fn maps(dir: &Path) -> std::path::PathBuf {
    let maps = dir.join("maps");
    std::fs::create_dir_all(&maps).unwrap();
    std::fs::write(maps.join("FacilityToLocation.map"), MAP).unwrap();
    std::fs::write(maps.join("Facility.json"), FACILITY).unwrap();
    std::fs::write(maps.join("kinds.json"), KINDS).unwrap();
    maps
}

/// `count` records, one per line, with a blank line after every 50th so
/// line numbers and record numbers differ. The record on line `fail` (when
/// given) fails the map.
fn input(path: &Path, count: usize, fail: Option<usize>) {
    let mut text = String::new();
    let mut line = 0;
    for i in 0..count {
        line += 1;
        let code = if Some(line) == fail {
            format!("f{i}x")
        } else {
            format!("f{i}")
        };
        let kind = if i % 3 == 0 { "other" } else { "site" };
        // Some records name their type, the rest are given the map's.
        let record = if i % 2 == 0 {
            format!(
                r#"{{"resourceType":"Facility","code":"{code}","name":"n{i}","kind":"{kind}"}}"#
            )
        } else {
            format!(r#"{{"code":"{code}","name":"n{i}","kind":"{kind}"}}"#)
        };
        text.push_str(&record);
        text.push('\n');
        if i % 50 == 49 {
            text.push('\n');
            line += 1;
        }
    }
    std::fs::write(path, text).unwrap();
}

fn map_run(maps: &Path, input: &Path, out: &Path, threads: usize) -> std::process::Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["map", "run", "--maps"])
        .arg(maps)
        .arg("--in")
        .arg(input)
        .arg("--out")
        .arg(out)
        .args(["--threads", &threads.to_string()])
        .output()
        .unwrap()
}

#[test]
fn the_output_is_the_same_at_any_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let maps = maps(dir.path());
    let records = dir.path().join("records.ndjson");
    input(&records, 400, None);
    let mut first: Option<(Vec<u8>, Vec<u8>)> = None;
    for threads in [1, 8] {
        let out = dir.path().join(format!("out-{threads}.ndjson"));
        let run = map_run(&maps, &records, &out, threads);
        assert!(run.status.success(), "{run:?}");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(
            stdout.contains("Wrote 400 resources from 400 records"),
            "{stdout}"
        );
        // One warning for every third record, each naming its line.
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert_eq!(stderr.matches("line ").count(), 134, "{stderr}");
        assert!(stderr.starts_with("line 1: "), "{stderr}");
        let written = std::fs::read(&out).unwrap();
        let text = String::from_utf8_lossy(&written);
        assert_eq!(text.lines().count(), 400);
        assert!(text.lines().next().unwrap().contains(r#""id":"f0""#));
        assert!(text.lines().last().unwrap().contains(r#""id":"f399""#));
        match &first {
            None => first = Some((written, run.stderr)),
            Some((written_1, stderr_1)) => {
                assert_eq!(&written, written_1, "{threads} threads");
                assert_eq!(&run.stderr, stderr_1, "{threads} threads");
            }
        }
    }
}

#[test]
fn a_failing_record_names_its_line_at_any_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let maps = maps(dir.path());
    let records = dir.path().join("records.ndjson");
    // Line 260 holds record 254: five blank lines come before it.
    input(&records, 300, Some(260));
    for threads in [1, 8] {
        let out = dir.path().join(format!("out-{threads}.ndjson"));
        let run = map_run(&maps, &records, &out, threads);
        assert!(!run.status.success(), "{run:?}");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("records.ndjson: line 260: map FacilityToLocation"),
            "{threads} threads: {stderr}"
        );
        assert!(stderr.contains("failed"), "{stderr}");
        // The warnings of the lines before it are printed, none after.
        assert!(stderr.contains("line 258: "), "{stderr}");
        assert!(!stderr.contains("line 261: "), "{stderr}");
        assert!(!out.exists(), "{threads} threads");
    }
}
