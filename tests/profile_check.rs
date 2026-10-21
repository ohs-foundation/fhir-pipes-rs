//! `fhir-pipes profile check` against a slice of ICR's IG
//! (tests/fixtures/ig: the resources ICR's profile refers to, from SUSHI).

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::str::contains;

const ICR_PROFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/profiles/icr.toml");
const IG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ig");

fn check(profile: &Path) -> assert_cmd::assert::Assert {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_PROFILE")
        .args(["profile", "check", "--ig", IG, "--profile"])
        .arg(profile)
        .assert()
}

fn lines(a: &assert_cmd::assert::Assert, prefix: &str) -> Vec<String> {
    String::from_utf8_lossy(&a.get_output().stdout)
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

#[test]
fn icrs_profile_matches_its_ig() {
    let a = check(Path::new(ICR_PROFILE)).success();
    assert_eq!(lines(&a, "error"), Vec::<String>::new());
    let warnings = lines(&a, "warning");
    assert_eq!(warnings.len(), 3, "{warnings:#?}");
    // The older location-type URI, kept on purpose for old data.
    assert!(warnings[0].contains("icr-location-type is not a CodeSystem"));
    // national-admin-code is read but not sliced in ICRLocation.
    assert!(warnings[1].contains("national-admin-code is not an identifier slice"));
    let out = String::from_utf8_lossy(&a.get_output().stdout).into_owned();
    assert!(
        out.contains("nga-nhfr-code (the IG's Organization profiles slice no identifier systems)"),
        "{out}"
    );
    // Read as [location] catchment_extension.
    assert!(!out.contains("StructureDefinition/catchment-of"), "{out}");
    assert!(out.contains("StructureDefinition/building-count"), "{out}");
    assert!(out.contains("0 error(s), 3 warning(s)"), "{out}");
}

/// The profile as it was before it was brought in line with the IG.
#[test]
fn the_old_icr_profile_shows_its_drift() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.toml");
    std::fs::write(
        &old,
        r#"name = "icr-old"
[location]
overlays_extension = "https://icr.healthcampaigns.org/StructureDefinition/overlays-admin-unit"
admin_unit_type = "admin-unit"
type_systems = ["https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs"]

[[location.column]]
name = "settlement_type"
kind = "extension_code"
url = "https://icr.healthcampaigns.org/StructureDefinition/settlement-type"

[[location.column]]
name = "delivery_strategy"
kind = "extension_code"
url = "https://icr.healthcampaigns.org/StructureDefinition/delivery-strategy"

[organization]
facility_type = "clinic"
"#,
    )
    .unwrap();
    let a = check(&old).code(1);
    let stderr = String::from_utf8_lossy(&a.get_output().stderr).to_string();
    let errors = lines(&a, "error");
    assert_eq!(
        stderr,
        format!(
            "fhir-pipes: the profile does not match the IG: {} error(s)\n",
            errors.len()
        )
    );
    let errors = lines(&a, "error").join("\n");
    for want in [
        "overlays-admin-unit is not an extension in the IG",
        "settlement-type has value CodeableConcept, not code; add system = \"https://icr.healthcampaigns.org/CodeSystem/icr-settlement-type-cs\"",
        "delivery-strategy applies to PlanDefinition, ActivityDefinition, Task, not Location",
        "`clinic` is not a code in https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs",
    ] {
        assert!(errors.contains(want), "{want}\n{errors}");
    }
}

/// The source extension before it was made generic, and a code system
/// without the codes `dhis2 reports` writes.
#[test]
fn the_dhis2_source_extension_and_system_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("p.toml");
    let icr = std::fs::read_to_string(ICR_PROFILE).unwrap();
    std::fs::write(
        &p,
        icr.replace(
            "StructureDefinition/report-source",
            "StructureDefinition/dhis2-source",
        )
        .replace("icr-report-source-cs", "icr-denominator-source-cs"),
    )
    .unwrap();
    let a = check(&p).code(1);
    assert_eq!(
        lines(&a, "error"),
        [
            "error: [dhis2] source_extension: https://icr.healthcampaigns.org/StructureDefinition/dhis2-source is not an extension in the IG",
            "error: [dhis2] source_system: `dhis2-analytics` is not a code in https://icr.healthcampaigns.org/CodeSystem/icr-denominator-source-cs",
            "error: [dhis2] source_system: `dhis2-data-values` is not a code in https://icr.healthcampaigns.org/CodeSystem/icr-denominator-source-cs",
        ]
    );

    // An extension whose value is not a CodeableConcept.
    std::fs::write(
        &p,
        icr.replace(
            "StructureDefinition/report-source",
            "StructureDefinition/dhis2-period",
        ),
    )
    .unwrap();
    let a = check(&p).code(1);
    assert_eq!(
        lines(&a, "error"),
        [
            "error: [dhis2] source_extension: https://icr.healthcampaigns.org/StructureDefinition/dhis2-period has value string, not CodeableConcept"
        ]
    );
}

#[test]
fn a_column_system_outside_the_bound_value_set_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("p.toml");
    std::fs::write(
        &p,
        r#"[[location.column]]
name = "settlement_type"
kind = "extension_code"
url = "https://icr.healthcampaigns.org/StructureDefinition/settlement-type"
system = "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs"
"#,
    )
    .unwrap();
    let a = check(&p).success();
    assert!(
        lines(&a, "warning")
            .iter()
            .any(|w| w.contains("icr-ownership-cs is not in the ValueSet")),
        "{:?}",
        lines(&a, "warning")
    );
}

#[test]
fn check_needs_a_profile_and_a_readable_ig() {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env_remove("FHIR_PIPES_PROFILE")
        .args(["profile", "check", "--ig", IG])
        .assert()
        .code(2)
        .stderr(contains("needs a profile"));
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env_remove("FHIR_PIPES_PROFILE")
        .args([
            "profile",
            "check",
            "--profile",
            ICR_PROFILE,
            "--ig",
            "http://example.org/package.tgz",
        ])
        .assert()
        .code(2)
        .stderr(contains("https only"));
    let empty = tempfile::tempdir().unwrap();
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env_remove("FHIR_PIPES_PROFILE")
        .args(["profile", "check", "--profile", ICR_PROFILE, "--ig"])
        .arg(empty.path())
        .assert()
        .code(2)
        .stderr(contains("no StructureDefinitions or CodeSystems"));
}

/// ICR's profile with the optional keys `import` reads, set explicitly.
fn with_import_keys(location_profile: &str) -> String {
    // A Windows checkout has CRLF line endings; the replacements expect LF.
    std::fs::read_to_string(ICR_PROFILE)
        .unwrap()
        .replace("\r\n", "\n")
        .replace(
            "admin_code_column = \"pcode\"\n",
            &format!(
                "admin_code_column = \"pcode\"\nprofile = \"{location_profile}\"\n\
                 admin_code_system = \"https://icr.healthcampaigns.org/identifiers/national-admin-code\"\n"
            ),
        )
        .replace(
            "id_prefix = \"org-\"\n",
            "id_prefix = \"org-\"\nprofile = \"https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization\"\n",
        )
}

#[test]
fn the_import_keys_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.toml");
    std::fs::write(
        &good,
        with_import_keys("https://icr.healthcampaigns.org/StructureDefinition/ICRLocation"),
    )
    .unwrap();
    let a = check(&good).success();
    assert_eq!(lines(&a, "error"), Vec::<String>::new());
    assert!(
        lines(&a, "warning")
            .iter()
            .any(|w| w.contains("[location] admin_code_system")),
        "{:?}",
        lines(&a, "warning")
    );

    let bad = dir.path().join("bad.toml");
    std::fs::write(
        &bad,
        with_import_keys(
            "https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization",
        ),
    )
    .unwrap();
    let a = check(&bad).code(1);
    let errors = lines(&a, "error").join("\n");
    assert!(
        errors.contains("[location] profile: https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization constrains Organization, not Location"),
        "{errors}"
    );
}
