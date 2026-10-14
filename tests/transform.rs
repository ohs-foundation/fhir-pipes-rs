use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;

fn fixture_snapshot() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot")
}

#[test]
fn transform_writes_dataset_and_report() {
    let out = tempfile::tempdir().unwrap();
    let assert = Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Wrote 8 rows across 4 partitions"),
        "{stdout}"
    );
    assert!(stdout.contains("cycle: 2"), "{stdout}");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path().join("_report.json")).unwrap())
            .unwrap();
    assert_eq!(report["counts"]["orphan"], 1);
    assert_eq!(report["counts"]["point_outside_parent"], 1);
    assert_eq!(report["counts"]["duplicate_pcode"], 1);
    assert!(out
        .path()
        .join("locations/country=NG/geom_type=polygon/type=admin-unit/part-0.parquet")
        .exists());
}

#[test]
fn transform_rejects_missing_snapshot_with_exit_2() {
    let out = tempfile::tempdir().unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot", "/nonexistent", "--out"])
        .arg(out.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("locations.ndjson"));
}

#[test]
fn transform_rejects_bad_partition_key_with_exit_2() {
    let out = tempfile::tempdir().unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .args(["--partition-by", "country,nope"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("nope"));
}

#[test]
fn transform_rejects_an_out_path_that_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let out_file = dir.path().join("not-a-dir");
    std::fs::write(&out_file, "not a directory").unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(&out_file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--out"));
}

#[test]
fn a_stale_report_is_replaced() {
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("_report.json"), "stale").unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();
    let text = std::fs::read_to_string(out.path().join("_report.json")).unwrap();
    assert!(text.contains("\"counts\""), "{text}");
}

#[cfg(unix)]
#[test]
fn a_failed_transform_removes_the_stale_report_and_keeps_the_dataset() {
    use std::os::unix::fs::symlink;

    let out = tempfile::tempdir().unwrap();
    let real = out.path().join("empty-real-dir");
    std::fs::create_dir_all(&real).unwrap();
    symlink(&real, out.path().join("locations")).unwrap();
    std::fs::write(out.path().join("_report.json"), "stale").unwrap();

    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .assert()
        .failure()
        .code(2);

    assert!(!out.path().join("_report.json").exists());
    assert!(
        out.path().join("locations").is_symlink(),
        "the dataset symlink must be left untouched"
    );
}

#[test]
fn duckdb_reads_the_output_when_available() {
    if Command::new("duckdb").arg("--version").output().is_err() {
        eprintln!("duckdb not on PATH; skipping");
        return;
    }
    let out = tempfile::tempdir().unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();
    let glob = format!("{}/locations/**/*.parquet", out.path().display());

    // Hard assertion: DuckDB (with no extensions) can read the dataset's
    // columns and see the geometry column's native logical type.
    let sql = format!(
        "SELECT id, admin1_name, admin2_name, geom_type FROM '{glob}' WHERE id='clinic';\n\
         SELECT name, logical_type FROM parquet_schema('{glob}') WHERE name='geometry';"
    );
    let output = Command::new("duckdb")
        .args(["-csv", "-c", &sql])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("clinic,Kano,Nassarawa,point"), "{text}");
    assert!(text.contains("GeometryType"), "{text}");

    // Best-effort: the spatial extension may not be cached in offline CI,
    // so a failure to install/load it skips rather than fails the test.
    let spatial_sql = format!(
        "INSTALL spatial; LOAD spatial; SELECT count(*) FROM '{glob}' WHERE ST_Intersects(geometry, ST_MakeEnvelope(3,6,4,7));"
    );
    let spatial_output = Command::new("duckdb")
        .args(["-csv", "-c", &spatial_sql])
        .output()
        .unwrap();
    if !spatial_output.status.success() {
        eprintln!(
            "duckdb spatial extension unavailable; skipping envelope check: {}",
            String::from_utf8_lossy(&spatial_output.stderr)
        );
        return;
    }
    let spatial_text = String::from_utf8(spatial_output.stdout).unwrap();
    let last_line = spatial_text.lines().last().unwrap_or("").trim();
    // 7 of the 8 retained rows intersect the envelope (clinic, ng, kano,
    // dup, gama, nassarawa, orphan; only "stray" at (50, 50) is outside),
    // counting a boundary touch as an intersection per OGC semantics.
    assert_eq!(last_line, "7", "{spatial_text}");
}

#[test]
fn inspect_summarises_a_written_dataset() {
    let out = tempfile::tempdir().unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .args(["--row-group-size", "2"])
        .assert()
        .success();
    let assert = Command::cargo_bin("kiln")
        .unwrap()
        .args(["inspect", "--out"])
        .arg(out.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("8 rows across 4 partitions"), "{stdout}");
    assert!(
        stdout.contains("locations/country=NG/geom_type=polygon/type=admin-unit/part-0.parquet"),
        "{stdout}"
    );
    // Four admin-unit polygons at two rows per group.
    assert!(stdout.contains("row_groups=2"), "{stdout}");
    assert!(stdout.contains("geo=1.1.0 covering=true"), "{stdout}");
    assert!(stdout.contains("types=Polygon"), "{stdout}");
}

#[test]
fn inspect_of_an_empty_dir_says_so() {
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(out.path().join("locations")).unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["inspect", "--out"])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No parquet files found."));
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
fn inspect_ignores_stale_staging_and_backup_dirs() {
    let out = tempfile::tempdir().unwrap();
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();

    let locations = out.path().join("locations");
    copy_dir_recursive(&locations, &out.path().join(".locations.tmp"));
    copy_dir_recursive(&locations, &out.path().join(".locations.bak"));

    let assert = Command::cargo_bin("kiln")
        .unwrap()
        .args(["inspect", "--out"])
        .arg(out.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("8 rows across 4 partitions"), "{stdout}");
    assert!(!stdout.contains(".locations"), "{stdout}");
}

#[test]
fn inspect_names_a_corrupt_file() {
    let out = tempfile::tempdir().unwrap();
    let locations = out.path().join("locations");
    std::fs::create_dir_all(&locations).unwrap();
    let bad = locations.join("part-0.parquet");
    std::fs::write(&bad, "not parquet").unwrap();

    Command::cargo_bin("kiln")
        .unwrap()
        .args(["inspect", "--out"])
        .arg(out.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(bad.to_string_lossy().to_string()));
}

#[test]
fn inspect_rejects_a_missing_dir_with_exit_2() {
    let out = tempfile::tempdir().unwrap();
    let missing = out.path().join("nonexistent");
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["inspect", "--out"])
        .arg(&missing)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no such directory"));
}

fn facility(id: &str, org: &str, lon: f64) -> String {
    serde_json::json!({"resourceType": "Location", "id": id, "name": id, "status": "active",
        "type": [{"coding": [{"code": "facility"}]}],
        "partOf": {"reference": "Location/kano"},
        "managingOrganization": {"reference": format!("Organization/{org}")},
        "position": {"longitude": lon, "latitude": 6.5}})
    .to_string()
}

fn organization(id: &str, name: &str) -> String {
    serde_json::json!({"resourceType": "Organization", "id": id, "active": true, "name": name,
        "identifier": [{"system": "https://icr.healthcampaigns.org/identifiers/nga-nhfr-code", "value": "05/08/1"},
                       {"system": "https://icr.healthcampaigns.org/identifiers/nga-nhfr-uid", "value": "21526030"}],
        "type": [{"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs", "code": "primary"}], "text": "Health Post"},
                 {"coding": [{"system": "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs", "code": "public"}], "text": "Local Government"}]})
    .to_string()
}

/// Reads one string column of the facility partition into `id -> value`.
fn column(out: &Path, name: &str) -> std::collections::HashMap<String, Option<String>> {
    use arrow_array::cast::AsArray;
    use arrow_array::Array;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(
        out.join("locations/country=NG/geom_type=point/type=facility/part-0.parquet"),
    )
    .unwrap();
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .build()
        .unwrap();
    let mut got = std::collections::HashMap::new();
    for batch in reader {
        let batch = batch.unwrap();
        let ids = batch.column_by_name("id").unwrap().as_string::<i32>();
        let col = batch.column_by_name(name).unwrap().as_string::<i32>();
        for r in 0..batch.num_rows() {
            got.insert(
                ids.value(r).to_string(),
                (!col.is_null(r)).then(|| col.value(r).to_string()),
            );
        }
    }
    got
}

fn transform_into(snap: &Path, out: &Path) {
    Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(snap)
        .arg("--out")
        .arg(out)
        .assert()
        .success();
}

fn report_counts(out: &Path) -> serde_json::Value {
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("_report.json")).unwrap()).unwrap();
    report["counts"].clone()
}

#[test]
fn facility_rows_carry_their_organization() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("snapshot");
    std::fs::create_dir_all(&snap).unwrap();
    let mut locations =
        std::fs::read_to_string(fixture_snapshot().join("locations.ndjson")).unwrap();
    locations.push_str(&facility("paired", "org-paired", 3.4));
    locations.push('\n');
    locations.push_str(&facility("lonely", "org-gone", 3.5));
    locations.push('\n');
    std::fs::write(snap.join("locations.ndjson"), locations).unwrap();
    std::fs::write(
        snap.join("organizations.ndjson"),
        format!(
            "{}\n{}\n",
            organization("org-paired", "paired"),
            organization("org-other", "Other")
        ),
    )
    .unwrap();
    let out = dir.path().join("out");
    transform_into(&snap, &out);

    let nhfr = column(&out, "nhfr_code");
    assert_eq!(nhfr["paired"].as_deref(), Some("05/08/1"));
    assert_eq!(nhfr["lonely"], None);
    assert_eq!(
        nhfr["clinic"], None,
        "a facility with no managingOrganization"
    );
    assert_eq!(
        column(&out, "facility_level_text")["paired"].as_deref(),
        Some("Health Post")
    );
    assert_eq!(
        column(&out, "ownership_text")["paired"].as_deref(),
        Some("Local Government")
    );
    assert!(column(&out, "organization_json")["paired"]
        .as_deref()
        .unwrap()
        .contains("\"id\":\"org-paired\""));
    let counts = report_counts(&out);
    assert_eq!(counts["organization_missing"], 1);
    assert!(counts.get("organization_name_mismatch").is_none());

    // A renamed Organization is drift, reported once.
    std::fs::write(
        snap.join("organizations.ndjson"),
        format!("{}\n", organization("org-paired", "Renamed")),
    )
    .unwrap();
    transform_into(&snap, &out);
    assert_eq!(report_counts(&out)["organization_name_mismatch"], 1);

    // No organizations file at all: columns null, nothing reported.
    std::fs::remove_file(snap.join("organizations.ndjson")).unwrap();
    transform_into(&snap, &out);
    assert_eq!(column(&out, "nhfr_code")["paired"], None);
    assert!(report_counts(&out).get("organization_missing").is_none());
}

/// Every row of the dataset under `out`, as (id, row) with the columns this
/// file checks: geometry is null, and the text of a few others.
fn read_rows(out: &Path) -> std::collections::BTreeMap<String, serde_json::Value> {
    use arrow_array::Array;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn files(dir: &Path, into: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, into);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                into.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    files(&out.join("locations"), &mut paths);
    let mut rows = std::collections::BTreeMap::new();
    for path in paths {
        let file = std::fs::File::open(&path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let text = |name: &str, r: usize| -> serde_json::Value {
                let col = batch.column_by_name(name).unwrap();
                if col.is_null(r) {
                    return serde_json::Value::Null;
                }
                let col = col
                    .as_any()
                    .downcast_ref::<arrow_array::StringArray>()
                    .unwrap();
                col.value(r).into()
            };
            let is_null = |name: &str, r: usize| batch.column_by_name(name).unwrap().is_null(r);
            for r in 0..batch.num_rows() {
                let id = text("id", r).as_str().unwrap().to_string();
                rows.insert(
                    id,
                    serde_json::json!({
                        "file": path.strip_prefix(out).unwrap().to_string_lossy().replace('\\', "/"),
                        "geometry_null": is_null("geometry", r),
                        "geom_type": text("geom_type", r),
                        "lon_null": is_null("lon", r),
                        "lat_null": is_null("lat", r),
                        "bbox_null": is_null("bbox", r),
                        "quadkey_null": is_null("quadkey", r),
                        "country": text("country", r),
                        "tier": text("tier", r),
                        "path": text("path", r),
                        "pcode": text("pcode", r),
                        "admin1_name": text("admin1_name", r),
                        "name": text("name", r),
                    }),
                );
            }
        }
    }
    rows
}

fn transform(out: &Path, extra: &[&str]) -> String {
    let assert = Command::cargo_bin("kiln")
        .unwrap()
        .args(["transform", "--snapshot"])
        .arg(fixture_snapshot())
        .arg("--out")
        .arg(out)
        .args(extra)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

#[test]
fn without_keep_without_geometry_ghost_and_remote_have_no_row() {
    let out = tempfile::tempdir().unwrap();
    let stdout = transform(out.path(), &[]);
    assert!(!stdout.contains("have no geometry"), "{stdout}");
    let rows = read_rows(out.path());
    assert_eq!(rows.len(), 8);
    assert!(!rows.contains_key("ghost"));
    assert!(!rows.contains_key("remote"));
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path().join("_report.json")).unwrap())
            .unwrap();
    assert!(report.get("written_without_geometry").is_none());
    assert_eq!(report["counts"]["no_geometry"], 2);
}

#[test]
fn keep_without_geometry_writes_ghost_and_remote_with_null_geometry() {
    let out = tempfile::tempdir().unwrap();
    let stdout = transform(out.path(), &["--keep-without-geometry"]);
    assert!(
        stdout.contains("Wrote 10 rows across 6 partitions"),
        "{stdout}"
    );
    assert!(
        stdout.contains("2 of these rows have no geometry"),
        "{stdout}"
    );
    let rows = read_rows(out.path());
    assert_eq!(rows.len(), 10);

    let ghost = &rows["ghost"];
    assert_eq!(
        ghost["file"],
        "locations/country=NG/geom_type=null/type=admin-unit/part-0.parquet"
    );
    for col in [
        "geometry_null",
        "lon_null",
        "lat_null",
        "bbox_null",
        "quadkey_null",
    ] {
        assert_eq!(ghost[col], true, "ghost {col}");
    }
    assert_eq!(ghost["geom_type"], serde_json::Value::Null);
    assert_eq!(ghost["country"], "NG");
    assert_eq!(ghost["tier"], "2");
    assert_eq!(ghost["path"], "/ng/kano/ghost");
    assert_eq!(ghost["pcode"], "NG001003");
    assert_eq!(ghost["admin1_name"], "Kano");
    assert_eq!(ghost["name"], "Ghost Ward");

    let remote = &rows["remote"];
    assert_eq!(remote["geometry_null"], true);
    assert_eq!(remote["tier"], "site");
    assert_eq!(
        remote["file"],
        "locations/country=NG/geom_type=null/type=operational-area/part-0.parquet"
    );

    // Rows with geometry are as they were.
    let clinic = &rows["clinic"];
    assert_eq!(clinic["geometry_null"], false);
    assert_eq!(clinic["geom_type"], "point");
    assert_eq!(clinic["bbox_null"], false);

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path().join("_report.json")).unwrap())
            .unwrap();
    assert_eq!(report["written_without_geometry"], 2);
    assert_eq!(report["counts"]["no_geometry"], 2);
    let ghost_issue = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "no_geometry" && i["location_id"] == "ghost")
        .unwrap();
    assert!(
        ghost_issue["detail"]
            .as_str()
            .unwrap()
            .contains("written without geometry"),
        "{ghost_issue}"
    );
}

#[test]
fn without_the_flag_the_geometry_columns_stay_required() {
    // The flag changes the schema (nullable geometry columns), so a run
    // without it must keep the required ones.
    let out = tempfile::tempdir().unwrap();
    transform(out.path(), &[]);
    let path = out
        .path()
        .join("locations/country=NG/geom_type=point/type=facility/part-0.parquet");
    let reader =
        parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&path).unwrap())
            .unwrap();
    use parquet::file::reader::FileReader;
    let fields = reader
        .metadata()
        .file_metadata()
        .schema()
        .get_fields()
        .to_vec();
    for name in ["geom_type", "lon", "lat", "bbox"] {
        let field = fields.iter().find(|f| f.name() == name).unwrap();
        assert!(!field.is_optional(), "{name} should stay required");
    }
}

#[test]
fn duckdb_reads_rows_without_geometry_as_null_when_available() {
    if Command::new("duckdb").arg("--version").output().is_err() {
        eprintln!("duckdb not on PATH; skipping");
        return;
    }
    let out = tempfile::tempdir().unwrap();
    transform(out.path(), &["--keep-without-geometry"]);
    let glob = format!("{}/locations/**/*.parquet", out.path().display());
    let sql = format!(
        "SELECT id, geometry IS NULL, geom_type IS NULL, lon IS NULL, bbox IS NULL, admin1_name \
         FROM read_parquet('{glob}', hive_partitioning=false) ORDER BY id;"
    );
    let output = Command::new("duckdb")
        .args(["-csv", "-noheader", "-c", &sql])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(text.lines().count(), 10, "{text}");
    assert!(text.contains("ghost,true,true,true,true,Kano"), "{text}");
    assert!(text.contains("remote,true,true,true,true,Kano"), "{text}");
    assert!(
        text.contains("clinic,false,false,false,false,Kano"),
        "{text}"
    );
}
