//! Binary-level tests for `fhir-pipes views`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

fn views() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_DATABASE_URL").arg("views");
    c
}

#[test]
fn run_writes_csv_to_stdout() {
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .arg("--input")
        .arg(fixtures().join("raw/Observation.ndjson"))
        .assert()
        .success()
        .stdout(contains("id,location_id,value,issued"))
        .stdout(contains("o2,loc-kono,5,2026-02-03T04:05:06Z"));
}

#[test]
fn run_streams_ndjson() {
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .arg("--input")
        .arg(fixtures().join("raw/Observation.ndjson"))
        .args(["--output", "ndjson"])
        .assert()
        .success()
        .stdout(contains(r#""id":"o3""#))
        .stderr(contains("3 rows"));
}

#[test]
fn run_a_directory_writes_one_parquet_file_per_view() {
    let out = tempfile::tempdir().unwrap();
    // The directory also holds a JSON file that is not a view; it is skipped.
    views()
        .arg("run")
        .arg(fixtures().join("views"))
        .arg("--input")
        .arg(fixtures().join("raw"))
        .args([
            "--output",
            "parquet",
            "--parquet-temporal",
            "native",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success()
        .stderr(contains("IcrObs"))
        .stderr(contains("(3 rows)"));
    assert!(out.path().join("IcrObs.parquet").exists());
    assert!(out.path().join("IcrPatient.parquet").exists());
}

#[test]
fn a_directory_needs_out() {
    views()
        .arg("run")
        .arg(fixtures().join("views"))
        .arg("--input")
        .arg(fixtures().join("raw"))
        .assert()
        .failure()
        .stderr(contains("--out <dir>"));
}

#[test]
fn sql_emits_ddl() {
    views()
        .arg("sql")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .args(["--ddl", "--dialect", "duckdb", "--table", "obs"])
        .assert()
        .success()
        .stdout(contains("CREATE TABLE"))
        .stdout(contains("obs"));
}

#[test]
fn validate_passes_a_good_view_and_fails_a_bad_one() {
    views()
        .arg("validate")
        .arg(fixtures().join("views/Binary-IcrPatient.json"))
        .assert()
        .success()
        .stdout(contains("valid"));

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    fs::write(
        &bad,
        r#"{"resourceType":"ViewDefinition","name":"Bad","status":"active","resource":"Observation",
            "select":[{"column":[{"name":"x","path":"code.text"},{"name":"x","path":"id"}]}]}"#,
    )
    .unwrap();
    views()
        .args(["--no-color", "validate", "--json"])
        .arg(&bad)
        .assert()
        .failure()
        .stdout(contains("FH08"));
}

#[test]
fn lint_shareable_runs_offline() {
    views()
        .args(["lint", "--shareable"])
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .assert()
        .success();
}

#[test]
fn test_runs_the_official_suite() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/sof/tests/spec");
    views()
        .args(["--no-color", "test"])
        .arg(spec)
        .assert()
        .success()
        .stdout(contains("144/144 passed"));
}

#[test]
fn out_that_is_the_input_file_is_refused_and_the_input_survives() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("Patient.ndjson");
    fs::copy(fixtures().join("raw/Patient.ndjson"), &input).unwrap();
    let before = fs::read(&input).unwrap();
    for output in ["ndjson", "csv"] {
        views()
            .arg("run")
            .arg(fixtures().join("views/Binary-IcrPatient.json"))
            .arg("--input")
            .arg(&input)
            .args(["--output", output, "--out"])
            .arg(&input)
            .assert()
            .failure()
            .stderr(contains("is the --input file"));
        assert_eq!(fs::read(&input).unwrap(), before, "--output {output}");
    }
}

#[test]
fn stdin_takes_formatted_json_and_ndjson_for_ndjson_output() {
    let view = fixtures().join("views/Binary-IcrPatient.json");
    let formatted = "{\n  \"resourceType\": \"Patient\",\n  \"id\": \"p1\"\n}\n";
    let bundle = "{\n  \"resourceType\": \"Bundle\",\n  \"entry\": [\n    {\"resource\": {\"resourceType\": \"Patient\", \"id\": \"p1\"}},\n    {\"resource\": {\"resourceType\": \"Patient\", \"id\": \"p2\"}}\n  ]\n}\n";
    let array = "[\n  {\"resourceType\": \"Patient\", \"id\": \"p1\"}\n]\n";
    for (input, ids) in [
        (formatted, vec!["p1"]),
        (bundle, vec!["p1", "p2"]),
        (array, vec!["p1"]),
    ] {
        let a = views()
            .arg("run")
            .arg(&view)
            .args(["--input", "-", "--output", "ndjson"])
            .write_stdin(input)
            .assert()
            .success();
        let out = String::from_utf8_lossy(&a.get_output().stdout).into_owned();
        for id in ids {
            assert!(out.contains(&format!(r#""id":"{id}""#)), "{input}\n{out}");
        }
    }
    let ndjson = fs::read_to_string(fixtures().join("raw/Patient.ndjson")).unwrap();
    views()
        .arg("run")
        .arg(&view)
        .args(["--input", "-", "--output", "ndjson"])
        .write_stdin(format!("\n{ndjson}"))
        .assert()
        .success()
        .stdout(contains(r#""id":"p2""#))
        .stderr(contains("2 rows"));
}

#[test]
#[cfg(not(feature = "postgres"))]
fn db_says_how_to_get_postgres_support() {
    views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .args(["--db", "postgres://localhost/fhir"])
        .assert()
        .failure()
        .stderr(contains("--features postgres"));
}

#[test]
#[cfg(not(feature = "fhir-packages"))]
fn package_lint_says_how_to_get_package_support() {
    views()
        .args(["lint", "--package", "hl7.fhir.r4.core"])
        .arg(fixtures().join("views/Binary-IcrObs.json"))
        .assert()
        .failure()
        .stderr(contains("--features fhir-packages"));
}

/// A transaction Bundle refers to its own entries by `urn:uuid` fullUrl. Read
/// from a file, those references become `Type/id`, as a server stores them,
/// so `getReferenceKey()` works. A urn whose entry has no id, or that is not
/// in the Bundle, is left as is.
#[test]
fn bundle_urn_references_resolve_to_type_and_id() {
    let dir = tempfile::tempdir().unwrap();
    let view = dir.path().join("obs.json");
    fs::write(
        &view,
        r#"{"resourceType": "ViewDefinition", "name": "obs", "resource": "Observation", "status": "active",
            "select": [{"column": [
              {"name": "id", "path": "id"},
              {"name": "patient_id", "path": "subject.getReferenceKey(Patient)"},
              {"name": "subject", "path": "subject.reference"},
              {"name": "performer", "path": "performer.first().reference"}]}]}"#,
    )
    .unwrap();
    let bundle = serde_json::json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {"fullUrl": "urn:uuid:aaaa", "resource": {"resourceType": "Patient", "id": "p1"}},
            {"fullUrl": "urn:uuid:bbbb", "resource": {"resourceType": "Practitioner"}},
            {"fullUrl": "urn:uuid:cccc", "resource": {"resourceType": "Observation", "id": "o1",
                "subject": {"reference": "urn:uuid:aaaa"},
                "performer": [{"reference": "urn:uuid:bbbb"}]}},
            {"fullUrl": "urn:uuid:dddd", "resource": {"resourceType": "Observation", "id": "o2",
                "subject": {"reference": "urn:uuid:not-in-bundle"}}}
        ]
    });
    let json = dir.path().join("bundle.json");
    fs::write(&json, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
    // The same Bundle on one NDJSON line goes through the streaming reader.
    let ndjson = dir.path().join("bundle.ndjson");
    fs::write(&ndjson, format!("{bundle}\n")).unwrap();

    for input in [&json, &ndjson] {
        let a = views()
            .arg("run")
            .arg(&view)
            .arg("--input")
            .arg(input)
            .args(["--output", "ndjson"])
            .assert()
            .success();
        let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&a.get_output().stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows,
            [
                serde_json::json!({"id": "o1", "patient_id": "p1", "subject": "Patient/p1",
                                   "performer": "urn:uuid:bbbb"}),
                serde_json::json!({"id": "o2", "subject": "urn:uuid:not-in-bundle"}),
            ],
            "{}",
            input.display()
        );
    }
}

/// A directory is read with its subdirectories, as fhir-data-pipes'
/// `synthea-hiv/sample_data_segments/segment_*` are laid out; hidden files
/// such as `.export-state.json` are skipped.
#[test]
fn input_directories_are_read_recursively_without_hidden_files() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    fs::create_dir_all(input.join("segment_1/deeper")).unwrap();
    fs::create_dir_all(input.join(".hidden")).unwrap();
    let patient = |id: &str| format!(r#"{{"resourceType": "Patient", "id": "{id}"}}"#);
    fs::write(input.join("top.json"), patient("p1")).unwrap();
    fs::write(input.join("segment_1/a.json"), patient("p2")).unwrap();
    fs::write(
        input.join("segment_1/deeper/b.ndjson"),
        patient("p3") + "\n",
    )
    .unwrap();
    fs::write(input.join(".hidden/c.json"), patient("hidden1")).unwrap();
    fs::write(
        input.join(".export-state.json"),
        r#"{"Patient": "2026-01-01"}"#,
    )
    .unwrap();
    let view = fixtures().join("views/Binary-IcrPatient.json");
    let a = views()
        .arg("run")
        .arg(&view)
        .arg("--input")
        .arg(&input)
        .args(["--output", "ndjson"])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&a.get_output().stdout).into_owned();
    for id in ["p1", "p2", "p3"] {
        assert!(out.contains(&format!(r#""id":"{id}""#)), "{out}");
    }
    assert!(!out.contains("hidden1"), "{out}");
    assert_eq!(out.lines().count(), 3, "{out}");
}

/// A Bundle that is an entry of another resolves its own urns: wrapping it
/// in an outer Bundle that maps the same urn elsewhere changes nothing.
#[test]
fn a_nested_bundle_resolves_its_own_urns() {
    let dir = tempfile::tempdir().unwrap();
    let view = dir.path().join("obs.json");
    fs::write(
        &view,
        r#"{"resourceType": "ViewDefinition", "name": "obs", "resource": "Observation", "status": "active",
            "select": [{"column": [
              {"name": "id", "path": "id"},
              {"name": "patient_id", "path": "subject.getReferenceKey(Patient)"}]}]}"#,
    )
    .unwrap();
    let inner = serde_json::json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"fullUrl": "urn:uuid:p", "resource": {"resourceType": "Patient", "id": "inner-p"}},
            {"fullUrl": "urn:uuid:o", "resource": {"resourceType": "Observation", "id": "o1",
                "subject": {"reference": "urn:uuid:p"}}}
        ]
    });
    let outer = serde_json::json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"fullUrl": "urn:uuid:p", "resource": {"resourceType": "Patient", "id": "outer-p"}},
            {"fullUrl": "urn:uuid:inner", "resource": inner}
        ]
    });
    let json = dir.path().join("outer.json");
    fs::write(&json, outer.to_string()).unwrap();
    let ndjson = dir.path().join("outer.ndjson");
    fs::write(&ndjson, format!("{outer}\n")).unwrap();
    for input in [&json, &ndjson] {
        views()
            .arg("run")
            .arg(&view)
            .arg("--input")
            .arg(input)
            .args(["--output", "ndjson"])
            .assert()
            .success()
            .stdout(r#"{"id":"o1","patient_id":"inner-p"}"#.to_string() + "\n");
    }
}

/// A symlink to a directory is not followed, so a link back up the tree does
/// not read the same files again; a symlinked file is read.
#[test]
#[cfg(unix)]
fn input_directories_skip_symlinked_directories() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    fs::create_dir_all(&input).unwrap();
    fs::write(
        input.join("p.json"),
        r#"{"resourceType": "Patient", "id": "p1"}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("elsewhere.json"),
        r#"{"resourceType": "Patient", "id": "p2"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&input, input.join("again")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere.json"), input.join("linked.json"))
        .unwrap();
    let a = views()
        .arg("run")
        .arg(fixtures().join("views/Binary-IcrPatient.json"))
        .arg("--input")
        .arg(&input)
        .args(["--output", "ndjson"])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&a.get_output().stdout).into_owned();
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(
        out.contains(r#""id":"p1""#) && out.contains(r#""id":"p2""#),
        "{out}"
    );
}

/// Values that do not match their column's type, written with native
/// temporal types and read back: an integral decimal is written as the
/// integer, a dateTime in a date column as its date as written, and what
/// cannot be converted as null, with a count per column on stderr.
#[test]
fn parquet_values_that_do_not_match_their_column_type() {
    use arrow::array::{Array, Date32Array, Int64Array, StringArray, TimestampMicrosecondArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let dir = tempfile::tempdir().unwrap();
    let view = dir.path().join("view.json");
    fs::write(
        &view,
        serde_json::json!({"resourceType": "ViewDefinition", "name": "Measures", "status": "active",
            "resource": "Observation", "select": [{"column": [
                {"name": "id", "path": "id", "type": "id"},
                {"name": "n", "path": "value.ofType(Quantity).value", "type": "integer"},
                {"name": "start", "path": "effective.ofType(Period).start", "type": "date"},
                {"name": "end", "path": "effective.ofType(Period).end", "type": "dateTime"},
            ]}]})
        .to_string(),
    )
    .unwrap();
    let obs = |id: &str, value: &str, start: &str, end: &str| {
        format!(
            r#"{{"resourceType": "Observation", "id": "{id}", "status": "final", "code": {{"text": "x"}},
                "valueQuantity": {{"value": {value}}}, "effectivePeriod": {{"start": "{start}", "end": "{end}"}}}}"#
        )
        .replace('\n', " ")
            + "\n"
    };
    let input = dir.path().join("obs.ndjson");
    fs::write(
        &input,
        [
            obs("a", "5.0", "2026-06-15T23:30:00-05:00", "2026-06-15"),
            obs("b", "5.5", "2026-06", "2026"),
            obs("c", "-3", "2026-06-15", "2026-06-15T08:30:00+01:00"),
        ]
        .concat(),
    )
    .unwrap();
    let out = dir.path().join("out.parquet");
    views()
        .arg("run")
        .arg(&view)
        .arg("--input")
        .arg(&input)
        .args([
            "--output",
            "parquet",
            "--parquet-temporal",
            "native",
            "--out",
        ])
        .arg(&out)
        .assert()
        .success()
        .stderr(contains("values that did not match their column's type:"))
        .stderr(contains(
            "Measures.n: 1 value did not fit integer and was written as null",
        ))
        .stderr(contains(
            "Measures.start: 1 dateTime value written as its date",
        ));

    let batch = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&out).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let col = |name: &str| batch.column_by_name(name).unwrap().clone();
    let ids = col("id");
    let ids = ids.as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!((ids.value(0), ids.value(1), ids.value(2)), ("a", "b", "c"));

    let n = col("n");
    let n = n.as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(n.value(0), 5);
    assert!(n.is_null(1), "5.5 is not rounded");
    assert_eq!(n.value(2), -3);

    // Days since 1970-01-01.
    let day = |y, m, d| {
        (chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
            - chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
        .num_days() as i32
    };
    let start = col("start");
    let start = start.as_any().downcast_ref::<Date32Array>().unwrap();
    // The date as written, not 2026-06-16 in UTC.
    assert_eq!(start.value(0), day(2026, 6, 15));
    // A partial date is floored to the start of the month.
    assert_eq!(start.value(1), day(2026, 6, 1));
    assert_eq!(start.value(2), day(2026, 6, 15));

    let micros = |y, m, d, h: i64| i64::from(day(y, m, d)) * 86_400_000_000 + h * 3_600_000_000;
    let end = col("end");
    let end = end
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    // Date-only and partial values are midnight UTC; an offset is converted.
    assert_eq!(end.value(0), micros(2026, 6, 15, 0));
    assert_eq!(end.value(1), micros(2026, 1, 1, 0));
    assert_eq!(end.value(2), micros(2026, 6, 15, 7) + 30 * 60_000_000);
}

/// More resources than one chunk of lines: every view's rows come in the
/// order of the input, with the types no view reads (Encounter) left out
/// and the other view's type (Observation) between them.
#[test]
fn rows_keep_the_input_order_across_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    fs::create_dir_all(&input).unwrap();
    let mut text = String::new();
    let mut patients = Vec::new();
    for i in 0..45_000 {
        let line = match i % 3 {
            0 => {
                patients.push(format!("p{i}"));
                format!(r#"{{"resourceType":"Patient","id":"p{i}","birthDate":"2020-01-01"}}"#)
            }
            1 => format!(
                r#"{{"resourceType":"Observation","id":"o{i}","status":"final","code":{{"text":"x"}},"valueInteger":{i}}}"#
            ),
            _ => format!(r#"{{"resourceType":"Encounter","id":"e{i}"}}"#),
        };
        text.push_str(&line);
        text.push('\n');
    }
    fs::write(input.join("all.ndjson"), text).unwrap();
    let out = dir.path().join("out");
    views()
        .arg("run")
        .arg(fixtures().join("views"))
        .arg("--input")
        .arg(&input)
        .args(["--threads", "3", "--out"])
        .arg(&out)
        .assert()
        .success()
        .stderr(contains("IcrPatient.csv (15000 rows)"))
        .stderr(contains("IcrObs.csv (15000 rows)"));
    let csv = fs::read_to_string(out.join("IcrPatient.csv")).unwrap();
    let ids: Vec<&str> = csv
        .lines()
        .skip(1)
        .map(|l| l.split(',').next().unwrap())
        .collect();
    assert_eq!(ids, patients);
}
