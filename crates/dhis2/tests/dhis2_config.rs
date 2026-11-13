//! The settings file (`--config FILE`), `extract all` and `config check`,
//! against the fake DHIS2 of tests/common/dhis2.rs.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use httptest::{Expectation, Server, all_of, matchers::*};
use serde_json::{Value, json};

#[path = "common/dhis2.rs"]
mod stub;
use stub::*;

const PACKAGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/import/icr-dhis2"
);

/// A TOML string: forward slashes, so a Windows path needs no escaping.
fn toml_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

/// `fhir-pipes-dhis2 ARGS`, signed in with the test token.
fn dhis2(args: &[&str]) -> Command {
    let mut c = fhir_pipes_dhis2();
    c.args(args).env("FHIR_PIPES_DHIS2_TOKEN", "d2pat_test");
    c
}

/// The reports run of tests/dhis2_import.rs's resume test: two levels, three
/// months, one batch per indicator, level and month.
const REPORT_OPTIONS: [&str; 15] = [
    "--indicator-group",
    "srxUNt2w5jn",
    "--level",
    "2",
    "--level",
    "3",
    "--chunk",
    "1",
    "--period",
    "202501",
    "--period",
    "202502",
    "--period",
    "202503",
    "--max-rows",
];

/// The same run as a settings file.
fn report_file(server: &Server, chunk: usize) -> String {
    format!(
        "url = \"{}\"\ninstance = \"sl-demo\"\n\n[indicators]\nindicator_groups = [\"srxUNt2w5jn\"]\n\n\
         [reports]\nlevels = [2, 3]\nperiods = [\"202501\", \"202502\", \"202503\"]\nchunk = {chunk}\n\
         max_rows = 1\n",
        base(server)
    )
}

fn org_units(dir: &Path) -> PathBuf {
    let locations = locations_for(
        dir,
        &["O6uvpzGd5pu", "fdc6uOvgoji", "YuQRtpLP10I", "vWbkYPRmKyS"],
    );
    org_units_from_locations(&locations, &dir.join("org-units.ndjson"))
}

/// The same `extract reports` given by options and by a file writes the
/// same bytes. The report of the file's run names the file.
#[test]
fn options_and_file_write_the_same_records() {
    let dir = tempfile::tempdir().unwrap();
    let units = org_units(dir.path());
    let by_options = dir.path().join("options");
    let server = stub_units(false, 0, None);
    let mut c = dhis2(&["extract", "reports", "--dhis2", &base(&server)]);
    c.args(["--instance", "sl-demo"])
        .args(REPORT_OPTIONS)
        .arg("1")
        .arg("--org-units")
        .arg(&units)
        .arg("--out")
        .arg(&by_options)
        .assert()
        .success();

    let server = stub_units(false, 0, None);
    let file = write(dir.path(), "sl.toml", &report_file(&server, 1));
    let by_file = dir.path().join("file");
    let report = dir.path().join("report.json");
    dhis2(&["extract", "reports", "--config"])
        .arg(&file)
        .arg("--org-units")
        .arg(&units)
        .arg("--out")
        .arg(&by_file)
        .arg("--report")
        .arg(&report)
        .assert()
        .success();
    let a = std::fs::read(by_options.join("DHIS2Value.ndjson")).unwrap();
    assert_eq!(a, std::fs::read(by_file.join("DHIS2Value.ndjson")).unwrap());
    assert_eq!(read_ndjson(&by_file.join("DHIS2Value.ndjson")).len(), 24);
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let named = Path::new(r["config"].as_str().unwrap());
    assert_eq!(named, file.canonicalize().unwrap());
}

/// An option on the command line wins over the file: here the file's URL,
/// instance, levels and periods are all wrong for the stub.
#[test]
fn the_command_line_overrides_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let units = org_units(dir.path());
    let server = stub_units(false, 0, None);
    let file = write(
        dir.path(),
        "other.toml",
        "url = \"https://dhis2.invalid\"\ninstance = \"other\"\n\n[indicators]\n\
         indicators = [\"tUIlpyeeX9N\"]\n\n[reports]\nlevels = [4]\nperiods = [\"2024\"]\n\
         chunk = 1\nmax_rows = 1\n",
    );
    let out = dir.path().join("out");
    dhis2(&["extract", "reports", "--config"])
        .arg(&file)
        .args(["--dhis2", &base(&server), "--instance", "sl-demo"])
        .args(REPORT_OPTIONS)
        .arg("1")
        .arg("--org-units")
        .arg(&units)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let records = read_ndjson(&out.join("DHIS2Value.ndjson"));
    assert_eq!(records.len(), 24);
    assert!(records.iter().all(|r| r["instance"] == "sl-demo"));

    // $FHIR_PIPES_DHIS2_CONFIG names the file; --dhis2 still wins.
    let server = stub_units(false, 0, None);
    let out = dir.path().join("env");
    dhis2(&[
        "extract",
        "reports",
        "--dhis2",
        &base(&server),
        "--instance",
        "sl-demo",
    ])
    .env("FHIR_PIPES_DHIS2_CONFIG", &file)
    .args(REPORT_OPTIONS)
    .arg("1")
    .arg("--org-units")
    .arg(&units)
    .arg("--out")
    .arg(&out)
    .assert()
    .success();
}

/// A setting that neither the command line nor the file gives: the error
/// names both ways to give it, and nothing is asked of the server.
#[test]
fn a_missing_setting_names_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "no-levels.toml",
        "url = \"https://dhis2.invalid\"\ninstance = \"sl-demo\"\n[indicators]\n\
         indicator_groups = [\"srxUNt2w5jn\"]\n[reports]\nperiods = [\"2025\"]\n",
    );
    let out = dir.path().join("out");
    let a = dhis2(&["extract", "reports", "--config"])
        .arg(&file)
        .arg("--out")
        .arg(&out)
        .assert()
        .code(2);
    let e = stderr(&a);
    assert!(
        e.contains("no org unit levels to report on: give --level N, or `[reports] levels` in")
            && e.contains("no-levels.toml"),
        "{e}"
    );
    let a = dhis2(&["extract", "all", "--config"])
        .arg(&file)
        .arg("--out")
        .arg(&out)
        .assert()
        .code(2);
    let e = stderr(&a);
    assert!(
        e.contains("extract all, reports step: no org unit levels") && e.contains("--skip reports"),
        "{e}"
    );
    assert!(!out.exists(), "nothing was run");
    // A key the file does not know, and a password.
    for (text, want) in [
        (
            "[reports]\nlevels = [2]\nperiod = [\"2025\"]\n",
            "unknown field `period`",
        ),
        ("password = \"district\"\n", "$FHIR_PIPES_DHIS2_PASSWORD"),
    ] {
        let bad = write(dir.path(), "bad.toml", text);
        let a = dhis2(&["extract", "reports", "--config"])
            .arg(&bad)
            .arg("--out")
            .arg(&out)
            .assert()
            .code(2);
        let e = stderr(&a);
        assert!(e.contains("bad.toml") && e.contains(want), "{e}");
    }
}

/// `--resume` of a run given by a file is refused when a value in the file
/// that shapes the batches changed, and resumes when none did.
#[test]
fn resume_is_refused_when_the_file_changed() {
    let dir = tempfile::tempdir().unwrap();
    let units = org_units(dir.path());
    let out = dir.path().join("in");
    let mut server = Server::run();
    expect_units_batches(&server, false, 0, Some(6));
    let file = dir.path().join("sl.toml");
    std::fs::write(&file, report_file(&server, 1)).unwrap();
    let run = |extra: &[&str]| {
        dhis2(&["extract", "reports", "--config"])
            .arg(&file)
            .arg("--org-units")
            .arg(&units)
            .arg("--out")
            .arg(&out)
            .args(extra)
            .assert()
    };
    run(&["--concurrency", "1"]).code(2);
    server.verify_and_clear();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    expect_units(&server, 3, 2);
    std::fs::write(&file, report_file(&server, 2)).unwrap();
    let refused = run(&["--resume"]).code(2);
    assert!(
        stderr(&refused).contains("was planned differently (chunk changed)"),
        "{}",
        stderr(&refused)
    );
    // The same value on the command line instead of in the file is the
    // same plan.
    server.verify_and_clear();
    expect_units_batches(&server, false, 6, None);
    run(&["--resume", "--chunk", "1"]).success();
    assert_eq!(read_ndjson(&out.join("DHIS2Value.ndjson")).len(), 24);
}

/// A fake DHIS2 that answers all three steps once each: four org units on
/// three levels, two indicators in a group, and one analytics request for
/// level 2 and 2025.
fn all_server() -> Server {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitLevels.json",
        ))
        .respond_with(ok(json!({"organisationUnitLevels": [
            {"level": 1, "name": "National"}, {"level": 2, "name": "District"},
            {"level": 3, "name": "Facility"}]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/organisationUnits.json"),
            request::query(url_decoded(not(contains(("pageSize", "1"))))),
        ])
        .respond_with(ok(
            json!({"pager": {"page": 1, "pageCount": 1}, "organisationUnits": [
                {"id": C, "name": "Sierra Leone", "level": 1},
                {"id": D1, "name": "Bo", "level": 2, "parent": {"id": C}},
                {"id": D2, "name": "Bombali", "level": 2, "parent": {"id": C}},
                {"id": F[0], "name": "Ngelehun CHC", "level": 3, "parent": {"id": D1}}
            ]}),
        )),
    );
    // Both the measures and the reports step read the indicators.
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/indicatorGroups.json",
        ))
        .times(2)
        .respond_with(ok(json!({"indicatorGroups": [{"id": "srxUNt2w5jn"}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/indicators.json"))
            .times(2)
            .respond_with(ok(indicators())),
    );
    expect_units(&server, 2, 2);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/analytics.json"))
            .times(1)
            .respond_with(ok(analytics(json!([
                [
                    "tUIlpyeeX9N",
                    "2025",
                    D1,
                    "50.0",
                    "25.0",
                    "50.0",
                    "100",
                    "1",
                    "1"
                ],
                [
                    "FbKK4ofIv5R",
                    "2025",
                    D2,
                    "40.0",
                    "8.0",
                    "20.0",
                    "100",
                    "1",
                    "1"
                ]
            ])))),
    );
    server
}

fn all_file(dir: &Path, server: &Server) -> PathBuf {
    write(
        dir,
        "sl.toml",
        &format!(
            "url = \"{}\"\ninstance = \"sl-demo\"\npackage = \"{}\"\n\n[locations]\n\
             id_prefix = \"sle\"\ncountry_code = \"SLE\"\n\n[indicators]\n\
             indicator_groups = [\"srxUNt2w5jn\"]\n\n[reports]\nlevels = [2]\n\
             periods = [\"2025\"]\n",
            base(server),
            toml_path(Path::new(PACKAGE))
        ),
    )
}

/// `extract all` writes the three record files, which `import run` with
/// the example package maps; its report names the file and each step.
#[test]
fn extract_all_writes_three_files_that_import() {
    let dir = tempfile::tempdir().unwrap();
    let server = all_server();
    let file = all_file(dir.path(), &server);
    let raw = dir.path().join("raw");
    let report = dir.path().join("report.json");
    let a = dhis2(&["extract", "all", "--config"])
        .arg(&file)
        .arg("--out")
        .arg(&raw)
        .arg("--report")
        .arg(&report)
        .assert()
        .success();
    let e = stderr(&a);
    assert!(
        e.contains("step 1 of 3, locations") && e.contains("step 3 of 3, reports"),
        "{e}"
    );
    let mut names: Vec<String> = std::fs::read_dir(&raw)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "DHIS2Indicator.ndjson",
            "DHIS2OrgUnit.ndjson",
            "DHIS2Value.ndjson"
        ]
    );
    assert_eq!(read_ndjson(&raw.join("DHIS2OrgUnit.ndjson")).len(), 4);
    assert_eq!(read_ndjson(&raw.join("DHIS2Indicator.ndjson")).len(), 2);
    assert_eq!(read_ndjson(&raw.join("DHIS2Value.ndjson")).len(), 2);
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert!(r["config"].as_str().unwrap().ends_with("sl.toml"));
    for step in ["locations", "measures", "reports"] {
        assert!(r[step]["counts"].is_object(), "{step}: {r}");
    }

    let fhir = dir.path().join("fhir");
    fhir_pipes()
        .args(["import", "run", "--package", PACKAGE, "--in"])
        .arg(&raw)
        .arg("--out")
        .arg(&fhir)
        .assert()
        .success();
    let count = |t: &str| read_ndjson(&fhir.join(format!("{t}.ndjson"))).len();
    assert_eq!(count("Location"), 4);
    assert_eq!(count("Organization"), 1);
    assert_eq!(count("Measure"), 2);
    assert_eq!(count("MeasureReport"), 2);
}

/// A failing step stops `extract all` with its exit code, naming the step;
/// `--skip` leaves steps out.
#[test]
fn extract_all_stops_at_the_failing_step_and_skips_on_request() {
    let dir = tempfile::tempdir().unwrap();
    // No org unit records in the folder and the locations step left out:
    // the reports step fails before any request, with exit 2.
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/indicatorGroups.json",
        ))
        .respond_with(ok(json!({"indicatorGroups": [{"id": "srxUNt2w5jn"}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/indicators.json"))
            .respond_with(ok(indicators())),
    );
    let file = all_file(dir.path(), &server);
    let raw = dir.path().join("raw");
    let a = dhis2(&["extract", "all", "--skip", "locations", "--config"])
        .arg(&file)
        .arg("--out")
        .arg(&raw)
        .assert()
        .code(2);
    let e = stderr(&a);
    assert!(
        e.contains("locations: left out (--skip)")
            && e.contains("extract all, reports step: dhis2 extract reports needs the org units"),
        "{e}"
    );
    assert!(raw.join("DHIS2Indicator.ndjson").is_file());
    assert!(!raw.join("DHIS2Value.ndjson").exists());
}

/// `config check` prints the settings and where the credentials come from,
/// never the password, and asks nothing of the server.
#[test]
fn config_check_prints_the_settings_but_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "sl.toml",
        "url = \"https://dhis2.invalid\"\ninstance = \"sl-demo\"\nuser = \"admin\"\n\
         package = \"pkg\"\n[reports]\nlevels = [2]\nperiods = [\"2025\"]\n",
    );
    let a = fhir_pipes_dhis2()
        .args(["config", "check", "--config"])
        .arg(&file)
        .env("FHIR_PIPES_DHIS2_PASSWORD", "district")
        .assert()
        .success();
    let s = stdout(&a);
    assert!(!s.contains("district"), "{s}");
    assert!(
        s.contains("user admin with the password from $FHIR_PIPES_DHIS2_PASSWORD")
            && s.contains("extract all, locations: ready")
            && s.contains("extract all, measures: no indicators selected")
            && s.contains("instance = \"sl-demo\"")
            // The path ends the value. On Windows it holds backslashes, so
            // TOML writes it between single quotes.
            && (s.contains("pkg\"") || s.contains("pkg'")),
        "{s}"
    );
    // The package path, resolved against the file's folder.
    let folder = toml_path(&dir.path().canonicalize().unwrap());
    assert!(
        s.contains(&format!("package = \"{folder}/pkg\"")) || cfg!(windows),
        "{s}"
    );
    let bad = write(dir.path(), "bad.toml", "[reports]\nlevels = [0]\n");
    let a = fhir_pipes_dhis2()
        .args(["config", "check", "--config"])
        .arg(&bad)
        .assert()
        .code(2);
    assert!(stderr(&a).contains("counts from 1"), "{}", stderr(&a));
    let a = fhir_pipes_dhis2()
        .args(["config", "check"])
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("needs a settings file"),
        "{}",
        stderr(&a)
    );
}
