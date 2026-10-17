//! End-to-end runs of `fhir-pipes refresh`: the views step's tables,
//! partitions and lookups, and values that do not fit their column.

mod common;

use std::fs;

use parquet::file::reader::{FileReader, SerializedFileReader};
use predicates::prelude::*;
use predicates::str::contains as has;
use serde_json::{Value, json};

use common::*;

/// A view that selects nothing still writes its columns, as one empty file.
#[test]
fn an_empty_view_writes_an_empty_file() {
    let (dir, config) = setup_with_raw("\n[catalog]\n");
    let views = views_with(dir.path(), &[], true);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&views)
        .assert()
        .success()
        .stdout(has("IcrEmpty (Patient) → parquet/empty (0 rows)"));
    let file = dir
        .path()
        .join("data/parquet/empty/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet");
    let reader = SerializedFileReader::new(fs::File::open(&file).unwrap()).unwrap();
    assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
    let fields: Vec<String> = reader
        .metadata()
        .file_metadata()
        .schema()
        .get_fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(fields, ["id", "born", "country"]);
    // The catalog has a view for it.
    let sql = fs::read_to_string(dir.path().join("data/catalog.sql")).unwrap();
    assert!(sql.contains("CREATE OR REPLACE VIEW \"empty\""), "{sql}");
}

#[test]
fn a_view_column_named_like_the_partition_column_stops_the_run() {
    let (dir, config) = setup(NOWHERE, "");
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("v.json"),
        json!({"resourceType": "ViewDefinition", "name": "PatientCountry", "resource": "Patient",
            "status": "active", "select": [{"column": [
                {"name": "id", "path": "getResourceKey()"},
                {"name": "country", "path": "address.country.first()"}]}]})
        .to_string(),
    )
    .unwrap();
    let input = inputs(
        dir.path(),
        &[(
            "p.ndjson",
            lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
        )],
    );
    cmd(&config)
        .args(["--only", "views", "--views"])
        .arg(&views)
        .arg("--input")
        .arg(&input)
        .assert()
        .failure()
        .stderr(has(
            "view PatientCountry has a column `country`, which is the partition column",
        ));
    assert!(!dir.path().join("data/parquet/patient_country").exists());
}

#[test]
fn locations_are_found_under_another_partition_column() {
    let (dir, config) = setup(NOWHERE, "\n[partition]\ncolumn = \"part\"\n\n[catalog]\n");
    let root = dir.path();
    let mut o = obs("o1", 3, None);
    o["subject"] = json!({"reference": "Location/kano"});
    let input = inputs(
        root,
        &[
            ("l.ndjson", lines(&snapshot_locations())),
            ("o.ndjson", lines(&[o])),
        ],
    );
    cmd(&config)
        .args(["--only", "locations,views", "--input"])
        .arg(&input)
        .assert()
        .success();
    // The Locations stay partitioned by country; the view table by `part`,
    // its value looked up in the Locations' country.
    assert!(root.join("data/parquet/locations/country=NG").is_dir());
    let obs_rows = table_rows(&root.join("data/parquet/obs"));
    assert_eq!(obs_rows.len(), 1);
    assert_eq!(obs_rows[0].0, "part=NG");
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("data/manifest.json")).unwrap())
            .unwrap();
    assert!(
        manifest["tables"]["locations"]["rows"].as_i64().unwrap() > 0,
        "{manifest}"
    );
    assert_eq!(manifest["tables"]["obs"]["rows"], 1);
    let catalog = fs::read_to_string(root.join("data/catalog.sql")).unwrap();
    assert!(
        catalog.contains("parquet/locations/country=*/**/*.parquet"),
        "{catalog}"
    );
    assert!(
        catalog.contains("parquet/obs/part=*/**/*.parquet"),
        "{catalog}"
    );
}

/// Values that do not match their column's type are counted per table and
/// column, printed once the views step is done and kept in last-refresh.json.
#[test]
fn values_that_do_not_match_their_column_type_are_reported() {
    let (dir, config) = setup(NOWHERE, "");
    let root = dir.path();
    let (views, input) = mismatch_setup(root);
    cmd(&config)
        .args(["--only", "views", "--input"])
        .arg(&input)
        .arg("--views")
        .arg(&views)
        .assert()
        .success()
        .stdout(has("values that did not match their column's type:"))
        .stdout(has(
            "measures.n: 2 values did not fit integer and were written as null",
        ))
        .stdout(has(
            "measures.period_start: 2 dateTime values written as their date",
        ));
    let report: Value = serde_json::from_str(
        &fs::read_to_string(root.join("data/raw/test/last-refresh.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["type_mismatches"],
        json!({"measures": {
            "n": {"type": "integer", "written_as_date": 0, "written_as_null": 2},
            "period_start": {"type": "date", "written_as_date": 2, "written_as_null": 0},
        }})
    );
    let rows = table_rows(&root.join("data/parquet/measures"));
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|(_, r)| (get(r, "n"), get(r, "period_start")))
        .collect();
    assert_eq!(
        got,
        [
            ("5", "2026-06-15"),
            ("null", "2026-06-15"),
            ("null", "2026-06-15")
        ]
    );
}

/// Rows sent to the null partition because the lookup table is missing, or
/// their key is not in it, are counted in a warning.
#[test]
fn rows_without_a_partition_are_counted_in_a_warning() {
    let (dir, config) = setup_with_raw("");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has(
            "warning: parquet/obs: 1 of 3 rows in the null partition (1 with a `location_id` not in locations.id)",
        ));

    fs::remove_dir_all(dir.path().join("data/parquet/locations")).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has(
            "warning: parquet/obs: 3 of 3 rows in the null partition: there is no lookup table parquet/locations to find `location_id` in",
        ));
    // Patient has no lookup rule: no warning for it.
    let out = cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("parquet/patient: "));
}

/// views/ is staged and replaced with the tables: a removed view leaves
/// views/ and the manifest, and a failing run changes neither. The table a
/// removed view wrote is kept, and listed by --check and after a run.
#[test]
fn removed_views_leave_views_and_their_tables_are_listed() {
    let (dir, config) = setup_with_raw("\n[catalog]\nextra = \"extra.sql\"\n");
    let root = dir.path();
    let extra = root.join("extra.sql");
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    let data = root.join("data");
    let with_gone = views_with(root, &["IcrGone"], false);
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&with_gone)
        .assert()
        .success()
        .stdout(has("note:").not());
    assert!(data.join("views/IcrGone.json").is_file());
    assert!(data.join("parquet/gone").is_dir());

    // A run that fails after the views step leaves views/ as it was.
    let with_new = root.join("new-views");
    fs::create_dir_all(&with_new).unwrap();
    for f in fs::read_dir(fixtures().join("views")).unwrap() {
        let f = f.unwrap().path();
        fs::copy(&f, with_new.join(f.file_name().unwrap())).unwrap();
    }
    fs::copy(with_gone.join("IcrGone.json"), with_new.join("IcrNew.json")).unwrap();
    let text = fs::read_to_string(with_new.join("IcrNew.json")).unwrap();
    fs::write(
        with_new.join("IcrNew.json"),
        text.replace("IcrGone", "IcrNew"),
    )
    .unwrap();
    fs::remove_file(&extra).unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&with_new)
        .assert()
        .failure()
        .stderr(has("extra.sql"));
    assert!(data.join("views/IcrGone.json").is_file());
    assert!(!data.join("views/IcrNew.json").exists());
    assert_nothing_staged(&data);

    // The fixture views alone: IcrGone leaves views/ and the manifest's
    // views; its table is kept and named.
    fs::write(&extra, "CREATE OR REPLACE VIEW one AS SELECT 1;\n").unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .success()
        .stdout(has("note: no current view writes parquet/gone; kept"));
    assert!(!data.join("views/IcrGone.json").exists());
    assert!(data.join("views/IcrObs.json").is_file());
    assert!(!data.join(".views.old").exists());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["views"].get("IcrGone").is_none(), "{manifest}");
    assert!(manifest["views"].get("IcrObs").is_some());
    assert_eq!(manifest["tables"]["gone"]["rows"], 2);
    cmd(&config).arg("--check").assert().success().stdout(has(
        "unwritten:  gone (in parquet/, written by no current view; kept)",
    ));
}

/// A table other views look their partition up in is written before them
/// even when the type it comes from sorts after theirs: here a Patient
/// view's table gives the Observations their country.
#[test]
fn a_lookup_table_from_a_later_type_is_written_first() {
    let (dir, config) = setup_with_raw(
        "\n[[partition.lookup]]\nkey = \"patient_key\"\ntable = \"patient_place\"\n",
    );
    let views = dir.path().join("lookup-views");
    fs::create_dir_all(&views).unwrap();
    let view = |name: &str, resource: &str, columns: &str| {
        format!(
            r#"{{"resourceType": "ViewDefinition", "name": "{name}", "status": "active",
                "resource": "{resource}", "select": [{{"column": [{columns}]}}]}}"#
        )
    };
    fs::write(
        views.join("a.json"),
        view(
            "IcrObsByPatient",
            "Observation",
            r#"{"name": "id", "path": "getResourceKey()"},
               {"name": "patient_key", "path": "'p1'"}"#,
        ),
    )
    .unwrap();
    fs::write(
        views.join("b.json"),
        view(
            "IcrPatientPlace",
            "Patient",
            r#"{"name": "patient_key", "path": "getResourceKey()"},
               {"name": "location_id", "path": "'loc-bo'"}"#,
        ),
    )
    .unwrap();
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--views"])
        .arg(&views)
        .assert()
        .success()
        .stdout(has("parquet/patient_place"))
        .stdout(has("parquet/obs_by_patient (3 rows)"));
    let out = String::from_utf8(
        cmd(&config)
            .args(["--only", "views", "--reuse-raw", "--views"])
            .arg(&views)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let place = out.find("parquet/patient_place").unwrap();
    let obs = out.find("parquet/obs_by_patient").unwrap();
    assert!(place < obs, "{out}");
    assert!(!out.contains("warning: parquet/obs_by_patient"), "{out}");
    let rows = table_rows(&dir.path().join("data/parquet/obs_by_patient"));
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|(part, _)| part == "country=SLE"),
        "{rows:?}"
    );
}
