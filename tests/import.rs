//! `fhir-pipes import location-boundaries` and `import location-sites`, run
//! as commands: boundaries, then sites hung under them.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};

const ICR_PROFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/profiles/icr.toml");
const NATIONAL_ADMIN_CODE: &str = "https://icr.healthcampaigns.org/identifiers/national-admin-code";
const GRID3: &str = "https://icr.healthcampaigns.org/identifiers/grid3-globalid";
const NHFR: &str = "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code";
const FACILITY_TYPE_CS: &str = "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs";

fn square(x: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[x, 6.0], [x + 1.0, 6.0], [x + 1.0, 7.0], [x, 7.0], [x, 6.0]]]})
}

fn ward(lga: &str, ward: &str, x: f64) -> Value {
    json!({"type": "Feature", "geometry": square(x), "properties": {
        "state": "Bauchi", "statecode": "BA", "lga": lga, "ward": ward,
        "ward_alt_names": if ward == "Tilde" { "Tilden Fulani" } else { "" }}})
}

fn fhir_pipes(profile: Option<&str>) -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_PROFILE");
    if let Some(p) = profile {
        c.args(["--profile", p]);
    }
    c
}

fn read_ndjson(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Writes the wards GeoJSON and runs location-boundaries with `extra`.
fn boundaries(
    dir: &Path,
    profile: Option<&str>,
    extra: &[&str],
) -> (PathBuf, assert_cmd::assert::Assert) {
    let input = dir.join("wards.geojson");
    let fc = json!({"type": "FeatureCollection", "features": [
        ward("Toro", "Tilde", 3.0), ward("Toro", "Toro", 4.0), ward("Bogoro", "Bogoro Central", 5.0)]});
    std::fs::write(&input, fc.to_string()).unwrap();
    let out = dir.join("boundaries.ndjson");
    let a = fhir_pipes(profile)
        .args(["import", "location-boundaries", "--in"])
        .arg(&input)
        .args([
            "--country",
            "Nigeria=NGA",
            "--level",
            "state=state:statecode",
            "--level",
            "lga=lga",
            "--level",
            "ward=ward",
            "--alias",
            "ward=ward_alt_names",
        ])
        .args(extra)
        .arg("--out")
        .arg(&out)
        .assert();
    (out, a)
}

#[test]
fn boundaries_then_sites() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let (admin, a) = boundaries(
        dir.path(),
        Some(ICR_PROFILE),
        &["--dissolve-parents", "--report", report.to_str().unwrap()],
    );
    a.success().stdout(contains("Wrote 7 Locations"));
    let units = read_ndjson(&admin);
    let ids: Vec<&str> = units.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "nga",
            "nga-ba",
            "nga-ba-toro",
            "nga-ba-bogoro",
            "nga-ba-toro-tilde",
            "nga-ba-toro-toro",
            "nga-ba-bogoro-bogoro-central"
        ]
    );
    // Every unit has a boundary: the wards their own, the rest dissolved.
    assert!(
        units
            .iter()
            .all(|r| r["extension"][0]["valueAttachment"]["data"].is_string())
    );
    assert_eq!(
        units[1]["identifier"],
        json!([{"system": NATIONAL_ADMIN_CODE, "value": "BA"}])
    );
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(report["counts"], json!({}));

    // A BOM before the header, as Excel writes it.
    let csv = dir.path().join("facilities.csv");
    std::fs::write(
        &csv,
        "\u{feff}id,name,state,lga,ward,lat,lon,nhfr,level\n\
         f1,Tilde PHC,Bauchi,Toro,Tilden Fulani,6.5,3.5,08/1,Primary\n\
         f2,Toro Clinic,Bauchi,Toro,Nowhere,6.5,4.5,,Secondary\n\
         f3,No Place,Bauchi,Toro,Toro,,,,\n",
    )
    .unwrap();
    let out = dir.path().join("sites.ndjson");
    let sites_report = dir.path().join("sites-report.json");
    fhir_pipes(Some(ICR_PROFILE))
        .args(["import", "location-sites", "--in"])
        .arg(&csv)
        .arg("--boundaries")
        .arg(&admin)
        .args([
            "--type",
            "facility",
            "--name-col",
            "name",
            "--lat-col",
            "lat",
            "--lon-col",
            "lon",
            "--id-col",
            "id",
            "--parent",
            "state=state",
            "--parent",
            "lga=lga",
            "--parent",
            "ward=ward",
            "--identifier",
            &format!("{GRID3}=id"),
            "--paired-org",
            "--org-identifier",
            &format!("{NHFR}=nhfr"),
            "--org-type-coding",
            &format!("{FACILITY_TYPE_CS}=level"),
            "--spatial-index",
            "quadkey:18",
            "--out",
        ])
        .arg(&out)
        .arg("--report")
        .arg(&sites_report)
        .assert()
        .success()
        .stdout(contains("Wrote 6 resources"))
        .stdout(contains("(3 Locations, 3 Organizations)"));
    let sites = read_ndjson(&out);
    let kinds: Vec<(&str, &str)> = sites
        .iter()
        .map(|r| {
            (
                r["resourceType"].as_str().unwrap(),
                r["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("Organization", "org-f1"),
            ("Location", "f1"),
            ("Organization", "org-f2"),
            ("Location", "f2"),
            ("Organization", "org-f3"),
            ("Location", "f3")
        ]
    );
    // Matched by alias.
    assert_eq!(
        sites[1]["partOf"]["reference"],
        "Location/nga-ba-toro-tilde"
    );
    assert_eq!(
        sites[1]["managingOrganization"]["reference"],
        "Organization/org-f1"
    );
    assert_eq!(
        sites[1]["extension"][0]["extension"][2]["valueString"]
            .as_str()
            .unwrap()
            .len(),
        18
    );
    assert_eq!(
        sites[0]["identifier"],
        json!([{"system": NHFR, "value": "08/1"}])
    );
    assert_eq!(sites[0]["type"][1]["coding"][0]["code"], "primary");
    // Fell back to the LGA.
    assert_eq!(sites[3]["partOf"]["reference"], "Location/nga-ba-toro");
    assert!(sites[5].get("position").is_none());
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&sites_report).unwrap()).unwrap();
    assert_eq!(
        report["counts"],
        json!({"missing_position": 1, "parent_unresolved": 1})
    );
}

#[test]
fn a_mapping_problem_exits_2_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (out, a) = boundaries(
        dir.path(),
        Some(ICR_PROFILE),
        &["--level", "village=village"],
    );
    a.code(2)
        .stderr(contains("property \"village\" is empty on every feature"));
    assert!(!out.exists());

    let (admin, a) = boundaries(dir.path(), Some(ICR_PROFILE), &[]);
    a.success();
    let csv = dir.path().join("sites.csv");
    std::fs::write(&csv, "id,name,lat,lon\nf1,A,1,1\n").unwrap();
    let out = dir.path().join("sites.ndjson");
    fhir_pipes(Some(ICR_PROFILE))
        .args(["import", "location-sites", "--in"])
        .arg(&csv)
        .arg("--boundaries")
        .arg(&admin)
        .args([
            "--type",
            "facility",
            "--name-col",
            "name",
            "--lat-col",
            "lat",
            "--lon-col",
            "lon",
            "--id-col",
            "id",
            "--parent",
            "ward=ward",
            "--out",
        ])
        .arg(&out)
        .assert()
        .code(2)
        .stderr(contains("the CSV has no column \"ward\" (--parent)"));
    assert!(!out.exists());

    // Rows are written as they are built; a repeated id found after some
    // were written still leaves nothing, not even the temporary file.
    let rows: String = (1..=500).map(|i| format!("f{i},A{i},1,1,Toro\n")).collect();
    std::fs::write(
        &csv,
        format!("id,name,lat,lon,lga\n{rows}f7,Again,1,1,Toro\n"),
    )
    .unwrap();
    fhir_pipes(Some(ICR_PROFILE))
        .args(["import", "location-sites", "--in"])
        .arg(&csv)
        .arg("--boundaries")
        .arg(&admin)
        .args([
            "--type",
            "facility",
            "--name-col",
            "name",
            "--lat-col",
            "lat",
            "--lon-col",
            "lon",
            "--id-col",
            "id",
            "--parent",
            "lga=lga",
            "--out",
        ])
        .arg(&out)
        .assert()
        .code(2)
        .stderr(contains("duplicate id"));
    assert!(!out.exists());
    assert!(!dir.path().join("sites.ndjson.tmp").exists());
}

#[test]
fn without_a_profile_the_shape_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (out, a) = boundaries(dir.path(), None, &[]);
    a.code(2)
        .stderr(contains("type_systems"))
        .stderr(contains("no profile was given. Pass one with --profile"))
        .stderr(contains("has none").not());
    assert!(!out.exists());
}

/// A country without DHIS2: the URLs come from the [location] and
/// [organization] keys, and --code-system overrides the profile's.
#[test]
fn a_profile_without_dhis2_names_its_own_urls() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("p.toml");
    std::fs::write(
        &profile,
        r#"name = "other"
[location]
admin_unit_type = "jurisdiction"
type_systems = ["https://example.org/CodeSystem/location-type"]
profile = "https://example.org/StructureDefinition/OtherLocation"
admin_code_system = "https://example.org/identifiers/admin-code"
"#,
    )
    .unwrap();
    let (out, a) = boundaries(dir.path(), profile.to_str(), &[]);
    a.success();
    let units = read_ndjson(&out);
    assert_eq!(
        units[0]["meta"]["profile"],
        json!(["https://example.org/StructureDefinition/OtherLocation"])
    );
    assert_eq!(
        units[0]["type"][0]["coding"][0],
        json!({"system": "https://example.org/CodeSystem/location-type", "code": "jurisdiction"})
    );
    assert_eq!(
        units[0]["identifier"][0]["system"],
        "https://example.org/identifiers/admin-code"
    );
    // The HL7 boundary extension, as the profile names none.
    assert_eq!(
        units[4]["extension"][0]["url"],
        "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson"
    );

    let (out, a) = boundaries(
        dir.path(),
        profile.to_str(),
        &["--code-system", "https://example.org/identifiers/other"],
    );
    a.success();
    assert_eq!(
        read_ndjson(&out)[0]["identifier"][0]["system"],
        "https://example.org/identifiers/other"
    );
}
