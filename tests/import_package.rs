//! Binary-level tests for `fhir-pipes import check`, `run` and `test`, on
//! the example package in examples/import/digit-visits and on copies of it
//! changed to fail. The tests that run a library step need the duckdb CLI
//! and are skipped without it, as the other Library tests are.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::prelude::*;
use serde_json::Value;

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/import/digit-visits")
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

/// Copies a file of the example package, with LF line ends.
fn copy_file(from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
    fs::write(to, read_lf(from.as_ref()))
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            copy_file(&path, &dest).unwrap();
        }
    }
}

/// A copy of the example package in a new temporary folder, at `<tmp>/pkg`.
fn package() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    copy_dir(&example(), &pkg);
    (tmp, pkg)
}

fn sample_in(pkg: &Path) -> PathBuf {
    pkg.join("tests/sample/in")
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{} has no {from:?}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

fn append(path: &Path, line: &str) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(f, "{line}").unwrap();
}

fn import(args: &[&str]) -> Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .arg("import")
        .args(args)
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

/// `import run` over `input` into `out`, with `extra` arguments.
fn run(pkg: &Path, input: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "run",
        "--package",
        s(pkg),
        "--in",
        s(input),
        "--out",
        s(out),
    ];
    args.extend_from_slice(extra);
    import(&args)
}

/// The entries of `dir` whose names start with `.fhir-pipes-import-`: work
/// folders left behind.
fn work_folders(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(".fhir-pipes-import-"))
        .collect()
}

#[test]
fn import_test_passes_on_the_example_package() {
    if !have_duckdb("import_test_passes_on_the_example_package") {
        return;
    }
    let o = import(&["test", "--package", s(&example())]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains(
            "case sample: passed (9 resources: Location 2, MedicationAdministration 4, Task 3)"
        ),
        "{out}"
    );
    assert!(out.contains("1 cases: 1 passed, 0 failed"), "{out}");
}

#[test]
fn import_test_shows_where_the_output_differs() {
    if !have_duckdb("import_test_shows_where_the_output_differs") {
        return;
    }
    let (_tmp, pkg) = package();
    edit(
        &pkg.join("tests/sample/expected/Task.ndjson"),
        "\"status\":\"failed\"",
        "\"status\":\"completed\"",
    );
    let o = import(&["test", "--package", s(&pkg), "--case", "sample"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(out.contains("case sample: FAILED"), "{out}");
    assert!(
        out.contains("Task.ndjson line 3 (Task/dgt-visit-t-004):"),
        "{out}"
    );
    assert!(
        out.contains("status: expected \"completed\", got \"failed\""),
        "{out}"
    );
    // --update writes the actual output, and the case then passes.
    let o = import(&["test", "--package", s(&pkg), "--update"]);
    assert!(o.status.success(), "{:?}", text(&o));
    let o = import(&["test", "--package", s(&pkg)]);
    assert!(o.status.success(), "{:?}", text(&o));
    let o = import(&["test", "--package", s(&pkg), "--case", "nope"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o)
            .1
            .contains("no test case nope; the cases are: sample")
    );
}

#[test]
fn run_writes_the_same_bytes_at_any_thread_count() {
    if !have_duckdb("run_writes_the_same_bytes_at_any_thread_count") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // More facilities than a batch, so the maps run on several threads.
    let input = tmp.path().join("in");
    copy_dir(&sample_in(&example()), &input);
    let mut f = fs::File::create(input.join("DIGITFacility.ndjson")).unwrap();
    for i in 0..5000 {
        let usage = ["Warehouse", "Health Facility", "Mobile"][i % 3];
        writeln!(
            f,
            r#"{{"facilityId":"f-{i}","name":"Facility {i}","usage":"{usage}"}}"#
        )
        .unwrap();
    }
    drop(f);
    let mut outputs = Vec::new();
    for threads in ["1", "8"] {
        let out = tmp.path().join(format!("out-{threads}"));
        let report = tmp.path().join(format!("report-{threads}.json"));
        let o = run(
            &example(),
            &input,
            &out,
            &["--threads", threads, "--report", s(&report)],
        );
        assert!(o.status.success(), "{:?}", text(&o));
        let mut files = Vec::new();
        for name in ["Location", "MedicationAdministration", "Task"] {
            files.push(fs::read(out.join(format!("{name}.ndjson"))).unwrap());
        }
        outputs.push(files);
        // A code with no ConceptMap target is a warning in the report, with
        // the record type and line.
        let report: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
        let step = &report["steps"][0];
        assert_eq!(step["warnings"], 1666);
        assert_eq!(step["warning_list"].as_array().unwrap().len(), 100);
        let first = &step["warning_list"][0];
        assert_eq!(first["record_type"], "DIGITFacility");
        assert!(
            first["at"]
                .as_str()
                .unwrap()
                .ends_with("DIGITFacility.ndjson:3"),
            "{first}"
        );
        assert!(
            first["message"]
                .as_str()
                .unwrap()
                .contains("`Mobile` has no translation"),
            "{first}"
        );
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        String::from_utf8_lossy(&outputs[0][0]).lines().count(),
        5000
    );
}

#[test]
fn check_passes_on_the_example_package() {
    let p = example();
    let o = import(&["check", "--package", s(&p), "--in", s(&sample_in(&p))]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains("digit-visits: 5 maps, 5 logical models: 0 errors, 0 warnings"),
        "{out}"
    );
    assert!(out.contains("12 records in 4 files: 0 problems"), "{out}");
}

#[test]
fn check_reports_a_type_error_in_a_map() {
    let (_tmp, pkg) = package();
    edit(
        &pkg.join("maps/StageDIGITTask.map"),
        "src.roundId as r",
        "src.roundID as r",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        out.contains("StageDIGITTask.map:") && out.contains("has no element `roundID`"),
        "{out}"
    );
    assert!(out.contains("1 errors"), "{out}");
}

#[test]
fn check_refuses_a_manifest_that_names_a_missing_map() {
    let (_tmp, pkg) = package();
    edit(
        &pkg.join("import.toml"),
        "map = \"StageDIGITTask\"",
        "map = \"StageDIGITTasks\"",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("import.toml: step 2") && err.contains("no map is named StageDIGITTasks"),
        "{err}"
    );
}

#[test]
fn check_refuses_a_manifest_with_an_unknown_key() {
    let (_tmp, pkg) = package();
    edit(&pkg.join("import.toml"), "stage = true", "staged = true");
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(err.contains("unknown field `staged`"), "{err}");
}

#[test]
fn check_refuses_a_library_step_before_a_stage_step() {
    let (_tmp, pkg) = package();
    let manifest = pkg.join("import.toml");
    edit(&manifest, "[[step]]\nlibrary = \"DigitVisit\"\n", "");
    edit(
        &manifest,
        "[[step]]\nmap = \"StageDIGITTask\"",
        "[[step]]\nlibrary = \"DigitVisit\"\n\n[[step]]\nmap = \"StageDIGITTask\"",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("step 3 stages resources after the library step 2"),
        "{err}"
    );
}

#[test]
fn check_refuses_reading_a_library_before_it_runs() {
    let (_tmp, pkg) = package();
    let manifest = pkg.join("import.toml");
    edit(&manifest, "[[step]]\nlibrary = \"DigitVisit\"\n", "");
    edit(
        &manifest,
        "[[test]]\nname = \"one row per visit\"",
        "[[step]]\nlibrary = \"DigitVisit\"\n\n[[test]]\nname = \"one row per visit\"",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("step 5: reads Library DigitVisit, which no earlier library step runs"),
        "{err}"
    );
}

#[test]
fn check_in_reports_each_record_problem_with_its_file_and_line() {
    let (_tmp, pkg) = package();
    let tasks = sample_in(&pkg).join("DIGITTask.ndjson");
    append(
        &tasks,
        r#"{"clientReferenceId":"t-005","projectBeneficiaryClientReferenceId":"pb-1","roundId":"r1","createdBy":"w1","status":"ADMINISTRATION_SUCCESS","createdTime":"2026-07-01T09:00:00Z","colour":"red"}"#,
    );
    append(
        &tasks,
        r#"{"clientReferenceId":"t-006","roundId":"r1","createdBy":"w1","status":"ADMINISTRATION_SUCCESS","createdTime":"2026-07-01T09:00:00Z"}"#,
    );
    append(
        &tasks,
        r#"{"clientReferenceId":7,"projectBeneficiaryClientReferenceId":"pb-1","roundId":"r1","createdBy":"w1","status":"ADMINISTRATION_SUCCESS","createdTime":"2026-07-01T09:00:00Z"}"#,
    );
    let o = import(&["check", "--package", s(&pkg), "--in", s(&sample_in(&pkg))]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    let file = tasks.display().to_string();
    for want in [
        format!("{file}:5: unknown element `colour`"),
        format!("{file}:6: missing required element `projectBeneficiaryClientReferenceId`"),
        format!("{file}:7: `clientReferenceId` must be a JSON string (string), not a number"),
        "15 records in 4 files: 3 problems".to_string(),
    ] {
        assert!(out.contains(&want), "{want}\n{out}");
    }
}

#[test]
fn a_record_that_fails_names_its_file_and_line_and_the_work_folder_is_removed() {
    // The package has a library step, so the run needs duckdb before it
    // reads a record.
    if !have_duckdb("a_record_that_fails_names_its_file_and_line_and_the_work_folder_is_removed") {
        return;
    }
    let (tmp, pkg) = package();
    let out = tmp.path().join("out");
    // A record that does not fit its model.
    let facilities = sample_in(&pkg).join("DIGITFacility.ndjson");
    append(
        &facilities,
        r#"{"facilityId":"f-3","name":"Mobile team","colour":"red"}"#,
    );
    let o = run(&pkg, &sample_in(&pkg), &out, &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!(
            "{}:3: unknown element `colour`",
            facilities.display()
        )),
        "{err}"
    );
    // A record the map fails on.
    edit(&facilities, ",\"colour\":\"red\"", "");
    edit(
        &pkg.join("maps/DIGITFacilityToICR.map"),
        "src.name as n -> loc.name = n \"name\";",
        "src.name as n check (n.length() < 18) -> loc.name = n \"name\";",
    );
    let o = run(&pkg, &sample_in(&pkg), &out, &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!(
            "{}:2: map DIGITFacilityToICR",
            facilities.display()
        )) && err.contains("check `n.length() < 18` failed"),
        "{err}"
    );
    assert!(!out.exists());
    assert!(work_folders(tmp.path()).is_empty());
}

#[test]
fn failing_data_tests_are_all_reported_and_nothing_is_written() {
    if !have_duckdb("failing_data_tests_are_all_reported_and_nothing_is_written") {
        return;
    }
    let (tmp, pkg) = package();
    // A column that is always null, a key that repeats, and a query that
    // returns rows.
    edit(
        &pkg.join("libraries/DigitVisit.sql"),
        "worker_id AS \"workerId\",",
        "worker_id AS \"workerId\", CAST(NULL AS VARCHAR) AS note,",
    );
    let manifest = pkg.join("import.toml");
    edit(
        &manifest,
        "unique = [\"visitKey\"]",
        "unique = [\"visitKey\", \"roundId\"]",
    );
    edit(
        &manifest,
        "not_null = [\"householdId\", \"workerId\"]",
        "not_null = [\"householdId\", \"note\"]",
    );
    edit(&manifest, "EXCEPT\n", "UNION ALL\n");
    let out = tmp.path().join("out");
    let report = tmp.path().join("report.json");
    let o = run(&pkg, &sample_in(&pkg), &out, &["--report", s(&report)]);
    let (stdout, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{stdout}\n{err}");
    for want in [
        "test one row per visit (unique visitKey in digit_visit): passed",
        "test one row per visit (unique roundId in digit_visit): FAILED, 1 rows",
        "test one row per visit (not_null householdId in digit_visit): passed",
        "test one row per visit (not_null note in digit_visit): FAILED, 3 rows",
        "test every staged task is in a visit (sql): FAILED, 8 rows",
    ] {
        assert!(stdout.contains(want), "{want}\n{stdout}");
    }
    assert!(
        err.contains("3 of 5 data tests failed; nothing was written"),
        "{err}"
    );
    // The map that reads the Library did not run, and nothing was written.
    assert!(!stdout.contains("step 6"), "{stdout}");
    assert!(!out.exists());
    let report: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["tests"][1]["examples"][0]["roundId"], "dgt-r1");
    assert_eq!(report["tests"][1]["examples"][0]["rows"], 3);
    assert_eq!(report["tests"][3]["examples"][0]["row"], 1);
    assert!(work_folders(tmp.path()).is_empty());
}

#[test]
fn without_a_manifest_each_file_runs_through_the_map_that_reads_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("facilities");
    let p = example();
    for f in [
        "models/StructureDefinition-DIGITFacility.json",
        "maps/DIGITFacilityToICR.map",
        "maps/ConceptMap-digit-facility-usage.json",
    ] {
        fs::create_dir_all(pkg.join(f).parent().unwrap()).unwrap();
        copy_file(p.join(f), pkg.join(f)).unwrap();
    }
    fixed_prefix(&pkg);
    let input = tmp.path().join("in");
    fs::create_dir(&input).unwrap();
    copy_file(
        sample_in(&p).join("DIGITFacility.ndjson"),
        input.join("DIGITFacility.ndjson"),
    )
    .unwrap();
    let out = tmp.path().join("out");
    let o = run(&pkg, &input, &out, &[]);
    let (stdout, err) = text(&o);
    assert!(o.status.success(), "{stdout}\n{err}");
    assert!(
        stdout.contains("step 1 DIGITFacilityToICR: 2 DIGITFacility records → output (Location 2)"),
        "{stdout}"
    );
    assert_eq!(
        fs::read(out.join("Location.ndjson")).unwrap(),
        read_lf(&p.join("tests/sample/expected/Location.ndjson"))
    );
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    assert!(o.status.success(), "{:?}", text(&o));
    assert!(text(&o).0.contains("facilities: 1 maps, 1 logical models"));
    // A record file no map reads.
    copy_file(
        sample_in(&p).join("DIGITTask.ndjson"),
        input.join("DIGITTask.ndjson"),
    )
    .unwrap();
    let o = run(&pkg, &input, &tmp.path().join("out2"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("DIGITTask.ndjson: no logical model in models/ is record type DIGITTask"),
        "{err}"
    );
}

/// The facility map of a package built from the example's files reads the
/// id prefix as the parameter %idPrefix, which only the example's manifest
/// declares; give it the default as a literal instead.
fn fixed_prefix(pkg: &Path) {
    edit(
        &pkg.join("maps/DIGITFacilityToICR.map"),
        "let idPrefix = %idPrefix;",
        "let idPrefix = 'dgt-';",
    );
}

/// A package of the example's facility map and model alone, with
/// `manifest` as its import.toml, and an input folder with its records.
fn facility_package(manifest: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("facilities");
    let p = example();
    for f in [
        "models/StructureDefinition-DIGITFacility.json",
        "maps/DIGITFacilityToICR.map",
        "maps/ConceptMap-digit-facility-usage.json",
    ] {
        fs::create_dir_all(pkg.join(f).parent().unwrap()).unwrap();
        copy_file(p.join(f), pkg.join(f)).unwrap();
    }
    fixed_prefix(&pkg);
    fs::write(pkg.join("import.toml"), manifest).unwrap();
    let input = tmp.path().join("in");
    fs::create_dir(&input).unwrap();
    (tmp, pkg, input)
}

#[test]
fn records_can_have_nulls_and_members_the_model_does_not_describe() {
    // The example's two facilities as a raw API returns them: nulls, and
    // members the model does not describe, at the top and nested.
    let raw = [
        r#"{"facilityId":"f-1","name":"Bo district store","usage":"Warehouse","isPermanent":true,"storageCapacity":500,"source":null,"rowVersion":1,"auditDetails":{"createdBy":"u1","createdTime":1789492036910}}"#,
        r#"{"facilityId":"f-2","name":"Tikonko health post","usage":"Health Facility","isPermanent":true,"storageCapacity":null,"additionalFields":null,"rowVersion":3}"#,
    ];
    let strict = "name = \"facilities\"\n";
    let (tmp, pkg, input) = facility_package(strict);
    let file = input.join("DIGITFacility.ndjson");
    fs::write(&file, raw.join("\n") + "\n").unwrap();
    let f = file.display().to_string();

    // By default each is refused.
    let o = run(&pkg, &input, &tmp.path().join("out1"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!("{f}:1: unknown element `source` (and 2 more")),
        "{err}"
    );
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    for want in [
        format!("{f}:1: unknown element `source`"),
        format!("{f}:1: unknown element `rowVersion`"),
        format!("{f}:1: unknown element `auditDetails`"),
        format!("{f}:2: `storageCapacity` is null"),
        format!("{f}:2: unknown element `additionalFields`"),
        "2 records in 1 files: 6 problems".to_string(),
    ] {
        assert!(out.contains(&want), "{want}\n{out}");
    }

    // Nulls as absent only: the members the model does not describe are still refused.
    fs::write(
        pkg.join("import.toml"),
        "name = \"facilities\"\n[records]\nnull = \"absent\"\n",
    )
    .unwrap();
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(!out.contains("is null"), "{out}");
    assert!(out.contains("2 records in 1 files: 3 problems"), "{out}");

    // Both: the records are read as the model describes them, and the
    // resources are those of the clean records.
    fs::write(
        pkg.join("import.toml"),
        "name = \"facilities\"\n[records]\nnull = \"absent\"\nunknown = \"ignore\"\n",
    )
    .unwrap();
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains(&format!(
            "{f}: ignored 2 members the model does not describe (records with each): \
             auditDetails (1), rowVersion (2)"
        )),
        "{out}"
    );
    assert!(out.contains("2 records in 1 files: 0 problems"), "{out}");
    let out = tmp.path().join("out2");
    let o = run(&pkg, &input, &out, &[]);
    let (stdout, err) = text(&o);
    assert!(o.status.success(), "{stdout}\n{err}");
    assert_eq!(
        fs::read(out.join("Location.ndjson")).unwrap(),
        read_lf(&example().join("tests/sample/expected/Location.ndjson"))
    );

    // An unknown value for either is refused.
    fs::write(
        pkg.join("import.toml"),
        "name = \"facilities\"\n[records]\nunknown = \"drop\"\n",
    )
    .unwrap();
    let o = import(&["check", "--package", s(&pkg)]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("records.unknown is \"drop\"; give \"error\" or \"ignore\""),
        "{err}"
    );
}

#[test]
fn the_values_a_map_may_create_from_one_record_can_be_raised() {
    let (tmp, pkg, input) = facility_package("name = \"facilities\"\n[maps]\nmax_values = 5\n");
    copy_file(
        sample_in(&example()).join("DIGITFacility.ndjson"),
        input.join("DIGITFacility.ndjson"),
    )
    .unwrap();
    let o = run(&pkg, &input, &tmp.path().join("out1"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(
            "map DIGITFacilityToICR: the transform created more than 5 values; a record \
             this large needs a higher limit: set max_values under [maps] in import.toml"
        ),
        "{err}"
    );
    fs::write(
        pkg.join("import.toml"),
        "name = \"facilities\"\n[maps]\nmax_values = 5_000\n",
    )
    .unwrap();
    let o = run(&pkg, &input, &tmp.path().join("out2"), &[]);
    assert!(o.status.success(), "{:?}", text(&o));
}

#[test]
fn a_record_file_that_no_step_reads_is_refused() {
    let (tmp, pkg) = package();
    let input = tmp.path().join("in");
    copy_dir(&sample_in(&pkg), &input);
    fs::write(input.join("DIGITStock.ndjson"), "{}\n").unwrap();
    let o = run(&pkg, &input, &tmp.path().join("out"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("DIGITStock.ndjson: no step reads record type DIGITStock"),
        "{err}"
    );
}

#[test]
fn out_must_be_empty_unless_overwrite_and_overwrite_removes_only_its_own_files() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("facilities");
    let p = example();
    for f in [
        "models/StructureDefinition-DIGITFacility.json",
        "maps/DIGITFacilityToICR.map",
        "maps/ConceptMap-digit-facility-usage.json",
    ] {
        fs::create_dir_all(pkg.join(f).parent().unwrap()).unwrap();
        copy_file(p.join(f), pkg.join(f)).unwrap();
    }
    fixed_prefix(&pkg);
    let input = tmp.path().join("in");
    fs::create_dir(&input).unwrap();
    copy_file(
        sample_in(&p).join("DIGITFacility.ndjson"),
        input.join("DIGITFacility.ndjson"),
    )
    .unwrap();
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("notes.txt"), "mine").unwrap();
    let o = run(&pkg, &input, &out, &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("not empty") && err.contains("--overwrite"),
        "{err}"
    );
    // Pretend an earlier run wrote Stale.ndjson; Other.ndjson it did not.
    fs::write(out.join("Stale.ndjson"), "{}\n").unwrap();
    fs::write(out.join("Other.ndjson"), "{}\n").unwrap();
    fs::write(
        out.join(".fhir-pipes-import.json"),
        r#"{"package": "facilities", "files": ["Stale.ndjson", "Location.ndjson"]}"#,
    )
    .unwrap();
    fs::write(out.join("Location.ndjson"), "old\n").unwrap();
    let o = run(&pkg, &input, &out, &["--overwrite"]);
    assert!(o.status.success(), "{:?}", text(&o));
    assert!(!out.join("Stale.ndjson").exists());
    assert!(out.join("Other.ndjson").exists());
    assert_eq!(fs::read_to_string(out.join("notes.txt")).unwrap(), "mine");
    assert_eq!(
        fs::read_to_string(out.join("Location.ndjson"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let written: Value =
        serde_json::from_str(&fs::read_to_string(out.join(".fhir-pipes-import.json")).unwrap())
            .unwrap();
    assert_eq!(written["files"], serde_json::json!(["Location.ndjson"]));
}

#[test]
fn the_work_folder_is_removed_after_a_run_and_kept_with_keep_work() {
    if !have_duckdb("the_work_folder_is_removed_after_a_run_and_kept_with_keep_work") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let p = example();
    let o = run(&p, &sample_in(&p), &tmp.path().join("out"), &[]);
    assert!(o.status.success(), "{:?}", text(&o));
    assert!(work_folders(tmp.path()).is_empty());

    let work = tmp.path().join("work");
    let o = run(
        &p,
        &sample_in(&p),
        &tmp.path().join("out2"),
        &["--keep-work", s(&work)],
    );
    let (stdout, _) = text(&o);
    assert!(o.status.success(), "{stdout}");
    assert!(stdout.contains("work files kept in"), "{stdout}");
    for f in [
        "staged/Task.ndjson",
        "staged/Basic.ndjson",
        "tables/stg_task.parquet",
        "tables/digit_visit.parquet",
        "rows/digit_visit.ndjson",
    ] {
        assert!(work.join(f).is_file(), "{f}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [
            "staged/Task.ndjson",
            "tables/stg_task.parquet",
            "tables/digit_visit.parquet",
            "rows/digit_visit.ndjson",
        ] {
            let mode = fs::metadata(work.join(f)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{f}");
        }
        let mode = fs::metadata(&work).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    // A Library row is the record the map reads: columns named as the
    // model's elements, nested lists as arrays.
    let first: Value = serde_json::from_str(
        fs::read_to_string(work.join("rows/digit_visit.ndjson"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["visitKey"], "stg-t-001");
    assert_eq!(first["task"][1]["time"], "2026-07-01T09:05:00Z");
    // A --keep-work folder that is not empty is refused.
    let o = run(
        &p,
        &sample_in(&p),
        &tmp.path().join("out3"),
        &["--keep-work", s(&work)],
    );
    assert_eq!(o.status.code(), Some(2));
}
