//! The DHIS2 import both ways: for the same fake DHIS2 responses,
//!
//! (a) `fhir-pipes-dhis2 locations|measures|reports --package
//!     examples/import/icr-dhis2 --out a.ndjson`, which maps each record
//!     as it is read, and
//! (b) `fhir-pipes-dhis2 extract locations|measures|reports --out IN` with
//!     the same options, writing `IN/<RecordType>.ndjson`, then
//!     `fhir-pipes import run --package examples/import/icr-dhis2 --in IN`,
//!
//! must give the same resources. The import writes one file per resource
//! type, so the two are compared by `resourceType/id`, as JSON values (key
//! order is not compared), and in the same order within each resource type.
//!
//! What the built-in mapping wrote for these scenarios, before it was
//! removed, is kept as the package's test cases (examples/import/icr-dhis2/
//! tests): `sample`, `data-values` and `batches` here.
//!
//! The stub server helpers are shared with tests/dhis2.rs through
//! tests/common/dhis2.rs; that file's tests are not run again here.
//!
//! These tests run both programs: fhir-pipes-dhis2, and fhir-pipes from the
//! same folder (see `stub::fhir_pipes`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::Assert;
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
const MAPS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/import/icr-dhis2/maps"
);

/// What a scenario writes: resources with `dhis2 <command> --package
/// PACKAGE --out FILE`, or records with `dhis2 extract <command> --out DIR`.
#[derive(Clone, Copy)]
enum To<'a> {
    Resources(&'a Path),
    Records(&'a Path),
}

impl To<'_> {
    /// The --out path.
    fn path(&self) -> &Path {
        match self {
            To::Resources(p) | To::Records(p) => p,
        }
    }
}

/// One run of a dhis2 command: it starts its own stub server, runs the
/// command with `--out` and `extra`, and returns what it printed.
type Run<'a> = &'a dyn Fn(To, &[&str]) -> Assert;

/// `fhir-pipes-dhis2 <command> --package PACKAGE` (or `extract
/// <command>`) against `server`, signed in, with `--out`.
///
/// No `dhis2` command reads the profile: the records carry none of the
/// URLs, and the package's parameters give the maps their values. Both run
/// here with a profile that does not exist.
fn dhis2(server: &Server, command: &str, to: To) -> Command {
    let mut c = fhir_pipes_dhis2();
    c.env("FHIR_PIPES_PROFILE", "no-such-profile.toml");
    if let To::Records(_) = to {
        c.arg("extract");
    }
    c.arg(command);
    if let To::Resources(_) = to {
        c.args(["--package", PACKAGE]);
    }
    c.args(["--dhis2", &base(server), "--instance", "sl-demo"])
        .arg("--out")
        .arg(to.path())
        .env("FHIR_PIPES_DHIS2_TOKEN", "d2pat_test");
    c
}

/// Where a reports scenario reads its org units from.
#[derive(Clone, Copy)]
enum OrgUnits<'a> {
    /// The scenario's own Locations file: `dhis2 reports --locations FILE`,
    /// and `dhis2 extract reports --org-units` with the same org units as
    /// `DHIS2OrgUnit` records.
    Given(&'a Path),
    /// `dhis2 reports --locations FILE`, and `dhis2 extract reports` with
    /// no flag: it reads the `DHIS2OrgUnit.ndjson` that `extract locations`
    /// wrote in its `--out`.
    Extracted(&'a Path),
}

impl OrgUnits<'_> {
    /// The flags that give a reports run its org units.
    fn args(self, to: To) -> Vec<std::ffi::OsString> {
        let flag = |f: &str, p: &Path| vec![f.into(), p.as_os_str().to_owned()];
        match (self, to) {
            (OrgUnits::Given(p) | OrgUnits::Extracted(p), To::Resources(_)) => {
                flag("--locations", p)
            }
            (OrgUnits::Given(p), To::Records(_)) => {
                let records = p.with_file_name("org-units.ndjson");
                flag("--org-units", &org_units_from_locations(p, &records))
            }
            (OrgUnits::Extracted(_), To::Records(_)) => Vec::new(),
        }
    }
}

/// The folder Locations files of a scenario go in: beside its --out.
fn beside(to: To) -> PathBuf {
    let out = to.path();
    let dir = out.parent().unwrap().join(format!(
        "loc-{}",
        out.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn import_run(input: &Path, out: &Path) -> Assert {
    fhir_pipes()
        .args(["import", "run", "--package", PACKAGE, "--in"])
        .arg(input)
        .arg("--out")
        .arg(out)
        .assert()
        .success()
}

/// Every resource in the `*.ndjson` files of `dir`, file by file in name
/// order.
fn read_out(dir: &Path) -> Vec<Value> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ndjson"))
        .collect();
    files.sort();
    files.iter().flat_map(|p| read_ndjson(p)).collect()
}

fn key(r: &Value) -> String {
    format!(
        "{}/{}",
        r["resourceType"].as_str().unwrap(),
        r["id"].as_str().unwrap()
    )
}

/// The ids of each resource type, in the order given.
fn order(resources: &[Value]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in resources {
        out.entry(r["resourceType"].as_str().unwrap().to_string())
            .or_default()
            .push(key(r));
    }
    out
}

/// The same resources by `resourceType/id`, equal as JSON values, and in
/// the same order within each resource type.
fn assert_same(written: &[Value], imported: &[Value]) {
    let a: BTreeMap<String, &Value> = written.iter().map(|r| (key(r), r)).collect();
    let b: BTreeMap<String, &Value> = imported.iter().map(|r| (key(r), r)).collect();
    assert_eq!(a.len(), written.len(), "the command repeats a key");
    assert_eq!(b.len(), imported.len(), "the import repeats a key");
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "the same resources"
    );
    for (k, r) in &a {
        assert_eq!(*r, b[k], "{k}");
    }
    assert_eq!(
        order(written),
        order(imported),
        "the order within each type"
    );
}

/// What a scenario wrote both ways.
struct BothWays {
    /// What `dhis2 <command> --package` wrote.
    written: Vec<Value>,
    imported: Vec<Value>,
    /// What `dhis2 extract` wrote.
    records: Vec<Value>,
    /// What `dhis2 extract` printed.
    records_run: Assert,
}

/// Runs a scenario with `dhis2 <command> --package` and with `dhis2
/// extract` then `import run`, and checks the two agree.
fn both_ways(record_type: &str, run: Run) -> BothWays {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.ndjson");
    run(To::Resources(&a), &[]).success();
    // dhis2 extract makes the folder.
    let input = dir.path().join("in");
    let records_run = run(To::Records(&input), &[]).success();
    let records_file = input.join(format!("{record_type}.ndjson"));
    let names: Vec<String> = std::fs::read_dir(&input)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [format!("{record_type}.ndjson")], "only the records");
    let out = dir.path().join("out");
    let imported = import_run(&input, &out);
    let e = stderr(&imported);
    assert!(!e.contains("is built into fhir-pipes"), "{e}");
    let written = read_ndjson(&a);
    let imported = read_out(&out);
    assert!(!written.is_empty());
    assert_same(&written, &imported);
    BothWays {
        written,
        imported,
        records: read_ndjson(&records_file),
        records_run,
    }
}

/// A test case of the package: `records` are its `in/` file of their type
/// and `resources` its `expected/` files, one per resourceType. The
/// expected files hold what the built-in mapping wrote before it was
/// removed, so they are never written here. With
/// `FHIR_PIPES_UPDATE_SAMPLE=1` the `in/` file is written instead of
/// compared. With
/// `undated`, `date` is left out of the comparison: a run from data values
/// dates its records when it started.
fn package_case(case: &str, records: &[Value], resources: &[Value], undated: bool) {
    let strip = |v: &Value| {
        let mut v = v.clone();
        if undated {
            v.as_object_mut().unwrap().remove("date");
        }
        v
    };
    let dir = Path::new(PACKAGE).join("tests").join(case);
    let update = std::env::var_os("FHIR_PIPES_UPDATE_SAMPLE").is_some();
    let write = |path: &Path, list: &[&Value]| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = list.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(path, text).unwrap();
    };
    let kind = records[0]["resourceType"].as_str().unwrap();
    let file = dir.join("in").join(format!("{kind}.ndjson"));
    if update {
        write(&file, &records.iter().collect::<Vec<_>>());
    }
    assert_eq!(
        read_ndjson(&file).iter().map(strip).collect::<Vec<_>>(),
        records.iter().map(strip).collect::<Vec<_>>(),
        "{}",
        file.display()
    );
    let mut by_type: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for r in resources {
        by_type
            .entry(r["resourceType"].as_str().unwrap())
            .or_default()
            .push(r);
    }
    for (kind, list) in by_type {
        let file = dir.join("expected").join(format!("{kind}.ndjson"));
        let want = read_ndjson(&file);
        assert_eq!(
            want.iter().map(strip).collect::<Vec<_>>(),
            list.iter().map(|r| strip(r)).collect::<Vec<_>>(),
            "{}",
            file.display()
        );
    }
}

// ---------------------------------------------------------------------------
// The scenarios

const SL: &str = "ImspTQPwCqd";
const BO: &str = "O6uvpzGd5pu";
const BOMBALI: &str = "fdc6uOvgoji";
const NGELEHUN: &str = "DiszpKrYNg8";
const NJANDAMA: &str = "YuQRtpLP10I";
const ORPHAN: &str = "vWbkYPRmKyS";
const WARD: &str = "Rp268JB6Ne4";

/// Org units on four levels: the country with no geometry (its boundary is
/// merged from its districts), two districts with polygons, one of them
/// closed, a facility with a point, a closed facility with a point, a
/// facility whose parent is not exported, and a level-4 unit below the
/// facility level. Groups from a group set and a group in no set.
fn locations(to: To, extra: &[&str]) -> Assert {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitLevels.json",
        ))
        .respond_with(ok(json!({"organisationUnitLevels": [
            {"level": 1, "name": "National"}, {"level": 2, "name": "District"},
            {"level": 3, "name": "Facility"}, {"level": 4, "name": "Ward"}]}))),
    );
    let square = |x: f64, y: f64| json!({"type": "Polygon", "coordinates": [[[x, y], [x + 0.5, y], [x + 0.5, y + 0.5], [x, y + 0.5], [x, y]]]});
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnits.json",
        ))
        .respond_with(ok(json!({"pager": {"page": 1, "pageCount": 1}, "organisationUnits": [
            {"id": SL, "code": "OU_525", "name": "Sierra Leone", "shortName": "SL", "level": 1},
            {"id": BO, "code": "OU_264", "name": "Bo", "shortName": "Bo", "level": 2,
             "parent": {"id": SL}, "geometry": square(-11.9, 7.6)},
            {"id": BOMBALI, "name": "Bombali", "level": 2, "parent": {"id": SL},
             "closedDate": "2025-12-31T00:00:00.000", "geometry": square(-12.2, 9.0)},
            {"id": NGELEHUN, "code": "OU_559", "name": "Ngelehun CHC", "shortName": "Ngelehun",
             "level": 3, "parent": {"id": BO},
             "geometry": {"type": "Point", "coordinates": [-11.4197, 8.1039]}},
            {"id": NJANDAMA, "name": "Njandama MCHP", "level": 3, "parent": {"id": BOMBALI},
             "closedDate": "2024-06-30T00:00:00.000",
             "geometry": {"type": "Point", "coordinates": [-12.05, 9.1]}},
            {"id": ORPHAN, "name": "Mokpende MCHP", "level": 3, "parent": {"id": "notExported"}},
            {"id": WARD, "name": "Ward 1", "level": 4, "parent": {"id": NGELEHUN}}
        ]}))),
    );
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitGroupSets/J5jldMd8OHv.json",
        ))
        .respond_with(ok(json!({"id": "J5jldMd8OHv", "name": "Facility Type",
            "organisationUnitGroups": [
                {"id": "CXw2yu5fodb", "name": "CHC", "organisationUnits": [{"id": NGELEHUN}]},
                {"id": "uYxK4wmcPqA", "name": "MCHP",
                 "organisationUnits": [{"id": NJANDAMA}, {"id": ORPHAN}]}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitGroups/hSgatV6qHFI.json",
        ))
        .respond_with(ok(
            json!({"id": "hSgatV6qHFI", "name": "Districts", "groupSets": [],
            "organisationUnits": [{"id": BO}, {"id": BOMBALI}]}),
        )),
    );
    dhis2(&server, "locations", to)
        .args(["--id-prefix", "sle", "--country-code", "SLE"])
        .args(["--facility-level", "3", "--group-set", "J5jldMd8OHv"])
        .args(["--org-unit-group", "hSgatV6qHFI"])
        .args(extra)
        .assert()
}

/// An indicator group with a percent and a plain-number indicator, a data
/// element in a group of a group set, and a data set's reporting rate.
fn measures(to: To, extra: &[&str]) -> Assert {
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
            .respond_with(ok(json!({"indicators": [
                {"id": "tUIlpyeeX9N", "code": "IN_1", "name": "Penta 3 Coverage <1y",
                 "description": "Penta 3 doses over the target population", "annualized": true,
                 "decimals": 1, "lastUpdated": "2025-03-04T05:06:07.000",
                 "indicatorType": {"name": "Per cent", "factor": 100, "number": false},
                 "numerator": "#{a}", "numeratorDescription": "Penta 3 doses",
                 "denominator": "#{b}", "denominatorDescription": "Target population <1y",
                 "indicatorGroups": [{"id": "srxUNt2w5jn", "name": "Immunization"}]},
                {"id": "dy1a1mseGR7", "name": "ANC visits total", "annualized": false,
                 "indicatorType": {"name": "Number", "factor": 1, "number": true},
                 "numerator": "#{c}+#{d}", "denominator": "1",
                 "indicatorGroups": [{"id": "srxUNt2w5jn", "name": "Immunization"}]}
            ]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/dataElements.json"),
            request::query(url_decoded(contains(("filter", "id:in:[s46m5MS0hxu]")))),
        ])
        .respond_with(ok(json!({"dataElements": [
            {"id": "s46m5MS0hxu", "code": "DE_359706", "name": "BCG doses given",
             "aggregationType": "SUM", "lastUpdated": "2024-01-02T03:04:05.000",
             "dataElementGroups": [
                {"id": "h9cuJOkOwY2", "name": "Immunisation", "groupSets": [{"id": "XY1vwCQskjX"}]},
                {"id": "SriP0jBXMr6", "name": "All"}]}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/dataSets.json")).respond_with(
            ok(json!({"dataSets": [{"id": "QX4ZTUbOt3a", "code": "DS_RH",
                "name": "Reproductive Health", "periodType": "Monthly"}]})),
        ),
    );
    dhis2(&server, "measures", to)
        .args([
            "--indicator-group",
            "srxUNt2w5jn",
            "--data-element",
            "s46m5MS0hxu",
        ])
        .args(["--data-set", "QX4ZTUbOt3a"])
        .args(extra)
        .assert()
}

/// The Locations the reports scenarios read: Bo named, Bombali not.
fn report_locations(dir: &Path) -> PathBuf {
    let p = dir.join("locations.ndjson");
    let text = format!(
        "{}\n{}\n",
        json!({"resourceType": "Location", "id": format!("sle-{BO}"), "name": "Bo",
               "identifier": [{"system": ORGUNIT_SYSTEM, "value": BO}]}),
        json!({"resourceType": "Location", "id": format!("sle-{BOMBALI}"),
               "identifier": [{"system": ORGUNIT_SYSTEM, "value": BOMBALI}]})
    );
    std::fs::write(&p, text).unwrap();
    p
}

/// Two percent indicators for two months and two org units, broken down by
/// a category (both options) and an org unit group set (one option), with
/// a campaign. One value has no breakdown, one count is not whole, one org
/// unit has no Location, and one breakdown has no total.
fn reports_by_period(to: To, extra: &[&str]) -> Assert {
    let dir = beside(to);
    let locations = report_locations(&dir);
    reports_by_period_from(to, extra, OrgUnits::Given(&locations))
}

/// `reports_by_period`, reading the org units from `org_units`.
fn reports_by_period_from(to: To, extra: &[&str], org_units: OrgUnits) -> Assert {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 3);
    let t = |dx: &str, pe: &str, ou: &str, v: &str, n: &str, d: &str| json!([dx, pe, ou, v, n, d]);
    let totals = vec![
        t("tUIlpyeeX9N", "202501", BO, "50.0", "25.0", "50.0"),
        t("tUIlpyeeX9N", "202502", BO, "62.5", "12.5", "20.0"),
        t("tUIlpyeeX9N", "202501", BOMBALI, "40.0", "8.0", "20.0"),
        t("tUIlpyeeX9N", "202502", BOMBALI, "100.0", "10.0", "10.0"),
        t("FbKK4ofIv5R", "202501", BO, "33.3333333333", "1.0", "3.0"),
        t(
            "tUIlpyeeX9N",
            "202501",
            "unknownOU01",
            "10.0",
            "1.0",
            "10.0",
        ),
    ];
    let s = |dx: &str, o: &str, pe: &str, ou: &str, v: &str, n: &str, d: &str| {
        json!([dx, o, pe, ou, v, n, d])
    };
    let category = vec![
        s(
            "tUIlpyeeX9N",
            "qkPbeWaFsnU",
            "202501",
            BO,
            "60.0",
            "15.0",
            "25.0",
        ),
        s(
            "tUIlpyeeX9N",
            "VXJy4z1XYPl",
            "202501",
            BO,
            "40.0",
            "10.0",
            "25.0",
        ),
        s(
            "tUIlpyeeX9N",
            "VXJy4z1XYPl",
            "202502",
            BOMBALI,
            "100.0",
            "10.0",
            "10.0",
        ),
        // No total for this one.
        s(
            "FbKK4ofIv5R",
            "qkPbeWaFsnU",
            "202502",
            BO,
            "1.0",
            "1.0",
            "1.0",
        ),
    ];
    let group_set = vec![s(
        "tUIlpyeeX9N",
        "CXw2yu5fodb",
        "202501",
        BO,
        "50.0",
        "25.0",
        "50.0",
    )];
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(not(contains(("dimension", "fMZEcRHuamy"))))),
            request::query(url_decoded(not(contains(("dimension", "J5jldMd8OHv"))))),
        ])
        .times(1)
        .respond_with(ok(json!({
            "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},
                        {"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {
                "202501": {"startDate": "2025-01-01T00:00:00.000", "endDate": "2025-01-31T00:00:00.000"},
                "202502": {"startDate": "2025-02-01T00:00:00.000", "endDate": "2025-02-28T00:00:00.000"}}},
            "rows": totals
        }))),
    );
    let breakdown = |dim: &'static str, name: &str, items: Value, rows: Vec<Value>| {
        let mut meta = items;
        meta[dim] = json!({"name": name});
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/analytics.json"),
                request::query(url_decoded(contains(("dimension", dim)))),
            ])
            .times(1)
            .respond_with(ok(json!({
                "headers": [{"name":"dx"},{"name":dim},{"name":"pe"},{"name":"ou"},
                            {"name":"value"},{"name":"numerator"},{"name":"denominator"}],
                "metaData": {"items": meta},
                "rows": rows
            }))),
        );
    };
    breakdown(
        "fMZEcRHuamy",
        "Location Fixed/Outreach",
        json!({"qkPbeWaFsnU": {"name": "Fixed"}, "VXJy4z1XYPl": {"name": "Outreach"}}),
        category,
    );
    breakdown(
        "J5jldMd8OHv",
        "Facility Type",
        json!({"CXw2yu5fodb": {"name": "CHC"}}),
        group_set,
    );
    dhis2(&server, "reports", to)
        .args(["--indicator-group", "srxUNt2w5jn", "--level", "2"])
        .args(["--period", "202501", "--period", "202502"])
        .args(["--breakdown", "fMZEcRHuamy", "--breakdown", "J5jldMd8OHv"])
        .args(["--campaign", "CarePlan/sle-penta-2025"])
        .args(org_units.args(to))
        .args(extra)
        .assert()
}

/// A data element's count and a data set's reporting rate over a date
/// range, with no breakdown and no campaign.
fn reports_by_range(to: To, extra: &[&str]) -> Assert {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/dataElements.json"))
            .respond_with(ok(json!({"dataElements": [
                {"id": "s46m5MS0hxu", "name": "BCG doses given", "aggregationType": "SUM"}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/dataSets.json")).respond_with(
            ok(json!({"dataSets": [{"id": "QX4ZTUbOt3a", "name": "Reproductive Health"}]})),
        ),
    );
    expect_units(&server, 2, 2);
    server.expect(
        Expectation::matching(by_range("2025-01-01", "2025-03-31"))
            .times(1)
            .respond_with(ok(json!({
                "headers": [{"name":"dx"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
                "metaData": {"items": {}},
                "rows": [["s46m5MS0hxu", BO, "1204.0", "", ""],
                         ["s46m5MS0hxu", BOMBALI, "877.0", "", ""],
                         ["QX4ZTUbOt3a.REPORTING_RATE", BO, "93.33", "28.0", "30.0"]]
            }))),
    );
    let dir = beside(to);
    dhis2(&server, "reports", to)
        .args(["--data-element", "s46m5MS0hxu", "--data-set", "QX4ZTUbOt3a"])
        .args(["--level", "2", "--range", "2025-01-01:2025-03-31"])
        .args(OrgUnits::Given(&report_locations(&dir)).args(to))
        .args(extra)
        .assert()
}

/// `--source data-values`: the fixture of tests/dhis2.rs (a country, two
/// districts, four facilities; doses by Fixed/Outreach, population and
/// coverage), computed by fhir-pipes from raw data values.
fn reports_from_data_values(to: To, extra: &[&str]) -> Assert {
    let server = values_server("SUM", None);
    expect_values(&server, "2025-01-01", "2025-02-28", fixture_values());
    let dir = beside(to);
    dhis2(&server, "reports", to)
        .args(["--indicator", COVERAGE, "--data-element", DOSES])
        .args(["--data-element", POP, "--source", "data-values"])
        .args(["--level", "1", "--level", "2", "--level", "3"])
        .args(["--period", "202501", "--period", "202502"])
        .args(["--breakdown", "fMZEcRHuamy"])
        .args(OrgUnits::Given(&hierarchy(&dir)).args(to))
        .args(extra)
        .assert()
}

// ---------------------------------------------------------------------------
// The two routes agree

#[test]
fn locations_agree() {
    let r = both_ways("DHIS2OrgUnit", &locations);
    // 6 Locations (the level-4 unit is skipped) and 3 facility Organizations.
    assert_eq!(r.written.len(), 9);
    assert_eq!(r.records.len(), 6);
    let s = stdout(&r.records_run);
    assert!(s.contains("Wrote 6 DHIS2OrgUnit records to"), "{s}");
    assert!(s.contains("level 4 Ward: 1 (skipped)"), "{s}");
    // What is reported about the source is the same with dhis2 extract.
    for kind in [
        "boundary_from_children",
        "parent_not_exported",
        "below_facility_level",
    ] {
        assert!(s.contains(kind), "{kind}: {s}");
    }
    let by_id = |id: &str| r.imported.iter().find(|l| l["id"] == id).unwrap();
    assert_eq!(by_id(&format!("sle-{NJANDAMA}"))["status"], "inactive");
    assert!(
        by_id(&format!("sle-{ORPHAN}")).get("partOf").is_none(),
        "the parent is not exported"
    );
}

#[test]
fn measures_agree() {
    let r = both_ways("DHIS2Indicator", &measures);
    assert_eq!(r.written.len(), 4);
    let s = stdout(&r.records_run);
    assert!(
        s.contains("Wrote 4 DHIS2Indicator records to",)
            && s.contains("1 cohort, 1 data element counts, 1 proportion, 1 reporting rates"),
        "{s}"
    );
}

#[test]
fn reports_by_period_agree() {
    let r = both_ways("DHIS2Value", &reports_by_period);
    assert_eq!(r.written.len(), 5);
    let s = stdout(&r.records_run);
    assert!(
        s.contains("Wrote 5 DHIS2Value records to")
            && s.contains("(2 indicators, 2 Locations, 2 periods;"),
        "{s}"
    );
    assert!(s.contains("3 breakdowns attached (2 dimensions)"), "{s}");
    for kind in [
        "unmapped_org_unit",
        "stratum_without_total",
        "fractional_count",
    ] {
        assert!(s.contains(kind), "{kind}: {s}");
    }
    let bo = r
        .records
        .iter()
        .find(|v| v["id"] == format!("sl-demo-tUIlpyeeX9N-{BO}-202501"))
        .unwrap();
    assert_eq!(bo["stratifier"].as_array().unwrap().len(), 2);
    assert_eq!(bo["campaign"], "CarePlan/sle-penta-2025");
}

#[test]
fn reports_by_range_agree() {
    let r = both_ways("DHIS2Value", &reports_by_range);
    assert_eq!(r.written.len(), 3);
    assert!(r.records.iter().all(|v| v.get("campaign").is_none()));
}

#[test]
fn reports_from_data_values_agree_but_for_the_date() {
    // A run from data values dates its reports when it started, so the two
    // runs differ there; the import keeps the date the record carries.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.ndjson");
    reports_from_data_values(To::Resources(&a), &[]).success();
    let input = dir.path().join("in");
    reports_from_data_values(To::Records(&input), &[]).success();
    let records = input.join("DHIS2Value.ndjson");
    let out = dir.path().join("out");
    import_run(&input, &out);
    let undated = |list: Vec<Value>| -> Vec<Value> {
        list.into_iter()
            .map(|mut r| {
                assert!(r["date"].as_str().unwrap().ends_with('Z'));
                r.as_object_mut().unwrap().remove("date");
                r
            })
            .collect()
    };
    let written = undated(read_ndjson(&a));
    assert_eq!(written.len(), 28);
    assert_same(&written, &undated(read_out(&out)));
    // The date in each MeasureReport is the date in its record.
    let dates: Vec<Value> = read_ndjson(&records)
        .iter()
        .map(|r| r["date"].clone())
        .collect();
    let imported: Vec<Value> = read_out(&out).iter().map(|r| r["date"].clone()).collect();
    assert_eq!(dates, imported);
    // The package's case data-values: the records, and the reports with
    // the date of their records.
    let dated: Vec<Value> = written
        .into_iter()
        .zip(&dates)
        .map(|(mut r, d)| {
            r["date"] = d.clone();
            r
        })
        .collect();
    package_case("data-values", &read_ndjson(&records), &dated, true);
}

/// Three `dhis2 extract` runs into one folder: one file per record type,
/// which one `import run` maps to what the three `dhis2 --package` runs
/// write.
/// `extract reports` is given no org units: it reads the
/// `DHIS2OrgUnit.ndjson` that `extract locations` wrote in the folder, as
/// `dhis2 reports` reads the Locations that `dhis2 locations` wrote.
#[test]
fn three_extracts_into_one_folder_import_together() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    let mut written = Vec::new();
    let written_locations = dir.path().join("0.ndjson");
    let reports = |to: To, extra: &[&str]| {
        reports_by_period_from(to, extra, OrgUnits::Extracted(&written_locations))
    };
    for (i, run) in [&locations as Run, &measures, &reports]
        .into_iter()
        .enumerate()
    {
        let a = dir.path().join(format!("{i}.ndjson"));
        run(To::Resources(&a), &[]).success();
        written.extend(read_ndjson(&a));
        run(To::Records(&input), &[]).success();
    }
    // Bo and Bombali have Locations and records; the third org unit has
    // neither.
    let reports = read_ndjson(&dir.path().join("2.ndjson"));
    assert_eq!(reports.len(), 5);
    assert!(
        reports.iter().any(|r| r["subject"]["display"] == "Bombali"),
        "the Location's name, from the record"
    );
    let mut names: Vec<String> = std::fs::read_dir(&input)
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
    let out = dir.path().join("out");
    import_run(&input, &out);
    assert_same(&written, &read_out(&out));
}

/// fhir-pipes runs fhir-pipes-dhis2, from the folder it is in, as its
/// command `dhis2`. `--profile` before the command reaches the connector as
/// `$FHIR_PIPES_PROFILE`, and `--profile` after it as an option; no dhis2
/// command reads either, so a profile that does not exist neither fails
/// nor is warned about. The records are the same as from fhir-pipes-dhis2.
#[test]
fn fhir_pipes_runs_the_connector_as_its_dhis2_command() {
    let dir = tempfile::tempdir().unwrap();
    let run = |mut c: Command, out: &Path, extra: &[&str]| {
        let server = Server::run();
        expect_info(&server);
        expect_group(&server);
        c.args(["extract", "measures", "--dhis2", &base(&server)])
            .args(["--instance", "sl-demo", "--indicator-group", "srxUNt2w5jn"])
            .arg("--out")
            .arg(out)
            .args(extra)
            .env("FHIR_PIPES_DHIS2_TOKEN", "d2pat_test")
            .assert()
            .success()
    };
    let direct = dir.path().join("direct");
    run(fhir_pipes_dhis2(), &direct, &[]);
    let through = dir.path().join("through");
    let mut c = fhir_pipes();
    c.args(["--profile", "no-such-profile.toml", "dhis2"]);
    let a = run(c, &through, &["--profile", "other-missing-profile.toml"]);
    assert!(!stderr(&a).contains("profile"), "{}", stderr(&a));
    let records = read_ndjson(&direct.join("DHIS2Indicator.ndjson"));
    assert_eq!(records.len(), 2);
    assert_eq!(read_ndjson(&through.join("DHIS2Indicator.ndjson")), records);
}

// ---------------------------------------------------------------------------
// The package's test cases

/// The sample case of examples/import/icr-dhis2: its `in/` is what the
/// scenarios above write with `dhis2 extract` (both reports scenarios in one
/// DHIS2Value file), and its `expected/` is what the built-in mapping wrote
/// for them before it was removed, and what `dhis2 --package` writes.
/// `import test` passes on every case of the package.
///
/// With `FHIR_PIPES_UPDATE_SAMPLE=1` the test writes `in/` instead of
/// comparing it. `expected/` is not to be regenerated: a change there is a
/// change in what the package writes, to be made in the maps and checked
/// by hand.
#[test]
fn the_package_sample_is_what_the_commands_write() {
    let sample = Path::new(PACKAGE).join("tests/sample");
    let update = std::env::var_os("FHIR_PIPES_UPDATE_SAMPLE").is_some();
    let runs: [(&str, &[Run]); 3] = [
        ("DHIS2OrgUnit", &[&locations]),
        ("DHIS2Indicator", &[&measures]),
        ("DHIS2Value", &[&reports_by_period, &reports_by_range]),
    ];
    let mut written = Vec::new();
    for (record_type, scenarios) in runs {
        let mut records = Vec::new();
        for run in scenarios {
            let r = both_ways(record_type, *run);
            written.extend(r.written);
            records.extend(r.records);
        }
        let file = sample.join("in").join(format!("{record_type}.ndjson"));
        if update {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let text: String = records.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(&file, text).unwrap();
        }
        assert_eq!(read_ndjson(&file), records, "{}", file.display());
    }
    if update {
        return;
    }
    assert_same(&written, &read_out(&sample.join("expected")));
    let a = fhir_pipes()
        .args(["import", "test", "--package", PACKAGE])
        .assert()
        .success();
    assert!(
        !stderr(&a).contains("is built into fhir-pipes"),
        "{}",
        stderr(&a)
    );
}

// ---------------------------------------------------------------------------
// What dhis2 extract takes

/// `dhis2 extract` has no `--package` (nor its old name `--maps`) or
/// `--param`, `--records` is gone from the FHIR commands, and `extract
/// reports` has none of the options that compare MeasureReports with a
/// previous run. clap refuses each with exit 2.
#[test]
fn extract_refuses_the_options_of_the_fhir_commands() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &[BO]);
    let org_units = org_units_from_locations(&locations, &dir.path().join("org-units.ndjson"));
    let previous = dir.path().join("previous.ndjson");
    std::fs::write(&previous, "").unwrap();
    let p = previous.to_str().unwrap();
    let c = dir.path().join("c.ndjson");
    let r = dir.path().join("removed.txt");
    let reports = |command: &[&str], extra: &[&str]| {
        let (flag, file) = if command[0] == "extract" {
            ("--org-units", &org_units)
        } else {
            ("--locations", &locations)
        };
        fhir_pipes_dhis2()
            .args(command)
            .args(["--dhis2", "https://dhis2.invalid"])
            .args(["--instance", "sl-demo", "--indicator", "tUIlpyeeX9N"])
            .args(["--period", "2025", "--level", "2"])
            .arg(flag)
            .arg(file)
            .arg("--out")
            .arg(dir.path().join("out"))
            .args(extra)
            .assert()
            .code(2)
    };
    let l = locations.to_str().unwrap();
    let cases: [(&str, &[&str]); 9] = [
        ("--package", &["--package", PACKAGE]),
        ("--param", &["--param", "base=https://example.org"]),
        ("--previous", &["--previous", p]),
        ("--changed", &["--changed", c.to_str().unwrap()]),
        ("--removed", &["--removed", r.to_str().unwrap()]),
        ("--updated-since", &["--updated-since", "2026-10-01"]),
        ("--maps", &["--maps", MAPS]),
        // extract reports reads the org unit records, not Locations.
        ("--locations", &["--locations", l]),
        ("--org-unit-system", &["--org-unit-system", ORGUNIT_SYSTEM]),
    ];
    for (flag, extra) in cases {
        let e = stderr(&reports(&["extract", "reports"], extra));
        assert!(
            e.contains(&format!("unexpected argument '{flag}'")),
            "{flag}: {e}"
        );
    }
    let e = stderr(&reports(&["reports"], &["--records"]));
    assert!(e.contains("unexpected argument '--records'"), "{e}");
    let e = stderr(&reports(&["reports"], &["--org-units", l]));
    assert!(e.contains("unexpected argument '--org-units'"), "{e}");
    let a = fhir_pipes_dhis2()
        .args(["extract", "measures", "--dhis2", "https://dhis2.invalid"])
        .args(["--instance", "sl-demo", "--indicator", "tUIlpyeeX9N"])
        .args(["--maps", MAPS, "--out"])
        .arg(dir.path().join("m"))
        .assert()
        .code(2);
    assert!(stderr(&a).contains("unexpected argument '--maps'"));
    // --out is a folder.
    let file = dir.path().join("a-file");
    std::fs::write(&file, "").unwrap();
    let a = fhir_pipes_dhis2()
        .args(["extract", "measures", "--dhis2", "https://dhis2.invalid"])
        .args([
            "--instance",
            "sl-demo",
            "--indicator",
            "tUIlpyeeX9N",
            "--out",
        ])
        .arg(&file)
        .assert()
        .code(2);
    assert!(stderr(&a).contains("is not a folder"), "{}", stderr(&a));
}

/// The FHIR commands need a package, and stop without one, before any
/// request. Neither they nor `dhis2 extract` read a profile, even one that
/// does not exist. `extract reports` needs the org unit records, and says
/// how to get them.
#[test]
fn the_fhir_commands_need_a_package_and_no_command_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &[BO]);
    let out = dir.path().join("out");
    let run = |command: &[&str], profile: Option<&str>, extra: &[&std::ffi::OsStr]| {
        let mut c = fhir_pipes_dhis2();
        match profile {
            Some(p) => c.env("FHIR_PIPES_PROFILE", p),
            None => c.env_remove("FHIR_PIPES_PROFILE"),
        };
        c.args(command)
            .args(["--dhis2", "https://dhis2.invalid"])
            .args(["--instance", "sl-demo", "--indicator", "tUIlpyeeX9N"])
            .args(["--period", "2025", "--level", "2"])
            .args(extra)
            .arg("--out")
            .arg(&out)
            .assert()
            .code(2)
    };
    let e = stderr(&run(
        &["reports"],
        None,
        &["--locations".as_ref(), locations.as_os_str()],
    ));
    assert!(
        e.contains("dhis2 reports needs an import package to write resources with")
            && e.contains("--package DIR")
            && e.contains("$FHIR_PIPES_DHIS2_MAPS")
            && e.contains("fhir-pipes dhis2 extract reports"),
        "{e}"
    );
    // A folder that is not a package.
    let e = stderr(&run(
        &["reports"],
        Some("no-such-profile.toml"),
        &[
            "--locations".as_ref(),
            locations.as_os_str(),
            "--package".as_ref(),
            dir.path().as_os_str(),
        ],
    ));
    assert!(e.contains("has no models/ folder"), "{e}");
    // A package with steps: its records are not mapped one at a time.
    let stepped = dir.path().join("stepped");
    for sub in ["models", "maps"] {
        std::fs::create_dir_all(stepped.join(sub)).unwrap();
        for f in std::fs::read_dir(Path::new(PACKAGE).join(sub)).unwrap() {
            let f = f.unwrap().path();
            std::fs::copy(&f, stepped.join(sub).join(f.file_name().unwrap())).unwrap();
        }
    }
    std::fs::write(
        stepped.join("import.toml"),
        "name = \"stepped\"\n\n[[step]]\nmap = \"DHIS2ValueToICR\"\nfrom = \"DHIS2Value\"\n",
    )
    .unwrap();
    let e = stderr(&run(
        &["reports"],
        None,
        &[
            "--locations".as_ref(),
            locations.as_os_str(),
            "--package".as_ref(),
            stepped.as_os_str(),
        ],
    ));
    assert!(
        e.contains("the package has [[step]]s") && e.contains("fhir-pipes import run"),
        "{e}"
    );
    // An unknown parameter.
    let e = stderr(&run(
        &["reports"],
        None,
        &[
            "--locations".as_ref(),
            locations.as_os_str(),
            "--package".as_ref(),
            PACKAGE.as_ref(),
            "--param".as_ref(),
            "nope=1".as_ref(),
        ],
    ));
    assert!(e.contains("the package has no parameter nope"), "{e}");
    // No --org-units and none in --out.
    let e = stderr(&run(
        &["extract", "reports"],
        Some("no-such-profile.toml"),
        &[],
    ));
    assert!(
        e.contains("dhis2 extract reports needs the org units: there is no DHIS2OrgUnit.ndjson in")
            && e.contains("fhir-pipes dhis2 extract locations --out")
            && e.contains("--org-units FILE")
            && !e.contains("profile"),
        "{e}"
    );
    let missing = dir.path().join("missing.ndjson");
    let e = stderr(&run(
        &["extract", "reports"],
        None,
        &["--org-units".as_ref(), missing.as_os_str()],
    ));
    assert!(
        e.contains("--org-units") && e.contains("no such file"),
        "{e}"
    );
    // A Locations file is not org unit records.
    let e = stderr(&run(
        &["extract", "reports"],
        None,
        &["--org-units".as_ref(), locations.as_os_str()],
    ));
    assert!(
        e.contains("no DHIS2OrgUnit record of instance sl-demo in"),
        "{e}"
    );
    // Records of another instance are not read.
    let other = dir.path().join("other.ndjson");
    std::fs::write(
        &other,
        format!(
            "{}\n",
            json!({"resourceType": "DHIS2OrgUnit", "instance": "other", "idPrefix": "o", "uid": BO})
        ),
    )
    .unwrap();
    let e = stderr(&run(
        &["extract", "reports"],
        None,
        &["--org-units".as_ref(), other.as_os_str()],
    ));
    assert!(
        e.contains("no DHIS2OrgUnit record of instance sl-demo in"),
        "{e}"
    );
}

/// `dhis2 extract reports --resume`: an interrupted run (cut mid-line)
/// leaves its temporary and resume files beside DHIS2Value.ndjson, which
/// `import run` does not read, and resumes to the same bytes and the same
/// summary as an uninterrupted one. Its checkpoint is not resumed by
/// `dhis2 reports` writing to the same file, nor the other way round.
#[test]
fn an_interrupted_extract_resumes_to_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &[BO, BOMBALI, NJANDAMA, ORPHAN]);
    // Written once: the file is part of the --resume plan.
    let org_units = org_units_from_locations(&locations, &dir.path().join("org-units.ndjson"));
    let run = |server: &Server, to: To, extra: &[&str]| {
        let (flag, file) = match to {
            To::Resources(_) => ("--locations", &locations),
            To::Records(_) => ("--org-units", &org_units),
        };
        dhis2(server, "reports", to)
            .args(["--indicator-group", "srxUNt2w5jn"])
            .args(["--level", "2", "--level", "3", "--chunk", "1"])
            .args([
                "--period", "202501", "--period", "202502", "--period", "202503",
            ])
            .args(["--max-rows", "1"])
            .arg(flag)
            .arg(file)
            .args(extra)
            .assert()
    };
    let full_dir = dir.path().join("full");
    let full = full_dir.join("DHIS2Value.ndjson");
    let whole = run(&stub_units(false, 0, None), To::Records(&full_dir), &[]).success();

    let input = dir.path().join("in");
    let out = input.join("DHIS2Value.ndjson");
    let tmp = input.join("DHIS2Value.ndjson.tmp");
    let checkpoint = input.join("DHIS2Value.ndjson.resume");
    let mut server = Server::run();
    expect_units_batches(&server, false, 0, Some(6));
    let failed = run(&server, To::Records(&input), &["--concurrency", "1"]).code(2);
    assert!(
        stderr(&failed).contains("6 of 12 batches written"),
        "{}",
        stderr(&failed)
    );
    let mut partial = std::fs::read(&tmp).unwrap();
    partial.extend_from_slice(br#"{"resourceType":"DHIS2Va"#);
    std::fs::write(&tmp, partial).unwrap();
    // import run reads no records from the folder of an interrupted run.
    let partial = import_run(&input, &dir.path().join("fhir-partial"));
    assert!(
        stdout(&partial).contains("Wrote 0 resources"),
        "{}",
        stdout(&partial)
    );

    // Not resumed by dhis2 reports writing MeasureReports to the same file.
    server.verify_and_clear();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    expect_units(&server, 3, 2);
    let refused = run(&server, To::Resources(&out), &["--resume"]).code(2);
    // The extract run read org unit records, dhis2 reports reads Locations.
    assert!(
        stderr(&refused)
            .contains("was planned differently (locations, org_units, package, records changed)"),
        "{}",
        stderr(&refused)
    );
    assert!(tmp.exists() && checkpoint.exists());

    // Not resumed from other org unit records: a copy is another file.
    server.verify_and_clear();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    expect_units(&server, 3, 2);
    let copy = dir.path().join("org-units-copy.ndjson");
    std::fs::copy(&org_units, &copy).unwrap();
    let refused = dhis2(&server, "reports", To::Records(&input))
        .args(["--indicator-group", "srxUNt2w5jn"])
        .args(["--level", "2", "--level", "3", "--chunk", "1"])
        .args([
            "--period", "202501", "--period", "202502", "--period", "202503",
        ])
        .args(["--max-rows", "1", "--resume", "--org-units"])
        .arg(&copy)
        .assert()
        .code(2);
    assert!(
        stderr(&refused).contains("was planned differently (org_units changed)"),
        "{}",
        stderr(&refused)
    );
    assert!(tmp.exists() && checkpoint.exists());

    server.verify_and_clear();
    expect_units_batches(&server, false, 6, None);
    let resumed = run(&server, To::Records(&input), &["--resume"]).success();
    assert!(
        stderr(&resumed).contains("resuming after batch 6 of 12"),
        "{}",
        stderr(&resumed)
    );
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&full).unwrap());
    assert!(!tmp.exists() && !checkpoint.exists());
    let summary = |a: &Assert| {
        stdout(a)
            .replace(full.to_str().unwrap(), "OUT")
            .replace(out.to_str().unwrap(), "OUT")
    };
    assert_eq!(summary(&resumed), summary(&whole));
    assert!(
        summary(&whole).contains(
            "Wrote 24 DHIS2Value records to OUT (2 indicators, 4 Locations, 3 periods; 24 values from 12 analytics requests)"
        ),
        "{}",
        summary(&whole)
    );

    // The records import to what dhis2 reports --package writes, and both
    // are the package's case batches.
    let written = dir.path().join("written.ndjson");
    run(&stub_units(false, 0, None), To::Resources(&written), &[]).success();
    let imported = dir.path().join("fhir");
    import_run(&input, &imported);
    assert_same(&read_ndjson(&written), &read_out(&imported));
    package_case(
        "batches",
        &read_ndjson(&full),
        &read_ndjson(&written),
        false,
    );

    // A checkpoint written by dhis2 reports is not resumed by dhis2 extract.
    let plain_dir = dir.path().join("plain");
    std::fs::create_dir(&plain_dir).unwrap();
    let plain = plain_dir.join("DHIS2Value.ndjson");
    let mut server = Server::run();
    expect_units_batches(&server, false, 0, Some(6));
    run(&server, To::Resources(&plain), &["--concurrency", "1"]).code(2);
    server.verify_and_clear();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    expect_units(&server, 3, 2);
    let refused = run(&server, To::Records(&plain_dir), &["--resume"]).code(2);
    assert!(
        stderr(&refused)
            .contains("was planned differently (locations, org_units, package, records changed)"),
        "{}",
        stderr(&refused)
    );
}

// ---------------------------------------------------------------------------
// Comparing with a previous run, both ways

/// Two runs of `dhis2 reports`, a day and an analytics build apart, over
/// the same org units and period: one value the same, one changed, one new
/// and one gone. `dhis2 reports --previous --changed --removed` and the
/// general route (`dhis2 extract`, then `import run --previous --changed
/// --removed` with the package, whose `[compare]` leaves out a report's
/// `date`) find the same new, changed and removed reports.
#[test]
fn a_rerun_compares_the_same_both_ways() {
    let (a, b) = (BO, BOMBALI);
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &[a, b]);
    let org_units = org_units_from_locations(&locations, &dir.path().join("org-units.ndjson"));
    let run = |built: &str, rows: Vec<Value>, to: To, extra: &[&str]| {
        let (flag, file) = match to {
            To::Resources(_) => ("--locations", &locations),
            To::Records(_) => ("--org-units", &org_units),
        };
        let server = Server::run();
        expect_info_built(&server, built);
        expect_group(&server);
        expect_units(&server, 2, 2);
        let mut response = analytics_for(rows);
        response["metaData"]["dimensions"] =
            json!({"ou": [a, b], "pe": ["202501"], "dx": [], "co": []});
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/analytics.json"),
                request::query(url_decoded(contains(("dimension", "pe:202501")))),
            ])
            .times(1)
            .respond_with(ok(response)),
        );
        let assert = dhis2(&server, "reports", to)
            .args(["--indicator-group", "srxUNt2w5jn"])
            .args(["--level", "2", "--period", "202501"])
            .arg(flag)
            .arg(file)
            .args(extra)
            .assert()
            .success();
        stdout(&assert)
    };
    let row = |dx: &str, ou: &str, num: f64| {
        json!([
            dx,
            "202501",
            ou,
            format!("{}", num * 10.0),
            format!("{num}"),
            "10.0"
        ])
    };
    let first = vec![
        row("tUIlpyeeX9N", a, 5.0),
        row("tUIlpyeeX9N", b, 3.0),
        row("FbKK4ofIv5R", a, 1.0),
    ];
    let second = vec![
        row("tUIlpyeeX9N", a, 5.0),
        row("tUIlpyeeX9N", b, 4.0),
        row("FbKK4ofIv5R", b, 2.0),
    ];
    let (day1, day2) = ("2026-10-12T02:07:00.052", "2026-10-13T02:07:00.052");

    // dhis2 reports --previous, the baseline rolled in place.
    let reports = dir.path().join("reports.ndjson");
    run(day1, first.clone(), To::Resources(&reports), &[]);
    let changed = dir.path().join("changed.ndjson");
    let removed = dir.path().join("removed.txt");
    let printed = run(
        day2,
        second.clone(),
        To::Resources(&reports),
        &[
            "--previous",
            reports.to_str().unwrap(),
            "--changed",
            changed.to_str().unwrap(),
            "--removed",
            removed.to_str().unwrap(),
        ],
    );
    assert!(
        printed.contains("1 new, 1 changed, 1 unchanged, 1 removed"),
        "{printed}"
    );

    // The general route, with one baseline folder.
    let fhir = dir.path().join("fhir");
    let records = |name: &str, built: &str, rows: Vec<Value>| {
        let input = dir.path().join(name);
        run(built, rows, To::Records(&input), &[]);
        input
    };
    let in1 = records("in1", day1, first);
    import_run(&in1, &fhir);
    let in2 = records("in2", day2, second);
    let import_changed = dir.path().join("import-changed");
    let import_removed = dir.path().join("import-removed.txt");
    let a2 = fhir_pipes()
        .args(["import", "run", "--package", PACKAGE, "--in"])
        .arg(&in2)
        .arg("--out")
        .arg(&fhir)
        .arg("--overwrite")
        .arg("--previous")
        .arg(&fhir)
        .arg("--changed")
        .arg(&import_changed)
        .arg("--removed")
        .arg(&import_removed)
        .assert()
        .success();
    let printed = stdout(&a2);
    assert!(
        printed.contains("1 new, 1 changed, 1 unchanged, 1 removed"),
        "{printed}"
    );

    // The same reports, in the same order.
    let ids = |list: Vec<Value>| -> Vec<String> {
        list.iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let written_changed = ids(read_ndjson(&changed));
    assert_eq!(written_changed.len(), 2);
    assert_eq!(
        ids(read_ndjson(&import_changed.join("MeasureReport.ndjson"))),
        written_changed
    );
    let lines = |p: &Path| -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let written_removed = lines(&removed);
    assert_eq!(written_removed.len(), 1);
    let import_removed: Vec<String> = lines(&import_removed)
        .iter()
        .map(|l| l.strip_prefix("MeasureReport/").unwrap().to_string())
        .collect();
    assert_eq!(import_removed, written_removed);
    // And the same baseline.
    assert_same(&read_ndjson(&reports), &read_out(&fhir));
}
