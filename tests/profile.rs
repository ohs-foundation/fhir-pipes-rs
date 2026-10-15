//! Profiles end to end: base FHIR without one, a custom profile's column read
//! and written back, the copy kept beside the snapshot, and diff refusing
//! columns the profile does not declare.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};

const ICR_PROFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/profiles/icr.toml");

/// A copy of the fixture snapshot: transform keeps the profile beside it.
fn snapshot_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson"),
        dir.path().join("locations.ndjson"),
    )
    .unwrap();
    dir
}

fn fhir_pipes() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_PROFILE");
    c
}

fn transform(snapshot: &Path, out: &Path, profile: Option<&str>) {
    let mut c = fhir_pipes();
    c.args(["transform", "locations", "--snapshot"])
        .arg(snapshot)
        .arg("--out")
        .arg(out);
    if let Some(p) = profile {
        c.args(["--profile", p]);
    }
    c.assert().success();
}

/// Column names of one partition file under `out`.
fn columns(out: &Path) -> Vec<String> {
    let file = walk(&out.join("locations"))
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "parquet"))
        .expect("a parquet file");
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        fs::File::open(file).unwrap(),
    )
    .unwrap();
    reader
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn report(out: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(out.join("_report.json")).unwrap()).unwrap()
}

#[test]
fn without_a_profile_only_base_fhir_is_read() {
    let snap = snapshot_copy();
    let out = tempfile::tempdir().unwrap();
    transform(snap.path(), out.path(), None);
    let cols = columns(out.path());
    for absent in [
        "pcode",
        "settlement_type",
        "nhfr_code",
        "quadkey",
        "overlays_admin_unit_ids",
    ] {
        assert!(!cols.iter().any(|c| c == absent), "{absent} in {cols:?}");
    }
    for present in [
        "id",
        "name",
        "part_of",
        "identifier",
        "admin0_name",
        "country",
    ] {
        assert!(cols.iter().any(|c| c == present), "{present} missing");
    }
    // Every row is filed under country=unknown, and that is not an issue
    // without an admin code to find a country from.
    assert!(out.path().join("locations/country=unknown").is_dir());
    assert!(report(out.path())["counts"].get("no_country").is_none());
    // The snapshot remembers it was transformed without a profile.
    assert_eq!(
        fs::read_to_string(snap.path().join("profile.toml")).unwrap(),
        "name = \"base\"\n"
    );
}

#[test]
fn the_icr_profile_restores_the_icr_columns_and_countries() {
    let snap = snapshot_copy();
    let out = tempfile::tempdir().unwrap();
    transform(snap.path(), out.path(), Some(ICR_PROFILE));
    let cols = columns(out.path());
    for present in [
        "pcode",
        "settlement_type",
        "facility_level",
        "nhfr_code",
        "quadkey",
    ] {
        assert!(cols.iter().any(|c| c == present), "{present} missing");
    }
    assert!(out.path().join("locations/country=NG").is_dir());
    assert_eq!(report(out.path())["counts"]["duplicate_pcode"], 1);
    // A later transform without --profile keeps using the snapshot's copy.
    let again = tempfile::tempdir().unwrap();
    transform(snap.path(), again.path(), None);
    assert!(columns(again.path()).iter().any(|c| c == "pcode"));
}

/// A profile with one extension_code column, `ward_status`.
fn custom_profile(dir: &Path) -> PathBuf {
    let path = dir.join("custom.toml");
    fs::write(
        &path,
        r#"name = "custom"

[[location.column]]
name = "ward_status"
kind = "extension_code"
url = "https://example.org/fhir/StructureDefinition/ward-status"
"#,
    )
    .unwrap();
    path
}

fn diff(
    snapshot: &Path,
    edits: &Path,
    out: &Path,
    profile: Option<&Path>,
) -> assert_cmd::assert::Assert {
    let mut c = fhir_pipes();
    c.args(["diff", "--snapshot"])
        .arg(snapshot)
        .arg("--in")
        .arg(edits)
        .arg("--out")
        .arg(out);
    if let Some(p) = profile {
        c.arg("--profile").arg(p);
    }
    c.assert()
}

fn edits(dir: &Path, properties: Value) -> PathBuf {
    let path = dir.join("edits.geojson");
    fs::write(
        &path,
        json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "geometry": null, "properties": properties}
        ]})
        .to_string(),
    )
    .unwrap();
    path
}

#[test]
fn a_custom_column_is_read_and_written_back() {
    let snap = snapshot_copy();
    let work = tempfile::tempdir().unwrap();
    let profile = custom_profile(work.path());
    let out = tempfile::tempdir().unwrap();
    transform(snap.path(), out.path(), Some(profile.to_str().unwrap()));
    assert!(columns(out.path()).iter().any(|c| c == "ward_status"));

    // diff takes the profile from the snapshot's copy.
    let changes = work.path().join("changes.ndjson");
    let input = edits(work.path(), json!({"id": "kano", "ward_status": "active"}));
    diff(snap.path(), &input, &changes, None)
        .success()
        .stderr(contains("profile: custom"));
    let changed: Value = serde_json::from_str(
        fs::read_to_string(&changes)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(changed["id"], "kano");
    let ext = changed["extension"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["url"] == "https://example.org/fhir/StructureDefinition/ward-status")
        .expect("the extension diff wrote");
    assert_eq!(ext["valueCode"], "active");
}

#[test]
fn diff_refuses_columns_the_profile_does_not_declare() {
    let snap = snapshot_copy();
    let work = tempfile::tempdir().unwrap();
    let changes = work.path().join("changes.ndjson");
    // An ICR column, but the snapshot has no profile copy and none is given.
    let input = edits(work.path(), json!({"id": "kano", "pcode": "NG-KN"}));
    diff(snap.path(), &input, &changes, None)
        .failure()
        .stderr(contains("pcode"))
        .stderr(contains("--profile"));
    assert!(!changes.exists());
    // With the ICR profile the same edit goes through.
    diff(snap.path(), &input, &changes, Some(Path::new(ICR_PROFILE))).success();
    assert!(fs::read_to_string(&changes).unwrap().contains("NG-KN"));
}

#[test]
fn commands_that_need_an_ig_say_so_without_a_profile() {
    let snap = snapshot_copy();
    let work = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args(["index", "--snapshot"])
        .arg(snap.path())
        .args(["--spatial-index", "quadkey:12", "--out"])
        .arg(work.path().join("cells.ndjson"))
        .assert()
        .failure()
        .stderr(contains("spatial_index_extension"));
    fhir_pipes()
        .args([
            "dhis2",
            "measures",
            "--dhis2",
            "http://127.0.0.1:9",
            "--dhis2-token",
            "d2pat_x",
        ])
        .args(["--instance", "x", "--indicator", "abcdefghijk", "--out"])
        .arg(work.path().join("m.ndjson"))
        .assert()
        .failure()
        .stderr(contains("[dhis2]"));
}

#[test]
fn a_bad_profile_is_refused_before_anything_runs() {
    let snap = snapshot_copy();
    let work = tempfile::tempdir().unwrap();
    let bad = work.path().join("bad.toml");
    fs::write(
        &bad,
        "[[location.column]]\nname = \"x\"\nkind = \"fhirpath\"\n",
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    fhir_pipes()
        .args(["transform", "locations", "--snapshot"])
        .arg(snap.path())
        .arg("--out")
        .arg(out.path())
        .arg("--profile")
        .arg(&bad)
        .assert()
        .failure()
        .stderr(contains("bad.toml"));
    fhir_pipes()
        .args([
            "transform",
            "locations",
            "--profile",
            "http://example.org/p.toml",
            "--snapshot",
        ])
        .arg(snap.path())
        .arg("--out")
        .arg(out.path())
        .assert()
        .failure()
        .stderr(contains("https only"));
}
