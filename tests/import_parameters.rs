//! Package parameters: `[parameters]` in import.toml, `--param NAME=VALUE`
//! on `import check`, `run` and `test`, a test case's `parameters.toml`, and
//! the values reaching maps (`%NAME`), views (`%NAME`) and SQLQuery
//! Libraries (`:NAME`). The icr-dhis2 package has maps only, so its tests
//! run without duckdb; the digit-visits ones run a library step and are
//! skipped without it.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::prelude::*;
use serde_json::Value;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/import")
        .join(name)
}

fn have_duckdb(skipped: &str) -> bool {
    let found = Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
}

/// The file's bytes with LF line ends. On Windows, Git checks the example
/// files out with CRLF.
fn read_lf(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    let mut out = Vec::with_capacity(bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            continue;
        }
        out.push(*b);
    }
    out
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            fs::write(&dest, read_lf(&path)).unwrap();
        }
    }
}

/// A copy of an example package at `<tmp>/pkg`.
fn package(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    copy_dir(&example(name), &pkg);
    (tmp, pkg)
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{} has no {from:?}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

fn fhir_pipes(args: &[&str]) -> Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(args)
        .env("DUCKDB_AGENT_MODE", "0")
        .output()
        .unwrap()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn read_ndjson(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// `import run` of a package over `input`, into `<tmp>/<out>`.
fn run(pkg: &Path, input: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "import",
        "run",
        "--package",
        s(pkg),
        "--in",
        s(input),
        "--out",
        s(out),
    ];
    args.extend_from_slice(extra);
    fhir_pipes(&args)
}

// ---------------------------------------------------------------------------
// Maps (icr-dhis2: no duckdb)

#[test]
fn a_param_reaches_the_maps_and_the_run_report() {
    let pkg = example("icr-dhis2");
    let input = pkg.join("tests/sample/in");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let report = tmp.path().join("report.json");
    let o = run(
        &pkg,
        &input,
        &out,
        &[
            "--param",
            "base=https://example.org",
            "--param",
            "organizationIdPrefix=o-",
            "--report",
            s(&report),
        ],
    );
    assert!(o.status.success(), "{:?}", text(&o));
    let measures = read_ndjson(&out.join("Measure.ndjson"));
    for m in &measures {
        let url = m["url"].as_str().unwrap();
        assert!(
            url.starts_with("https://example.org/Measure/sl-demo-"),
            "{url}"
        );
        let system = m["identifier"][0]["system"].as_str().unwrap();
        assert!(
            system.starts_with("https://example.org/identifiers/dhis2/sl-demo/"),
            "{system}"
        );
    }
    let orgs = read_ndjson(&out.join("Organization.ndjson"));
    assert!(!orgs.is_empty());
    assert!(
        orgs.iter()
            .all(|o| o["id"].as_str().unwrap().starts_with("o-sle-"))
    );
    for l in read_ndjson(&out.join("Location.ndjson")) {
        if let Some(r) = l["managingOrganization"]["reference"].as_str() {
            assert!(r.starts_with("Organization/o-sle-"), "{r}");
        }
        // Not given: the default.
        assert_eq!(
            l["meta"]["profile"][0],
            "https://icr.healthcampaigns.org/StructureDefinition/ICRLocation"
        );
    }
    let report: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["parameters"]["base"], "https://example.org");
    assert_eq!(report["parameters"]["organizationIdPrefix"], "o-");
    assert_eq!(
        report["parameters"]["periodExtension"],
        "https://icr.healthcampaigns.org/StructureDefinition/dhis2-period"
    );
}

#[test]
fn an_unknown_or_badly_typed_param_is_refused() {
    let (tmp, pkg) = package("icr-dhis2");
    let input = pkg.join("tests/sample/in");
    let out = tmp.path().join("out");
    let o = run(&pkg, &input, &out, &["--param", "bsae=https://example.org"]);
    assert_eq!(o.status.code(), Some(2));
    let (_, err) = text(&o);
    assert!(
        err.contains("--param bsae: the package has no parameter bsae; its parameters are: ")
            && err.contains("base"),
        "{err}"
    );
    let o = run(&pkg, &input, &out, &["--param", "base"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o).1.contains("give NAME=VALUE"), "{:?}", text(&o));
    // A typed parameter.
    edit(
        &pkg.join("import.toml"),
        "[parameters]\n",
        "[parameters]\nlimit = 3\n",
    );
    let o = run(&pkg, &input, &out, &["--param", "limit=three"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o)
            .1
            .contains("--param limit: \"three\" is not a whole number"),
        "{:?}",
        text(&o)
    );
    assert!(!out.exists());
}

#[test]
fn a_parameter_with_no_default_must_be_given() {
    let (tmp, pkg) = package("icr-dhis2");
    let input = pkg.join("tests/sample/in");
    edit(
        &pkg.join("import.toml"),
        "base = { default = \"https://icr.healthcampaigns.org\", description",
        "base = { type = \"string\", description",
    );
    let o = run(&pkg, &input, &tmp.path().join("a"), &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o).1.contains(
            "the package's parameters base have no default; give each with --param NAME=VALUE"
        ),
        "{:?}",
        text(&o)
    );
    // import check does not need the value, and says so.
    let o = fhir_pipes(&["import", "check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains("  base has no value; a run must give --param base=VALUE"),
        "{out}"
    );
    let o = run(
        &pkg,
        &input,
        &tmp.path().join("b"),
        &["--param", "base=https://icr.healthcampaigns.org"],
    );
    assert!(o.status.success(), "{:?}", text(&o));
    // The maps outside import run take the defaults, so there must be one.
    let o = fhir_pipes(&[
        "map",
        "run",
        "--maps",
        s(&pkg.join("maps")),
        "--map",
        "DHIS2IndicatorToICR",
        "--in",
        s(&input.join("DHIS2Indicator.ndjson")),
        "--out",
        s(&tmp.path().join("m.ndjson")),
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o).1.contains("parameter base has no default"),
        "{:?}",
        text(&o)
    );
}

#[test]
fn map_run_takes_the_defaults_of_the_maps_package() {
    let pkg = example("icr-dhis2");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("m.ndjson");
    let o = fhir_pipes(&[
        "map",
        "run",
        "--maps",
        s(&pkg.join("maps")),
        "--map",
        "DHIS2IndicatorToICR",
        "--in",
        s(&pkg.join("tests/sample/in/DHIS2Indicator.ndjson")),
        "--out",
        s(&out),
    ]);
    let (_, err) = text(&o);
    assert!(o.status.success(), "{err}");
    assert!(err.contains("parameters: the defaults in"), "{err}");
    assert_eq!(
        read_ndjson(&out),
        read_ndjson(&pkg.join("tests/sample/expected/Measure.ndjson"))
    );
}

#[test]
fn import_check_reports_parameters_nothing_reads_and_names_not_declared() {
    let (_tmp, pkg) = package("icr-dhis2");
    edit(
        &pkg.join("import.toml"),
        "[parameters]\n",
        "[parameters]\nunused = { default = 1, description = \"Read by nothing\" }\n",
    );
    edit(
        &pkg.join("maps/DHIS2ValueToICR.map"),
        "let periodExtension = %periodExtension;",
        "let periodExtension = %periodExtensoin;",
    );
    let o = fhir_pipes(&["import", "check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        out.contains("DHIS2ValueToICR.map: reads %periodExtensoin, which is not a parameter in import.toml and not a variable of the map"),
        "{out}"
    );
    assert!(
        out.contains("warning: parameter unused in import.toml: nothing reads it"),
        "{out}"
    );
    assert!(
        out.contains("warning: parameter periodExtension in import.toml: nothing reads it"),
        "{out}"
    );
    assert!(out.contains("  unused = 1"), "{out}");
    // --param works on check too, and an unknown name is refused there.
    let o = fhir_pipes(&["import", "check", "--package", s(&pkg), "--param", "nope=1"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn bad_parameter_declarations_stop_the_load() {
    let (_tmp, pkg) = package("icr-dhis2");
    let check = |pkg: &Path| text(&fhir_pipes(&["import", "check", "--package", s(pkg)])).1;
    edit(
        &pkg.join("import.toml"),
        "[parameters]\n",
        "[parameters]\nrecordLine = 1\n",
    );
    assert!(check(&pkg).contains("parameters.recordLine: `recordLine` is a variable"));
    edit(
        &pkg.join("import.toml"),
        "recordLine = 1\n",
        "x = { type = \"string\", defualt = \"a\" }\n",
    );
    assert!(check(&pkg).contains("parameters.x: unknown key `defualt`"));
}

#[test]
fn a_case_can_set_parameters_and_wins_over_param() {
    let (_tmp, pkg) = package("icr-dhis2");
    let case = pkg.join("tests/prefixed");
    copy_dir(&pkg.join("tests/sample/in"), &case.join("in"));
    fs::write(
        case.join("parameters.toml"),
        "organizationIdPrefix = \"o-\"\n",
    )
    .unwrap();
    let o = run(
        &pkg,
        &case.join("in"),
        &case.join("expected"),
        &["--param", "organizationIdPrefix=o-"],
    );
    assert!(o.status.success(), "{:?}", text(&o));
    fs::remove_file(case.join("expected/.fhir-pipes-import.json")).unwrap();
    let test = |extra: &[&str]| {
        let mut args = vec!["import", "test", "--package", s(&pkg)];
        args.extend_from_slice(extra);
        fhir_pipes(&args)
    };
    let o = test(&[]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(out.contains("case prefixed: passed"), "{out}");
    assert!(out.contains("case sample: passed"), "{out}");
    // --param applies to every case; the case's file still wins.
    let o = test(&["--param", "organizationIdPrefix=z-"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("case prefixed: passed"), "{out}");
    assert!(out.contains("case sample: FAILED"), "{out}");
    // A case's file names only declared parameters, typed as declared.
    fs::write(case.join("parameters.toml"), "organizationIdPrefix = 3\n").unwrap();
    let o = test(&["--case", "prefixed"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o).1.contains(
            "parameters.toml: organizationIdPrefix: the value is of type integer, and the parameter's type is string"
        ),
        "{:?}",
        text(&o)
    );
}

// ---------------------------------------------------------------------------
// Views and Libraries (digit-visits: duckdb)

#[test]
fn params_reach_maps_views_and_a_library() {
    if !have_duckdb("params_reach_maps_views_and_a_library") {
        return;
    }
    let pkg = example("digit-visits");
    let input = pkg.join("tests/sample/in");
    let tmp = tempfile::tempdir().unwrap();
    // The defaults give the expected sample: two visits in household h1.
    let out = tmp.path().join("defaults");
    let o = run(&pkg, &input, &out, &[]);
    assert!(o.status.success(), "{:?}", text(&o));
    assert_eq!(
        read_ndjson(&out.join("Task.ndjson")),
        read_ndjson(&pkg.join("tests/sample/expected/Task.ndjson"))
    );
    // A longer visit gap (the Library's :visitGapSeconds) joins h1's two
    // visits, and the id prefix (the maps' %idPrefix) changes every id.
    let out = tmp.path().join("changed");
    let report = tmp.path().join("report.json");
    let o = run(
        &pkg,
        &input,
        &out,
        &[
            "--param",
            "visitGapSeconds=86400",
            "--param",
            "idPrefix=x-",
            "--report",
            s(&report),
        ],
    );
    assert!(o.status.success(), "{:?}", text(&o));
    let tasks = read_ndjson(&out.join("Task.ndjson"));
    let ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["x-visit-t-001", "x-visit-t-004"]);
    assert_eq!(tasks[0]["requester"]["reference"], "Practitioner/x-w1");
    let report: Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["parameters"]["visitGapSeconds"], 86400);
    assert_eq!(report["parameters"]["stagingTag"], "digit-staging");
    // The views read %stagingTag: a tag the staging maps did not write gives
    // views with no rows. The maps read it too, so change only the view.
    let (tmp2, copy) = package("digit-visits");
    edit(
        &copy.join("views/StgTask.json"),
        "code = %stagingTag",
        "code = %stagingTag + '-other'",
    );
    let o = run(&copy, &input, &tmp2.path().join("o"), &[]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(out.contains("stg_task 0 rows"), "{out}");
}

#[test]
fn library_parameters_must_be_package_parameters() {
    let (_tmp, pkg) = package("digit-visits");
    let check = |pkg: &Path| text(&fhir_pipes(&["import", "check", "--package", s(pkg)]));
    // A Library parameter with no package parameter of its name.
    edit(
        &pkg.join("libraries/DigitVisit.toml"),
        "name = \"visitGapSeconds\"",
        "name = \"gapSeconds\"",
    );
    let (_, err) = check(&pkg);
    assert!(
        err.contains("Library DigitVisit declares the parameter gapSeconds, which import.toml does not declare under [parameters]"),
        "{err}"
    );
    // A type the package's cannot be cast to.
    edit(
        &pkg.join("libraries/DigitVisit.toml"),
        "name = \"gapSeconds\"\ntype = \"integer\"",
        "name = \"visitGapSeconds\"\ntype = \"boolean\"",
    );
    let (_, err) = check(&pkg);
    assert!(
        err.contains("declares the parameter visitGapSeconds of type boolean, and the package's parameter visitGapSeconds is of type integer"),
        "{err}"
    );
    edit(
        &pkg.join("libraries/DigitVisit.toml"),
        "type = \"boolean\"",
        "type = \"integer\"",
    );
    // SQL that names a package parameter the Library does not declare, and
    // a view that reads a name that is neither.
    edit(
        &pkg.join("libraries/DigitVisit.sql"),
        "AS \"roundId\"",
        "AS \"roundId\", :idPrefix AS p",
    );
    edit(
        &pkg.join("views/StgTask.json"),
        "code = %stagingTag",
        "code = %stagingTg",
    );
    let (out, err) = check(&pkg);
    assert!(
        out.contains("warning: Library DigitVisit: its SQL has :idPrefix, and it does not declare idPrefix under [[parameter]]"),
        "{out}\n{err}"
    );
    assert!(
        out.contains("error: view StgTask: reads %stagingTg, which is neither a constant of the view nor a parameter in import.toml"),
        "{out}"
    );
    // A view constant may not take a parameter's name.
    edit(
        &pkg.join("views/StgTask.json"),
        "\"status\":\"draft\",",
        "\"status\":\"draft\",\"constant\":[{\"name\":\"stagingTag\",\"valueString\":\"x\"}],",
    );
    let (_, err) = check(&pkg);
    assert!(
        err.contains("view StgTask declares the constant stagingTag, which is also a parameter in import.toml"),
        "{err}"
    );
}
