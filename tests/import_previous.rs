//! `fhir-pipes import run --previous --changed --removed`: a run's output
//! compared with an earlier run's, on the example package
//! examples/import/icr-dhis2 (no library step, so no duckdb) and on copies
//! of it. The failing data test case uses examples/import/digit-visits and
//! is skipped without the duckdb CLI.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::prelude::*;
use serde_json::{Map, Value, json};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn icr_dhis2() -> PathBuf {
    root().join("examples/import/icr-dhis2")
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

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `import run --package pkg --in input --out out` and `extra`.
fn run(pkg: &Path, input: &Path, out: &Path, extra: &[&str]) -> Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["import", "run", "--package", s(pkg), "--in", s(input)])
        .args(["--out", s(out)])
        .args(extra)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn ok(o: Output) -> String {
    assert!(o.status.success(), "{}", text(&o));
    text(&o)
}

fn read_lines(p: &Path) -> Vec<Value> {
    fs::read_to_string(p)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn write_lines(p: &Path, values: &[Value]) {
    let text: String = values.iter().map(|v| format!("{v}\n")).collect();
    fs::write(p, text).unwrap();
}

/// The `*.ndjson` file names in `dir`, sorted.
fn ndjson_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".ndjson"))
        .collect();
    names.sort();
    names
}

/// `resourceType/id` of every resource in a file.
fn keys(p: &Path) -> Vec<String> {
    read_lines(p)
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["resourceType"].as_str().unwrap(),
                r["id"].as_str().unwrap()
            )
        })
        .collect()
}

fn report(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

/// A copy of the sample records of examples/import/icr-dhis2 in `<tmp>/<name>`.
fn sample_in(tmp: &Path, name: &str) -> PathBuf {
    let dir = tmp.join(name);
    copy_dir(&icr_dhis2().join("tests/sample/in"), &dir);
    dir
}

/// Every file in `dir` and its bytes, to tell that a run left it alone.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&p).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_second_run_writes_what_changed_and_lists_what_was_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = icr_dhis2();
    let first = sample_in(tmp.path(), "in1");
    let base = tmp.path().join("base");
    ok(run(&pkg, &first, &base, &[]));

    // The next input: one value changed, one added, one gone.
    let second = sample_in(tmp.path(), "in2");
    let values_file = second.join("DHIS2Value.ndjson");
    let mut values = read_lines(&values_file);
    let changed_id = values[1]["id"].as_str().unwrap().to_string();
    values[1]["numerator"] = json!(26);
    let removed_id = values.last().unwrap()["id"].as_str().unwrap().to_string();
    values.pop();
    let mut added = values[0].clone();
    let added_id = added["id"].as_str().unwrap().replace("202501", "202503");
    added["id"] = json!(added_id);
    added["period"] = json!("202503");
    values.push(added);
    write_lines(&values_file, &values);

    let out = tmp.path().join("out");
    let changed = tmp.path().join("changed");
    let removed = tmp.path().join("removed.txt");
    let report_file = tmp.path().join("report.json");
    let printed = ok(run(
        &pkg,
        &second,
        &out,
        &[
            "--previous",
            s(&base),
            "--changed",
            s(&changed),
            "--removed",
            s(&removed),
            "--report",
            s(&report_file),
        ],
    ));
    // --changed: the changed and the added report, in --out's order.
    assert_eq!(ndjson_files(&changed), ["MeasureReport.ndjson"]);
    let changed_keys = keys(&changed.join("MeasureReport.ndjson"));
    assert_eq!(
        changed_keys,
        [
            format!("MeasureReport/{changed_id}"),
            format!("MeasureReport/{added_id}")
        ]
    );
    let out_keys = keys(&out.join("MeasureReport.ndjson"));
    let in_out: Vec<&String> = out_keys
        .iter()
        .filter(|k| changed_keys.contains(k))
        .collect();
    assert_eq!(in_out, changed_keys.iter().collect::<Vec<_>>());
    // Each line as --out has it.
    let out_text = fs::read_to_string(out.join("MeasureReport.ndjson")).unwrap();
    for line in fs::read_to_string(changed.join("MeasureReport.ndjson"))
        .unwrap()
        .lines()
    {
        assert!(out_text.lines().any(|l| l == line));
    }
    assert_eq!(
        fs::read_to_string(&removed).unwrap(),
        format!("MeasureReport/{removed_id}\n")
    );
    // --out has everything.
    assert_eq!(ndjson_files(&out).len(), 4);
    assert_eq!(out_keys.len(), 8);
    assert!(!out_keys.contains(&format!("MeasureReport/{removed_id}")));
    // The counts.
    let r = report(&report_file);
    let c = &r["comparison"];
    assert_eq!(c["previous"], s(&base));
    assert_eq!(
        (&c["new"], &c["changed"], &c["unchanged"], &c["removed"]),
        (&json!(1), &json!(1), &json!(19), &json!(1))
    );
    assert_eq!(
        c["types"]["MeasureReport"],
        json!({"new": 1, "changed": 1, "unchanged": 6, "removed": 1})
    );
    assert_eq!(
        c["types"]["Location"],
        json!({"new": 0, "changed": 0, "unchanged": 6, "removed": 0})
    );
    assert!(
        printed.contains(&format!(
            "Compared with {}: 1 new, 1 changed, 19 unchanged, 1 removed",
            base.display()
        )),
        "{printed}"
    );
    assert!(
        printed.contains("  MeasureReport: 1 new, 1 changed, 6 unchanged, 1 removed"),
        "{printed}"
    );
    assert!(
        printed.contains(&format!(
            "Wrote 2 new or changed resources (MeasureReport 2) to {}",
            changed.display()
        )),
        "{printed}"
    );
    assert!(
        printed.contains(&format!("Wrote 1 removed ids to {}", removed.display())),
        "{printed}"
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&changed.join("MeasureReport.ndjson")), 0o600);
        assert_eq!(mode(&removed), 0o600);
    }
    // The previous folder is not touched.
    assert_eq!(keys(&base.join("MeasureReport.ndjson")).len(), 8);

    // --changed must be empty unless --overwrite; with it, the types not
    // changed this time are removed from it.
    let out2 = tmp.path().join("out2");
    let o = run(
        &pkg,
        &first,
        &out2,
        &["--previous", s(&base), "--changed", s(&changed)],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(
        text(&o).contains("--changed") && text(&o).contains("not empty"),
        "{}",
        text(&o)
    );
    ok(run(
        &pkg,
        &first,
        &out2,
        &[
            "--previous",
            s(&base),
            "--changed",
            s(&changed),
            "--overwrite",
        ],
    ));
    assert!(ndjson_files(&changed).is_empty());
}

#[test]
fn the_baseline_folder_can_be_out_and_previous_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = icr_dhis2();
    let input = sample_in(tmp.path(), "in");
    let fhir = tmp.path().join("fhir");
    ok(run(&pkg, &input, &fhir, &[]));
    let changed = tmp.path().join("changed");
    let removed = tmp.path().join("removed.txt");
    let report_file = tmp.path().join("report.json");
    let again = |extra: &[&str]| {
        let mut args = vec![
            "--previous",
            s(&fhir),
            "--changed",
            s(&changed),
            "--removed",
            s(&removed),
            "--report",
            s(&report_file),
        ];
        args.extend_from_slice(extra);
        run(&pkg, &input, &fhir, &args)
    };
    // Without --overwrite: refused, and nothing changes.
    let before = snapshot(&fhir);
    let o = again(&[]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(
        text(&o).contains("is the --out folder; give --overwrite"),
        "{}",
        text(&o)
    );
    assert_eq!(snapshot(&fhir), before);
    assert!(!changed.exists() && !removed.exists());

    // The same records again: nothing changed.
    let printed = ok(again(&["--overwrite"]));
    assert!(
        printed.contains("0 new, 0 changed, 21 unchanged, 0 removed"),
        "{printed}"
    );
    assert!(changed.is_dir());
    assert!(ndjson_files(&changed).is_empty());
    assert_eq!(fs::read_to_string(&removed).unwrap(), "");
    assert_eq!(report(&report_file)["comparison"]["unchanged"], 21);
    assert_eq!(snapshot(&fhir), before);

    // One value changes: --out becomes the new baseline, so the run after
    // it finds nothing changed.
    let values_file = input.join("DHIS2Value.ndjson");
    let mut values = read_lines(&values_file);
    values[0]["denominator"] = json!(4);
    write_lines(&values_file, &values);
    let printed = ok(again(&["--overwrite"]));
    assert!(
        printed.contains("0 new, 1 changed, 20 unchanged, 0 removed"),
        "{printed}"
    );
    assert_eq!(keys(&changed.join("MeasureReport.ndjson")).len(), 1);
    let printed = ok(again(&["--overwrite"]));
    assert!(
        printed.contains("0 new, 0 changed, 21 unchanged, 0 removed"),
        "{printed}"
    );
    assert!(ndjson_files(&changed).is_empty());
    let mr = read_lines(&fhir.join("MeasureReport.ndjson"));
    assert_eq!(mr.len(), 8);
    // The comparison went through `./`: still one folder.
    let o = Command::cargo_bin("fhir-pipes")
        .unwrap()
        .current_dir(tmp.path())
        .args(["import", "run", "--package", s(&pkg), "--in", "in"])
        .args(["--out", "fhir", "--previous", "./fhir"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("is the --out folder"), "{}", text(&o));
}

/// A resource as a FHIR server sends it back: keys in another order, and
/// `meta.versionId`, `meta.lastUpdated` and `meta.source` set.
fn as_served(r: &Value) -> Value {
    fn reversed(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let mut m = Map::new();
                for (k, x) in o.iter().rev() {
                    m.insert(k.clone(), reversed(x));
                }
                Value::Object(m)
            }
            Value::Array(a) => Value::Array(a.iter().map(reversed).collect()),
            x => x.clone(),
        }
    }
    let mut r = r.clone();
    let meta = r
        .as_object_mut()
        .unwrap()
        .entry("meta")
        .or_insert_with(|| json!({}));
    meta["versionId"] = json!("7");
    meta["lastUpdated"] = json!("2026-11-01T03:04:05.678Z");
    meta["source"] = json!("#Ab12Cd34");
    reversed(&r)
}

#[test]
fn resources_from_a_server_compare_as_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = icr_dhis2();
    let input = sample_in(tmp.path(), "in");
    let base = tmp.path().join("base");
    ok(run(&pkg, &input, &base, &[]));
    // A folder of the resources as exported from a server, with no
    // .fhir-pipes-import.json: every *.ndjson file in it is read.
    let served = tmp.path().join("served");
    fs::create_dir(&served).unwrap();
    for name in ndjson_files(&base) {
        let resources: Vec<Value> = read_lines(&base.join(&name))
            .iter()
            .map(as_served)
            .collect();
        let text = fs::read_to_string(base.join(&name)).unwrap();
        let first = text.lines().next().unwrap();
        assert_ne!(first, resources[0].to_string(), "the keys are reordered");
        write_lines(&served.join(&name), &resources);
    }
    let printed = ok(run(
        &pkg,
        &input,
        &tmp.path().join("out"),
        &["--previous", s(&served)],
    ));
    assert!(
        printed.contains("0 new, 0 changed, 21 unchanged, 0 removed"),
        "{printed}"
    );
    // Any other change in meta is a change.
    let file = served.join("Location.ndjson");
    let mut locations = read_lines(&file);
    locations[0]["meta"]["tag"] = json!([{"code": "other"}]);
    write_lines(&file, &locations);
    let printed = ok(run(
        &pkg,
        &input,
        &tmp.path().join("out2"),
        &["--previous", s(&served)],
    ));
    assert!(
        printed.contains("  Location: 0 new, 1 changed, 5 unchanged, 0 removed"),
        "{printed}"
    );
}

#[test]
fn the_package_names_elements_that_are_not_compared() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = icr_dhis2();
    let first = sample_in(tmp.path(), "in1");
    let base = tmp.path().join("base");
    ok(run(&pkg, &first, &base, &[]));
    // The next analytics build: every value the same, every date later.
    let second = sample_in(tmp.path(), "in2");
    let values_file = second.join("DHIS2Value.ndjson");
    let mut values = read_lines(&values_file);
    for v in &mut values {
        assert_ne!(v["date"], "2026-10-13T02:07:00.052Z");
        v["date"] = json!("2026-10-13T02:07:00.052Z");
    }
    write_lines(&values_file, &values);
    let out = tmp.path().join("out");
    let printed = ok(run(&pkg, &second, &out, &["--previous", s(&base)]));
    assert!(
        printed.contains("  MeasureReport: 0 new, 0 changed, 8 unchanged, 0 removed"),
        "{printed}"
    );
    // The date did change in --out.
    let mr = read_lines(&out.join("MeasureReport.ndjson"));
    assert!(mr.iter().all(|r| r["date"] == "2026-10-13T02:07:00.052Z"));

    // The same package with no [compare]: every report changed. The
    // parameters stay: the maps read them.
    let bare = tmp.path().join("icr-dhis2");
    copy_dir(&pkg, &bare);
    let manifest = fs::read_to_string(bare.join("import.toml")).unwrap();
    let (without_compare, _) = manifest.split_once("[compare]").unwrap();
    fs::write(bare.join("import.toml"), without_compare).unwrap();
    let printed = ok(run(
        &bare,
        &second,
        &tmp.path().join("out2"),
        &["--previous", s(&base)],
    ));
    assert!(
        printed.contains("  MeasureReport: 0 new, 8 changed, 0 unchanged, 0 removed"),
        "{printed}"
    );
    // A bad path is refused when the manifest is read.
    fs::write(
        bare.join("import.toml"),
        format!("{without_compare}[compare]\nignore = [\"MeasureReport.group[0].measureScore\"]\n"),
    )
    .unwrap();
    let o = run(&bare, &second, &tmp.path().join("out3"), &[]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("arrays are not indexed"), "{}", text(&o));
}

#[test]
fn a_manifest_with_no_steps_runs_each_file_through_its_map() {
    // examples/import/icr-dhis2/import.toml has a name, parameters and
    // [compare], and no steps.
    let tmp = tempfile::tempdir().unwrap();
    let printed = ok(run(
        &icr_dhis2(),
        &sample_in(tmp.path(), "in"),
        &tmp.path().join("out"),
        &["--report", s(&tmp.path().join("report.json"))],
    ));
    assert!(
        printed.contains("step 3 DHIS2ValueToICR: 8 DHIS2Value records → output (MeasureReport 8)"),
        "{printed}"
    );
    assert_eq!(
        report(&tmp.path().join("report.json"))["package"],
        "icr-dhis2"
    );
    assert!(
        report(&tmp.path().join("report.json"))
            .get("comparison")
            .is_none()
    );
}

#[test]
fn changed_and_removed_need_previous() {
    let tmp = tempfile::tempdir().unwrap();
    let input = sample_in(tmp.path(), "in");
    for extra in [
        ["--changed", s(&tmp.path().join("c"))],
        ["--removed", s(&tmp.path().join("r.txt"))],
    ] {
        let o = run(&icr_dhis2(), &input, &tmp.path().join("out"), &extra);
        assert_eq!(o.status.code(), Some(2), "{}", text(&o));
        assert!(text(&o).contains("--previous"), "{}", text(&o));
    }
    // A --previous that is not there.
    let o = run(
        &icr_dhis2(),
        &input,
        &tmp.path().join("out"),
        &["--previous", s(&tmp.path().join("nothing"))],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("no such folder"), "{}", text(&o));
    assert!(!tmp.path().join("out").exists());
}

#[test]
fn a_resource_with_no_id_is_refused_with_its_file_and_line() {
    let tmp = tempfile::tempdir().unwrap();
    let input = sample_in(tmp.path(), "in");
    let base = tmp.path().join("base");
    ok(run(&icr_dhis2(), &input, &base, &[]));
    // In the previous run.
    let file = base.join("Measure.ndjson");
    let mut measures = read_lines(&file);
    measures[2].as_object_mut().unwrap().remove("id");
    write_lines(&file, &measures);
    let o = run(
        &icr_dhis2(),
        &input,
        &tmp.path().join("out"),
        &["--previous", s(&base)],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(
        text(&o).contains(&format!(
            "{}:3: a Measure with no id; --previous compares resources by resourceType and id",
            file.display()
        )),
        "{}",
        text(&o)
    );
    // In this run: a map that writes no id.
    let pkg = tmp.path().join("icr-dhis2");
    copy_dir(&icr_dhis2(), &pkg);
    let map = pkg.join("maps/DHIS2ValueToICR.map");
    let text_of_map = fs::read_to_string(&map).unwrap();
    let rule = "src.id as id -> r.id = id \"id\";";
    assert!(text_of_map.contains(rule));
    fs::write(&map, text_of_map.replacen(rule, "", 1)).unwrap();
    let good = tmp.path().join("good");
    ok(run(&icr_dhis2(), &input, &good, &[]));
    let out = tmp.path().join("out");
    let o = run(&pkg, &input, &out, &["--previous", s(&good)]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(
        text(&o).contains(&format!(
            "{}:1: map DHIS2ValueToICR wrote a MeasureReport with no id",
            input.join("DHIS2Value.ndjson").display()
        )),
        "{}",
        text(&o)
    );
    assert!(!out.exists());
    // Without --previous no id is needed.
    ok(run(&pkg, &input, &out, &[]));
}

#[test]
fn a_failed_run_leaves_out_changed_and_removed_as_they_were() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = icr_dhis2();
    let input = sample_in(tmp.path(), "in");
    let fhir = tmp.path().join("fhir");
    let changed = tmp.path().join("changed");
    let removed = tmp.path().join("removed.txt");
    ok(run(&pkg, &input, &fhir, &[]));
    // A run that changed something, so --changed has a file.
    let values_file = input.join("DHIS2Value.ndjson");
    let mut values = read_lines(&values_file);
    values[0]["numerator"] = json!(2);
    write_lines(&values_file, &values);
    let args = [
        "--previous",
        s(&fhir),
        "--changed",
        s(&changed),
        "--removed",
        s(&removed),
        "--overwrite",
    ];
    ok(run(&pkg, &input, &fhir, &args));
    fs::write(&removed, "MeasureReport/kept\n").unwrap();
    let before = (
        snapshot(&fhir),
        snapshot(&changed),
        fs::read(&removed).unwrap(),
    );
    assert_eq!(ndjson_files(&changed), ["MeasureReport.ndjson"]);

    // A record that does not fit its model.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&values_file)
        .unwrap();
    writeln!(f, r#"{{"resourceType":"DHIS2Value","colour":"red"}}"#).unwrap();
    drop(f);
    let o = run(&pkg, &input, &fhir, &args);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("DHIS2Value.ndjson:9"), "{}", text(&o));
    assert_eq!(
        (
            snapshot(&fhir),
            snapshot(&changed),
            fs::read(&removed).unwrap()
        ),
        before
    );
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

#[test]
fn a_failed_data_test_leaves_out_changed_and_removed_as_they_were() {
    if !have_duckdb("a_failed_data_test_leaves_out_changed_and_removed_as_they_were") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("digit-visits");
    copy_dir(&root().join("examples/import/digit-visits"), &pkg);
    let input = pkg.join("tests/sample/in");
    let fhir = tmp.path().join("fhir");
    let changed = tmp.path().join("changed");
    let removed = tmp.path().join("removed.txt");
    ok(run(&pkg, &input, &fhir, &[]));
    let args = [
        "--previous",
        s(&fhir),
        "--changed",
        s(&changed),
        "--removed",
        s(&removed),
        "--overwrite",
    ];
    let printed = ok(run(&pkg, &input, &fhir, &args));
    assert!(
        printed.contains("0 new, 0 changed, 9 unchanged, 0 removed"),
        "{printed}"
    );
    fs::write(changed.join("Task.ndjson"), "{}\n").unwrap();
    fs::write(&removed, "Task/kept\n").unwrap();
    let before = (
        snapshot(&fhir),
        snapshot(&changed),
        fs::read(&removed).unwrap(),
    );
    let manifest = pkg.join("import.toml");
    let text_of = fs::read_to_string(&manifest).unwrap();
    fs::write(&manifest, text_of.replacen("EXCEPT\n", "UNION ALL\n", 1)).unwrap();
    let o = run(&pkg, &input, &fhir, &args);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert_eq!(
        (
            snapshot(&fhir),
            snapshot(&changed),
            fs::read(&removed).unwrap()
        ),
        before
    );
}
