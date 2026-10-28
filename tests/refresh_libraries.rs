//! The libraries step of `fhir-pipes refresh`: SQLQuery Libraries run over
//! this run's tables with duckdb, their results published with the views'.
//! Each test reads its resources from files (`--input`) and is skipped
//! without duckdb.

mod common;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use predicates::str::contains as has;
use serde_json::{Value, json};

use common::*;

const SQLVIEW: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";
const SQLQUERY: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";
const HIERARCHY: &str = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy";
const READING: &str = "https://example.org/ViewDefinition/IcrReading";

fn have_duckdb(skipped: &str) -> bool {
    let found = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
}

/// One value from a duckdb query, run in `dir`.
fn duckdb_value(dir: &Path, sql: &str) -> String {
    let out = std::process::Command::new("duckdb")
        .args(["-noheader", "-list", "-c", sql])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A SQLView (`view`) or SQLQuery Library named `name`; `deps` are (label,
/// canonical) pairs.
fn library(name: &str, view: bool, deps: &[(&str, &str)], sql: &str) -> Value {
    let related: Vec<Value> = deps
        .iter()
        .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
        .collect();
    json!({
        "resourceType": "Library",
        "meta": {"profile": [if view { SQLVIEW } else { SQLQUERY }]},
        "url": format!("https://example.org/Library/{name}"),
        "name": name,
        "status": "draft",
        "relatedArtifact": related,
        "content": [{"contentType": "application/sql", "data": STANDARD.encode(sql)}],
    })
}

/// The fixture views (without urls, so no Library can name them) and
/// IcrReading, which has one: Observations with their location and value.
fn views(root: &Path) -> PathBuf {
    let dir = root.join("my-views");
    fs::create_dir_all(&dir).unwrap();
    for f in fs::read_dir(fixtures().join("views")).unwrap() {
        let f = f.unwrap().path();
        fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
    }
    let reading = json!({"resourceType": "ViewDefinition", "url": READING, "name": "IcrReading",
    "status": "active", "resource": "Observation", "select": [{"column": [
        {"name": "id", "path": "getResourceKey()", "type": "id"},
        {"name": "location_id", "path": "subject.getReferenceKey(Location)", "type": "string"},
        {"name": "value", "path": "value.ofType(integer)", "type": "integer"},
    ]}]});
    fs::write(dir.join("IcrReading.json"), reading.to_string()).unwrap();
    dir
}

/// A warehouse whose config names `libs/` (relative to it) as a library
/// source, holding `libraries`. Returns (dir, config, views directory).
fn setup_libraries(libraries: &[Value]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (dir, config) = setup(NOWHERE, "");
    let root = dir.path();
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("libraries = ['libs']\n{text}")).unwrap();
    write_libraries(root, libraries);
    let views = views(root);
    (dir, config, views)
}

fn write_libraries(root: &Path, libraries: &[Value]) {
    let libs = root.join("libs");
    let _ = fs::remove_dir_all(&libs);
    fs::create_dir_all(&libs).unwrap();
    for (i, l) in libraries.iter().enumerate() {
        fs::write(libs.join(format!("{i}.json")), l.to_string()).unwrap();
    }
}

fn refresh(config: &Path, views: &Path, input: &Path, only: &str) -> assert_cmd::assert::Assert {
    cmd(config)
        .args(["--only", only, "--views"])
        .arg(views)
        .arg("--input")
        .arg(input)
        .assert()
}

fn totals() -> Value {
    library(
        "ReadingTotals",
        false,
        &[("r", READING)],
        "SELECT country, count(*) AS n, sum(value) AS total FROM r GROUP BY country;",
    )
}

#[test]
fn a_sqlquery_result_is_published_with_the_views() {
    if !have_duckdb("a_sqlquery_result_is_published_with_the_views") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[totals()]);
    let root = dir.path();
    let input = inputs(
        root,
        &[(
            "a.ndjson",
            lines(&[obs("o1", 30, None), obs("o2", 5, None)]),
        )],
    );
    refresh(&config, &views, &input, "views")
        .success()
        .stdout(has("== libraries"))
        .stdout(has("ReadingTotals → parquet/reading_totals"));
    let data = root.join("data");
    // Partitioned by the result's country column, as the views' tables are.
    assert!(
        data.join("parquet/reading_totals/country=SLE/part-0.parquet")
            .is_file()
    );
    assert_eq!(
        duckdb_value(
            &data,
            "SELECT country || ' ' || n || ' ' || CAST(total AS BIGINT) FROM read_parquet(\
             'parquet/reading_totals/**/*.parquet', hive_partitioning = true)"
        ),
        "SLE 2 35"
    );
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["reading_totals"]["rows"], 1);
    assert_eq!(
        last_run(&data)["steps"],
        json!(["views", "libraries", "manifest"])
    );
    assert_nothing_staged(&data);
}

/// The step's duckdb does not read `~/.duckdbrc`, which could change
/// settings or run statements of its own. This one fails any duckdb that
/// reads it.
#[test]
fn the_users_duckdbrc_is_not_read() {
    if !have_duckdb("the_users_duckdbrc_is_not_read") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[totals()]);
    let root = dir.path();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join(".duckdbrc"),
        "SELECT error('~/.duckdbrc was read');\n",
    )
    .unwrap();
    let input = inputs(root, &[("a.ndjson", lines(&[obs("o1", 30, None)]))]);
    cmd(&config)
        .env("HOME", &home)
        .args(["--only", "views", "--views"])
        .arg(&views)
        .arg("--input")
        .arg(&input)
        .assert()
        .success();
    assert!(
        root.join("data/parquet/reading_totals/country=SLE/part-0.parquet")
            .is_file()
    );
}

#[test]
fn results_read_this_runs_tables() {
    if !have_duckdb("results_read_this_runs_tables") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[totals()]);
    let root = dir.path();
    let data = root.join("data");
    let total = || {
        duckdb_value(
            &data,
            "SELECT CAST(total AS BIGINT) FROM read_parquet('parquet/reading_totals/**/*.parquet')",
        )
    };
    let input = inputs(root, &[("a.ndjson", lines(&[obs("o1", 30, None)]))]);
    refresh(&config, &views, &input, "views").success();
    assert_eq!(total(), "30");
    let input = inputs(
        root,
        &[("a.ndjson", lines(&[obs("o1", 7, None), obs("o2", 5, None)]))],
    );
    refresh(&config, &views, &input, "views").success();
    assert_eq!(total(), "12");

    // `--only libraries` runs on the published tables.
    write_libraries(
        root,
        &[library(
            "ReadingTotals",
            false,
            &[("r", READING)],
            "SELECT country, sum(value) + 100 AS total FROM r GROUP BY country",
        )],
    );
    cmd(&config)
        .args(["--only", "libraries", "--views"])
        .arg(&views)
        .assert()
        .success();
    assert_eq!(total(), "112");
}

#[test]
fn an_empty_input_gives_an_empty_result() {
    if !have_duckdb("an_empty_input_gives_an_empty_result") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[
        totals(),
        library(
            "ReadingList",
            false,
            &[("r", READING)],
            "SELECT id, value FROM r",
        ),
    ]);
    let root = dir.path();
    let input = inputs(
        root,
        &[(
            "a.ndjson",
            lines(&[json!({"resourceType": "Patient", "id": "p1"})]),
        )],
    );
    refresh(&config, &views, &input, "views").success();
    let data = root.join("data");
    // With a country column: one empty file in the null partition.
    assert!(
        data.join("parquet/reading_totals/country=__HIVE_DEFAULT_PARTITION__/part-0.parquet")
            .is_file()
    );
    // Without: one empty file.
    assert!(data.join("parquet/reading_list/part-0.parquet").is_file());
    for (table, columns) in [
        ("reading_totals", "country,n,total"),
        ("reading_list", "id,value"),
    ] {
        let from =
            format!("read_parquet('parquet/{table}/**/*.parquet', hive_partitioning = true)");
        assert_eq!(
            duckdb_value(&data, &format!("SELECT count(*) FROM {from}")),
            "0"
        );
        assert_eq!(
            duckdb_value(
                &data,
                &format!(
                    "SELECT string_agg(column_name, ',' ORDER BY column_name) \
                     FROM (DESCRIBE SELECT * FROM {from})"
                )
            ),
            columns
        );
    }
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["tables"]["reading_list"]["rows"], 0);
}

#[test]
fn a_missing_input_fails_the_step_with_its_label() {
    if !have_duckdb("a_missing_input_fails_the_step_with_its_label") {
        return;
    }
    // A view in the library source: the views step does not run it, so it
    // has no table.
    let ghost = "https://example.org/ViewDefinition/IcrGhost";
    let (dir, config, views) = setup_libraries(&[
        json!({"resourceType": "ViewDefinition", "url": ghost, "name": "IcrGhost",
            "status": "active", "resource": "Patient",
            "select": [{"column": [{"name": "id", "path": "getResourceKey()"}]}]}),
        library("Haunted", false, &[("g", ghost)], "SELECT * FROM g"),
    ]);
    let root = dir.path();
    let input = inputs(root, &[("a.ndjson", lines(&[obs("o1", 30, None)]))]);
    refresh(&config, &views, &input, "views")
        .failure()
        .stderr(has("Haunted"))
        .stderr(has(format!(
            "Library Haunted: input g ({ghost}) has no table ghost"
        )));
    let data = root.join("data");
    let run = last_run(&data);
    assert_eq!(run["failed_step"], "libraries");
    assert_eq!(run["published"], false);
    assert!(!data.join("parquet/haunted").exists());
    // Nothing of the run is published.
    assert!(!data.join("parquet/reading").exists());
}

/// A view input whose table is left from an earlier configuration is not
/// read: here IcrReading has moved from the views directory to a library
/// source, and parquet/reading/ is the last views run's.
#[test]
fn a_view_table_left_from_an_earlier_config_is_not_read() {
    if !have_duckdb("a_view_table_left_from_an_earlier_config_is_not_read") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[totals()]);
    let root = dir.path();
    let input = inputs(root, &[("a.ndjson", lines(&[obs("o1", 30, None)]))]);
    refresh(&config, &views, &input, "views").success();
    let data = root.join("data");
    assert!(data.join("parquet/reading").is_dir());

    let reading = fs::read_to_string(views.join("IcrReading.json")).unwrap();
    fs::remove_file(views.join("IcrReading.json")).unwrap();
    let reading: Value = serde_json::from_str(&reading).unwrap();
    write_libraries(root, &[reading, totals()]);
    refresh(&config, &views, &input, "libraries")
        .failure()
        .stderr(has(format!(
            "Library ReadingTotals: input r ({READING}) is not a view in the views directory"
        )))
        .stderr(has("left from an earlier run is not read"));
    assert_eq!(last_run(&data)["failed_step"], "libraries");
}

#[test]
fn order_by_is_kept() {
    if !have_duckdb("order_by_is_kept") {
        return;
    }
    let (dir, config, views) = setup_libraries(&[library(
        "ReadingRank",
        false,
        &[("r", READING)],
        "SELECT id, value FROM r ORDER BY value DESC, id -- biggest first",
    )]);
    let root = dir.path();
    let input = inputs(
        root,
        &[(
            "a.ndjson",
            lines(&[
                obs("o1", 3, None),
                obs("o2", 50, None),
                obs("o3", 7, None),
                obs("o4", 50, None),
                obs("o5", 1, None),
            ]),
        )],
    );
    refresh(&config, &views, &input, "views").success();
    let got: Vec<String> = rows(&root.join("data/parquet/reading_rank/part-0.parquet"))
        .iter()
        .map(|r| get(r, "id").to_string())
        .collect();
    assert_eq!(got, ["o2", "o4", "o3", "o1", "o5"]);
}

#[test]
fn a_byarea_view_joins_the_hierarchy() {
    if !have_duckdb("a_byarea_view_joins_the_hierarchy") {
        return;
    }
    let by_area = library(
        "ReadingByArea",
        true,
        &[("r", READING), ("h", HIERARCHY)],
        "SELECT r.id, r.value, h.admin0_name, h.admin1_name, h.admin2_name \
         FROM r JOIN h ON h.id = r.location_id",
    );
    let query = library(
        "ReadingAreas",
        false,
        &[("b", "https://example.org/Library/ReadingByArea")],
        "SELECT * FROM b ORDER BY id",
    );
    let (dir, config, views) = setup_libraries(&[by_area, query]);
    let root = dir.path();
    let locations = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hierarchy/locations.ndjson"),
    )
    .unwrap();
    let at = |id: &str, loc: &str, v: i64| {
        json!({"resourceType": "Observation", "id": id, "status": "final", "code": {"text": "x"},
            "subject": {"reference": format!("Location/{loc}")}, "valueInteger": v})
    };
    let input = inputs(
        root,
        &[
            ("locations.ndjson", locations),
            (
                "obs.ndjson",
                lines(&[at("o1", "aa-hf1", 4), at("o2", "aa-r1", 9)]),
            ),
        ],
    );
    refresh(&config, &views, &input, "locations,views").success();
    let got: Vec<Vec<String>> = rows(&root.join("data/parquet/reading_areas/part-0.parquet"))
        .iter()
        .map(|r| {
            ["id", "value", "admin0_name", "admin1_name", "admin2_name"]
                .iter()
                .map(|c| get(r, c).to_string())
                .collect()
        })
        .collect();
    assert_eq!(
        got,
        [
            ["o1", "4", "Country A", "Region 1", "District 1"],
            ["o2", "9", "Country A", "Region 1", "null"],
        ]
    );
    // The SQLView is inlined: it writes no table.
    assert!(!root.join("data/parquet/reading_by_area").exists());
}

#[test]
fn a_result_cannot_replace_a_view_or_lookup_table() {
    if !have_duckdb("a_result_cannot_replace_a_view_or_lookup_table") {
        return;
    }
    // IcrObs → obs, the table of the fixture view IcrObs, which has no url;
    // CampaignCalendar → campaign_calendar, a lookup table; Locations →
    // locations, which the locations step writes.
    for (name, said) in [
        (
            "IcrObs",
            "Library IcrObs would write table obs, which is the table of view IcrObs",
        ),
        (
            "CampaignCalendar",
            "Library CampaignCalendar would write table campaign_calendar, which is a \
             [[partition.lookup]] table",
        ),
        (
            "Locations",
            "would write table locations, which is reserved",
        ),
    ] {
        let (dir, config, views) =
            setup_libraries(&[library(name, false, &[("r", READING)], "SELECT id FROM r")]);
        let root = dir.path();
        let text = fs::read_to_string(&config).unwrap();
        fs::write(
            &config,
            format!("{text}\n[[partition.lookup]]\nkey = \"campaign_id\"\ntable = \"campaign_calendar\"\n"),
        )
        .unwrap();
        let input = inputs(root, &[("a.ndjson", lines(&[obs("o1", 30, None)]))]);
        refresh(&config, &views, &input, "views")
            .failure()
            .code(2)
            .stderr(has(said));
        let data = root.join("data");
        let run = last_run(&data);
        assert_eq!(run["failed_step"], "libraries", "{name}");
        assert_eq!(run["published"], false, "{name}");
        // Nothing of the run is published.
        assert!(!data.join("parquet/obs").exists(), "{name}");
        assert!(!data.join("parquet/reading").exists(), "{name}");
    }
}

#[test]
fn results_and_the_hierarchy_are_in_the_catalog() {
    if !have_duckdb("results_and_the_hierarchy_are_in_the_catalog") {
        return;
    }
    // ReadingTotals has a country column (partitioned); ReadingAreas has not
    // (one part-0.parquet).
    let query = library(
        "ReadingAreas",
        false,
        &[("r", READING)],
        "SELECT id, value FROM r ORDER BY id",
    );
    let (dir, config, views) = setup_libraries(&[totals(), query]);
    let root = dir.path();
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{text}\n[catalog]\n")).unwrap();
    let locations = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hierarchy/locations.ndjson"),
    )
    .unwrap();
    let input = inputs(
        root,
        &[
            ("locations.ndjson", locations),
            (
                "obs.ndjson",
                lines(&[obs("o1", 30, None), obs("o2", 5, None)]),
            ),
        ],
    );
    // `--only views` also runs the libraries step, which is why `reading_*` exist.
    refresh(&config, &views, &input, "locations,views").success();
    let data = root.join("data");
    // Partitioned result: a country= directory. Unpartitioned: one file, no x= directories.
    let subdirs = |t: &str| -> Vec<String> {
        fs::read_dir(data.join("parquet").join(t))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    // These Observations have no country, so the partition is the default one.
    let totals_dirs = subdirs("reading_totals");
    assert!(
        totals_dirs.len() == 1 && totals_dirs[0].starts_with("country="),
        "{totals_dirs:?}"
    );
    assert_eq!(subdirs("reading_areas"), ["part-0.parquet"]);
    let sql = fs::read_to_string(data.join("catalog.sql")).unwrap();
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(data.join("manifest.json")).unwrap()).unwrap();
    for (table, count) in [
        // One row per Location in the fixture; none is in a cycle or too deep.
        ("location_hierarchy", "21"),
        ("reading_totals", "1"),
        ("reading_areas", "2"),
    ] {
        assert!(
            sql.contains(&format!("CREATE OR REPLACE VIEW \"{table}\" AS")),
            "{table}: {sql}"
        );
        assert_eq!(
            manifest["tables"][table]["rows"].to_string(),
            count,
            "{table}: {manifest}"
        );
        // The view reads the table's rows.
        assert_eq!(
            duckdb_value(&data, &format!("{sql}\nSELECT count(*) FROM \"{table}\";")),
            count,
            "{table}"
        );
    }
}
