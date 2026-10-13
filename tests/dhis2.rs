//! Binary-level tests for `kiln dhis2` against a fake DHIS2 Web API.

use std::path::Path;
use std::process::Command;

use assert_cmd::assert::Assert;
use assert_cmd::prelude::*;
use httptest::{all_of, cycle, matchers::*, responders::*, Expectation, Server};
use serde_json::{json, Value};

const ORGUNIT_SYSTEM: &str = "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo/orgunit";

fn kiln() -> Command {
    let mut c = Command::cargo_bin("kiln").unwrap();
    for var in [
        "KILN_DHIS2_URL",
        "KILN_DHIS2_TOKEN",
        "KILN_DHIS2_USER",
        "KILN_DHIS2_PASSWORD",
    ] {
        c.env_remove(var);
    }
    c
}

fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn read_ndjson(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ok(v: Value) -> impl httptest::responders::Responder {
    status_code(200)
        .insert_header("content-type", "application/json")
        .body(v.to_string())
}

/// `system/info`, signed in with the personal access token.
fn expect_info(server: &Server) {
    expect_info_built(server, "2026-10-12T02:07:00.052");
}

/// `system/info` with the analytics tables last built at `built`.
fn expect_info_built(server: &Server, built: &str) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/system/info.json"),
            request::headers(contains(("authorization", "ApiToken d2pat_test"))),
        ])
        .times(1..)
        .respond_with(ok(json!({
            "version": "2.41.10",
            "systemName": "Test Sierra Leone",
            "serverTimeZoneId": "Etc/UTC",
            "lastAnalyticsTableSuccess": built
        }))),
    );
}

fn base(server: &Server) -> String {
    server.url("/dhis").to_string()
}

fn indicators() -> Value {
    json!({"indicators": [
        {"id":"tUIlpyeeX9N","code":"IN_1","name":"Penta 3 Coverage <1y","annualized":true,
         "indicatorType":{"name":"Per cent","factor":100,"number":false},
         "numerator":"#{a}","denominator":"#{b}","indicatorGroups":[{"id":"srxUNt2w5jn","name":"Immunization"}]},
        {"id":"FbKK4ofIv5R","name":"Measles Coverage <1y","annualized":true,
         "indicatorType":{"name":"Per cent","factor":100,"number":false},
         "numerator":"#{c}","denominator":"#{b}","indicatorGroups":[{"id":"srxUNt2w5jn","name":"Immunization"}]}
    ]})
}

fn expect_group(server: &Server) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/indicatorGroups.json"),
            request::query(url_decoded(contains(("filter", "id:in:[srxUNt2w5jn]")))),
        ])
        .respond_with(ok(json!({"indicatorGroups": [{"id": "srxUNt2w5jn"}]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/indicators.json"),
            request::query(url_decoded(contains((
                "filter",
                "indicatorGroups.id:in:[srxUNt2w5jn]"
            )))),
        ])
        .respond_with(ok(indicators())),
    );
}

#[test]
fn locations_pages_through_org_units_and_pairs_facilities() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitLevels.json",
        ))
        .respond_with(ok(json!({"organisationUnitLevels": [
            {"level": 1, "name": "National"}, {"level": 2, "name": "District"}, {"level": 3, "name": "Facility"}
        ]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/organisationUnits.json"),
            request::query(url_decoded(contains(("page", "1")))),
        ])
        .respond_with(ok(json!({
            "pager": {"page": 1, "pageCount": 2},
            "organisationUnits": [
                {"id": "ImspTQPwCqd", "code": "OU_525", "name": "Sierra Leone", "level": 1},
                {"id": "O6uvpzGd5pu", "name": "Bo", "level": 2, "parent": {"id": "ImspTQPwCqd"},
                 "geometry": {"type": "Polygon", "coordinates": [[[-11.7, 7.9], [-11.6, 7.9], [-11.6, 8.0], [-11.7, 7.9]]]}}
            ]
        }))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/organisationUnits.json"),
            request::query(url_decoded(contains(("page", "2")))),
        ])
        .respond_with(ok(json!({
            "pager": {"page": 2, "pageCount": 2},
            "organisationUnits": [
                {"id": "DiszpKrYNg8", "name": "Ngelehun CHC", "level": 3, "parent": {"id": "O6uvpzGd5pu"},
                 "geometry": {"type": "Point", "coordinates": [-11.4197, 8.1039]}}
            ]
        }))),
    );
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("locations.ndjson");
    let a = kiln()
        .args([
            "dhis2",
            "locations",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args(["--id-prefix", "sle", "--country-code", "SLE", "--out"])
        .arg(&out)
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .success();
    let s = stdout(&a);
    assert!(s.contains("Wrote 4 resources"), "{s}");
    assert!(s.contains("level 3 Facility: 1 (facilities)"), "{s}");
    let r = read_ndjson(&out);
    let ids: Vec<&str> = r.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "sle-ImspTQPwCqd",
            "sle-O6uvpzGd5pu",
            "org-sle-DiszpKrYNg8",
            "sle-DiszpKrYNg8"
        ]
    );
    assert_eq!(r[1]["partOf"]["reference"], "Location/sle-ImspTQPwCqd");
    assert_eq!(
        r[3]["managingOrganization"]["reference"],
        "Organization/org-sle-DiszpKrYNg8"
    );
    assert_eq!(r[3]["identifier"][0]["system"], ORGUNIT_SYSTEM);
    assert_eq!(r[3]["identifier"][0]["value"], "DiszpKrYNg8");
}

#[test]
fn measures_are_written_for_every_indicator_in_the_group() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("measures.ndjson");
    let a = kiln()
        .args([
            "dhis2",
            "measures",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args(["--indicator-group", "srxUNt2w5jn", "--out"])
        .arg(&out)
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .success();
    assert!(stdout(&a).contains("Wrote 2 Measures"));
    let m = read_ndjson(&out);
    assert_eq!(m[0]["id"], "sl-demo-FbKK4ofIv5R");
    assert_eq!(
        m[1]["url"],
        "https://icr.healthcampaigns.org/Measure/sl-demo-tUIlpyeeX9N"
    );
    assert_eq!(m[1]["scoring"]["coding"][0]["code"], "proportion");
}

#[test]
fn an_unknown_indicator_is_a_usage_error() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/indicators.json"))
            .respond_with(ok(json!({"indicators": []}))),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = kiln()
        .args([
            "dhis2",
            "measures",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args(["--indicator", "nope0000000", "--out"])
        .arg(dir.path().join("m.ndjson"))
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .code(2);
    assert!(stderr(&a).contains("no indicator nope0000000"));
}

#[test]
fn refused_credentials_stop_at_sign_in() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/system/info.json"))
            .respond_with(status_code(401)),
    );
    let dir = tempfile::tempdir().unwrap();
    let a = kiln()
        .args([
            "dhis2",
            "measures",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args(["--indicator-group", "srxUNt2w5jn", "--out"])
        .arg(dir.path().join("m.ndjson"))
        .args(["--dhis2-user", "admin", "--dhis2-password", "wrong"])
        .assert()
        .code(2);
    assert!(stderr(&a).contains("refused the request"), "{}", stderr(&a));
}

fn analytics(rows: Value) -> Value {
    json!({
        "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},
                    {"name":"denominator"},{"name":"factor"},{"name":"multiplier"},{"name":"divisor"}],
        "metaData": {"items": {
            "2025": {"startDate": "2025-01-01T00:00:00.000", "endDate": "2025-12-31T00:00:00.000"}
        }},
        "rows": rows
    })
}

/// The org unit count kiln sizes analytics requests from.
fn expect_units(server: &Server, level: u32, total: u64) {
    let filter: &'static str = Box::leak(format!("level:eq:{level}").into_boxed_str());
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/organisationUnits.json"),
            request::query(url_decoded(contains(("filter", filter)))),
            request::query(url_decoded(contains(("fields", "id")))),
            request::query(url_decoded(contains(("pageSize", "1")))),
        ])
        .times(1..)
        .respond_with(ok(json!({
            "pager": {"page": 1, "pageCount": total, "total": total, "pageSize": 1},
            "organisationUnits": [{"id": "ImspTQPwCqd"}]
        }))),
    );
}

#[test]
fn reports_split_a_too_large_request_and_skip_unmapped_org_units() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 14);
    // Both indicators in one request is over the row limit; one at a time is not.
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "dx:FbKK4ofIv5R;tUIlpyeeX9N")))),
            request::query(url_decoded(contains(("dimension", "ou:LEVEL-2")))),
            request::query(url_decoded(contains(("includeNumDen", "true")))),
            request::query(url_decoded(contains(("skipRounding", "true")))),
            request::headers(contains(("cache-control", "no-cache"))),
        ])
        .respond_with(status_code(409).body(
            r#"{"httpStatusCode":409,"errorCode":"E7128","message":"Query result set exceeded max limit"}"#,
        )),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "dx:FbKK4ofIv5R")))),
        ])
        .respond_with(ok(analytics(json!([[
            "FbKK4ofIv5R",
            "2025",
            "O6uvpzGd5pu",
            "26.6",
            "59133.0",
            "221781.0",
            "100.0",
            "36500",
            "365"
        ]])))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "dx:tUIlpyeeX9N")))),
        ])
        .respond_with(cycle![ok(analytics(json!([
            [
                "tUIlpyeeX9N",
                "2025",
                "O6uvpzGd5pu",
                "27.98",
                "62045.0",
                "221781.0",
                "100.0",
                "36500",
                "365"
            ],
            [
                "tUIlpyeeX9N",
                "2025",
                "unknownOU01",
                "10.0",
                "1.0",
                "10.0",
                "100.0",
                "36500",
                "365"
            ]
        ])))]),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = dir.path().join("locations.ndjson");
    std::fs::write(
        &locations,
        format!(
            "{}\n",
            json!({"resourceType": "Location", "id": "sle-O6uvpzGd5pu",
                   "identifier": [{"system": ORGUNIT_SYSTEM, "value": "O6uvpzGd5pu"}]})
        ),
    )
    .unwrap();
    let out = dir.path().join("reports.ndjson");
    let report = dir.path().join("report.json");
    let a = kiln()
        .args([
            "dhis2",
            "reports",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args([
            "--indicator-group",
            "srxUNt2w5jn",
            "--period",
            "2025",
            "--level",
            "2",
        ])
        .arg("--locations")
        .arg(&locations)
        .arg("--out")
        .arg(&out)
        .arg("--report")
        .arg(&report)
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .success();
    assert!(
        stderr(&a).contains("splitting 2 indicators"),
        "{}",
        stderr(&a)
    );
    assert!(
        stdout(&a).contains("Wrote 2 MeasureReports"),
        "{}",
        stdout(&a)
    );
    let r = read_ndjson(&out);
    assert_eq!(r[0]["id"], "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-2025");
    assert_eq!(r[1]["id"], "sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-2025");
    assert_eq!(r[1]["subject"]["reference"], "Location/sle-O6uvpzGd5pu");
    assert_eq!(r[1]["period"]["end"], "2025-12-31");
    assert_eq!(r[1]["date"], "2026-10-12T02:07:00.052Z");
    assert_eq!(r[1]["group"][0]["measureScore"]["value"], 0.2798);
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(rep["counts"]["unmapped_org_unit"], 1);
}

#[test]
fn reporting_rates_and_breakdowns_come_out_as_valid_ids_and_stratifiers() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/dataSets.json")).respond_with(
            ok(json!({"dataSets": [{"id": "QX4ZTUbOt3a", "name": "Reproductive Health"}]})),
        ),
    );
    expect_units(&server, 2, 14);
    let headers = json!([{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}]);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(not(contains(("dimension", "fMZEcRHuamy"))))),
        ])
        .respond_with(ok(json!({"headers": headers,
            "metaData": {"items": {"2025": {"startDate": "2025-01-01T00:00:00.000", "endDate": "2025-12-31T00:00:00.000"}}},
            "rows": [["QX4ZTUbOt3a.REPORTING_RATE", "2025", "O6uvpzGd5pu", "83.61", "11599.0", "13872.0"]]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "fMZEcRHuamy")))),
        ])
        .respond_with(ok(json!({
            "headers": [{"name":"dx"},{"name":"fMZEcRHuamy"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {"fMZEcRHuamy": {"name": "Location Fixed/Outreach"}, "qkPbeWaFsnU": {"name": "Fixed"}}},
            "rows": [["QX4ZTUbOt3a.REPORTING_RATE", "qkPbeWaFsnU", "2025", "O6uvpzGd5pu", "80.0", "8.0", "10.0"]]}))),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = dir.path().join("locations.ndjson");
    std::fs::write(
        &locations,
        format!(
            "{}\n",
            json!({"resourceType": "Location", "id": "sle-O6uvpzGd5pu", "name": "Bo",
                               "identifier": [{"system": ORGUNIT_SYSTEM, "value": "O6uvpzGd5pu"}]})
        ),
    )
    .unwrap();
    let out = dir.path().join("reports.ndjson");
    let a = kiln()
        .args([
            "dhis2",
            "reports",
            "--dhis2",
            &base(&server),
            "--instance",
            "sl-demo",
        ])
        .args([
            "--data-set",
            "QX4ZTUbOt3a",
            "--period",
            "2025",
            "--level",
            "2",
            "--breakdown",
            "fMZEcRHuamy",
        ])
        .arg("--locations")
        .arg(&locations)
        .arg("--out")
        .arg(&out)
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .success();
    assert!(
        stdout(&a).contains("1 breakdowns attached"),
        "{}",
        stdout(&a)
    );
    let r = read_ndjson(&out);
    assert_eq!(
        r[0]["id"],
        "sl-demo-QX4ZTUbOt3a.REPORTING-RATE-O6uvpzGd5pu-2025"
    );
    assert_eq!(
        r[0]["measure"],
        "https://icr.healthcampaigns.org/Measure/sl-demo-QX4ZTUbOt3a.REPORTING-RATE"
    );
    assert_eq!(r[0]["subject"]["display"], "Bo");
    let g = &r[0]["group"][0];
    assert_eq!(g["population"][0]["count"], 11599);
    assert_eq!(g["measureScore"]["value"], 0.8361);
    assert_eq!(
        g["stratifier"][0]["code"][0]["text"],
        "Location Fixed/Outreach"
    );
    assert_eq!(g["stratifier"][0]["stratum"][0]["value"]["text"], "Fixed");
    assert_eq!(
        g["stratifier"][0]["stratum"][0]["measureScore"]["value"],
        0.8
    );
}

#[test]
fn reports_need_locations_carrying_this_instances_identifier() {
    let dir = tempfile::tempdir().unwrap();
    let locations = dir.path().join("locations.ndjson");
    std::fs::write(
        &locations,
        "{\"resourceType\":\"Location\",\"id\":\"nga\",\"identifier\":[{\"system\":\"other\",\"value\":\"x\"}]}\n",
    )
    .unwrap();
    let a = kiln()
        .args([
            "dhis2",
            "reports",
            "--dhis2",
            "https://dhis2.invalid",
            "--instance",
            "sl-demo",
        ])
        .args([
            "--indicator",
            "tUIlpyeeX9N",
            "--period",
            "2025",
            "--level",
            "2",
        ])
        .arg("--locations")
        .arg(&locations)
        .arg("--out")
        .arg(dir.path().join("r.ndjson"))
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("kiln dhis2 locations --instance sl-demo"),
        "{}",
        stderr(&a)
    );
}

/// A Locations file mapping each org unit to `sle-<uid>`.
fn locations_for(dir: &Path, org_units: &[&str]) -> std::path::PathBuf {
    let p = dir.join("locations.ndjson");
    let lines: String = org_units
        .iter()
        .map(|ou| {
            format!(
                "{}\n",
                json!({"resourceType": "Location", "id": format!("sle-{ou}"), "name": ou,
                       "identifier": [{"system": ORGUNIT_SYSTEM, "value": ou}]})
            )
        })
        .collect();
    std::fs::write(&p, lines).unwrap();
    p
}

/// `kiln dhis2 reports` for the Immunization group against the stub.
fn reports(server: &Server, locations: &Path, out: &Path) -> Command {
    let mut c = kiln();
    c.args([
        "dhis2",
        "reports",
        "--dhis2",
        &base(server),
        "--instance",
        "sl-demo",
        "--indicator-group",
        "srxUNt2w5jn",
    ])
    .arg("--locations")
    .arg(locations)
    .arg("--out")
    .arg(out)
    .env("KILN_DHIS2_TOKEN", "d2pat_test");
    c
}

/// An analytics response with dates for every period in `rows`.
fn analytics_for(rows: Vec<Value>) -> Value {
    let mut items = serde_json::Map::new();
    for r in &rows {
        let pe = r[1].as_str().unwrap();
        let (y, m) = (&pe[..4], &pe[4..6]);
        items.insert(
            pe.to_string(),
            json!({"startDate": format!("{y}-{m}-01T00:00:00.000"), "endDate": format!("{y}-{m}-28T00:00:00.000")}),
        );
    }
    json!({
        "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
        "metaData": {"items": items},
        "rows": rows
    })
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[test]
fn periods_of_one_kind_share_a_request_within_the_row_budget() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 3);
    // 2 indicators × 3 org units = 6 rows a period, so 2 periods fit in 12.
    for pe in ["20250101;20250102", "20250103", "2025W1;2025W2", "202501"] {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/analytics.json"),
                request::query(url_decoded(contains((
                    "dimension",
                    leak(format!("pe:{pe}"))
                )))),
                request::query(url_decoded(contains((
                    "dimension",
                    "dx:FbKK4ofIv5R;tUIlpyeeX9N"
                )))),
            ])
            .times(1)
            .respond_with(ok(analytics_for(vec![]))),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let a = reports(&server, &locations, &dir.path().join("r.ndjson"))
        .args(["--level", "2", "--max-rows", "12"])
        .args([
            "--period", "20250101", "--period", "2025W1", "--period", "20250102",
        ])
        .args([
            "--period", "20250103", "--period", "2025W2", "--period", "202501",
        ])
        .assert()
        .success();
    assert!(
        stdout(&a).contains("from 4 analytics requests"),
        "{}",
        stdout(&a)
    );
    assert!(
        stderr(&a).contains("[1/4] level 2 · 20250101…20250102 (2 periods) · 2 items: 0 values"),
        "{}",
        stderr(&a)
    );
}

#[test]
fn a_too_large_result_is_split_by_period_before_indicators() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 1);
    let dx = ("dimension", "dx:FbKK4ofIv5R;tUIlpyeeX9N");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(dx))),
            request::query(url_decoded(contains(("dimension", "pe:202501;202502")))),
        ])
        .times(1)
        .respond_with(status_code(409).body(
            r#"{"httpStatusCode":409,"errorCode":"E7128","message":"Query result set exceeded max limit"}"#,
        )),
    );
    for pe in ["202501", "202502"] {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/analytics.json"),
                request::query(url_decoded(contains(dx))),
                request::query(url_decoded(contains((
                    "dimension",
                    leak(format!("pe:{pe}"))
                )))),
            ])
            .times(1)
            .respond_with(ok(analytics_for(vec![
                json!(["tUIlpyeeX9N", pe, "O6uvpzGd5pu", "50.0", "5.0", "10.0"]),
                json!(["FbKK4ofIv5R", pe, "O6uvpzGd5pu", "40.0", "4.0", "10.0"]),
            ]))),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let out = dir.path().join("r.ndjson");
    let a = reports(&server, &locations, &out)
        .args(["--level", "2", "--period", "202501", "--period", "202502"])
        .assert()
        .success();
    let err = stderr(&a);
    assert!(err.contains("splitting 2 periods in two"), "{err}");
    assert!(!err.contains("splitting 2 indicators"), "{err}");
    assert!(stdout(&a).contains("from 3 analytics requests"));
    let ids: Vec<String> = read_ndjson(&out)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        [
            "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-202501",
            "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-202502",
            "sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-202501",
            "sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-202502",
        ]
    );
}

/// A stub with 2 org units at levels 2 and 3 and one answer per level,
/// indicator and month. The earliest are the slowest, so with several
/// workers later units finish first.
fn stub_for_concurrency() -> Server {
    stub_units(true, 0, None)
}

/// The same twelve batches, in planned order: none before `from` may be
/// asked for, and the one at `fail_at` is refused (those after it may or
/// may not be asked for before the run stops).
fn stub_units(slow: bool, from: usize, fail_at: Option<usize>) -> Server {
    let server = Server::run();
    expect_units_batches(&server, slow, from, fail_at);
    server
}

fn expect_units_batches(server: &Server, slow: bool, from: usize, fail_at: Option<usize>) {
    expect_info(server);
    expect_group(server);
    let org_units: [(usize, [&str; 2]); 2] = [
        (2, ["O6uvpzGd5pu", "fdc6uOvgoji"]),
        (3, ["YuQRtpLP10I", "vWbkYPRmKyS"]),
    ];
    for (level, _) in org_units {
        expect_units(server, level as u32, 2);
    }
    let mut delay = if slow { 12u64 } else { 0 };
    let mut idx = 0usize;
    for (level, ous) in org_units {
        for dx in ["FbKK4ofIv5R", "tUIlpyeeX9N"] {
            for (m, pe) in ["202501", "202502", "202503"].into_iter().enumerate() {
                let rows = ous
                    .iter()
                    .enumerate()
                    .map(|(i, ou)| {
                        json!([
                            dx,
                            pe,
                            ou,
                            format!("{}.5", level * 10 + m + i),
                            "1.0",
                            "2.0"
                        ])
                    })
                    .collect();
                let matcher = all_of![
                    request::method_path("GET", "/dhis/api/analytics.json"),
                    request::query(url_decoded(contains((
                        "dimension",
                        leak(format!("dx:{dx}"))
                    )))),
                    request::query(url_decoded(contains((
                        "dimension",
                        leak(format!("pe:{pe}"))
                    )))),
                    request::query(url_decoded(contains((
                        "dimension",
                        leak(format!("ou:LEVEL-{level}"))
                    )))),
                ];
                let answer = delay_and_then(
                    std::time::Duration::from_millis(delay * 15),
                    ok(analytics_for(rows)),
                );
                match fail_at {
                    _ if idx < from => {}
                    Some(f) if idx == f => {
                        server.expect(Expectation::matching(matcher).times(1).respond_with(
                            status_code(409).body(
                                r#"{"errorCode":"E7124","message":"Dimension is not valid"}"#,
                            ),
                        ))
                    }
                    Some(f) if idx > f => server.expect(
                        Expectation::matching(matcher)
                            .times(0..=1)
                            .respond_with(answer),
                    ),
                    _ => {
                        server.expect(Expectation::matching(matcher).times(1).respond_with(answer))
                    }
                }
                idx += 1;
                delay = delay.saturating_sub(1);
            }
        }
    }
}

#[test]
fn output_is_the_same_at_any_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(
        dir.path(),
        &["O6uvpzGd5pu", "fdc6uOvgoji", "YuQRtpLP10I", "vWbkYPRmKyS"],
    );
    let run = |concurrency: &str| {
        let server = stub_for_concurrency();
        let out = dir.path().join(format!("r{concurrency}.ndjson"));
        reports(&server, &locations, &out)
            .args([
                "--level",
                "2",
                "--level",
                "3",
                "--chunk",
                "1",
                "--max-rows",
                "1",
            ])
            .args([
                "--period", "202501", "--period", "202502", "--period", "202503",
            ])
            .args(["--concurrency", concurrency])
            .assert()
            .success();
        std::fs::read_to_string(out).unwrap()
    };
    let one = run("1");
    let four = run("4");
    assert_eq!(one, four);
    let r: Vec<Value> = one
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(r.len(), 24);
    // Level, then indicator, then period; by id within each.
    assert_eq!(r[0]["id"], "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-202501");
    assert_eq!(r[1]["id"], "sl-demo-FbKK4ofIv5R-fdc6uOvgoji-202501");
    assert_eq!(r[2]["id"], "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-202502");
    assert_eq!(r[23]["id"], "sl-demo-tUIlpyeeX9N-vWbkYPRmKyS-202503");
    assert!(!dir.path().join("r4.ndjson.tmp").exists());
    assert!(!dir.path().join("r4.ndjson.resume").exists());
}

#[test]
fn breakdowns_attach_to_their_totals_across_batched_periods() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    let ous = ["O6uvpzGd5pu", "fdc6uOvgoji"];
    let pes = ["202501", "202502"];
    let mut totals = Vec::new();
    let mut strata = Vec::new();
    for (i, ou) in ous.iter().enumerate() {
        for (j, pe) in pes.iter().enumerate() {
            let n = 10 * (i + 1) + j;
            totals.push(json!([
                "tUIlpyeeX9N",
                pe,
                ou,
                "50.0",
                format!("{}.0", 2 * n),
                "100.0"
            ]));
            strata.push(json!([
                "tUIlpyeeX9N",
                "qkPbeWaFsnU",
                pe,
                ou,
                "50.0",
                format!("{n}.0"),
                "50.0"
            ]));
            strata.push(json!([
                "tUIlpyeeX9N",
                "VXJy4z1XYPl",
                pe,
                ou,
                "50.0",
                format!("{n}.0"),
                "50.0"
            ]));
        }
    }
    // A breakdown with no total to go with it.
    strata.push(json!([
        "FbKK4ofIv5R",
        "qkPbeWaFsnU",
        "202501",
        "O6uvpzGd5pu",
        "1.0",
        "1.0",
        "1.0"
    ]));
    let both = ("dimension", "pe:202501;202502");
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(both))),
            request::query(url_decoded(not(contains(("dimension", "fMZEcRHuamy"))))),
        ])
        .times(1)
        .respond_with(ok(analytics_for(totals))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(both))),
            request::query(url_decoded(contains(("dimension", "fMZEcRHuamy")))),
        ])
        .times(1)
        .respond_with(ok(json!({
            "headers": [{"name":"dx"},{"name":"fMZEcRHuamy"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {"fMZEcRHuamy": {"name": "Location Fixed/Outreach"},
                                   "qkPbeWaFsnU": {"name": "Fixed"}, "VXJy4z1XYPl": {"name": "Outreach"}}},
            "rows": strata
        }))),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &ous);
    let out = dir.path().join("r.ndjson");
    let report = dir.path().join("report.json");
    let a = reports(&server, &locations, &out)
        .args(["--level", "2", "--period", "202501", "--period", "202502"])
        .args(["--breakdown", "fMZEcRHuamy"])
        .arg("--report")
        .arg(&report)
        .assert()
        .success();
    assert!(
        stdout(&a).contains("4 breakdowns attached"),
        "{}",
        stdout(&a)
    );
    let r = read_ndjson(&out);
    assert_eq!(r.len(), 4);
    for (k, rep) in r.iter().enumerate() {
        let (i, j) = (k / 2, k % 2);
        let n = 10 * (i + 1) + j;
        assert_eq!(
            rep["id"],
            format!("sl-demo-tUIlpyeeX9N-{}-{}", ous[i], pes[j])
        );
        let g = &rep["group"][0];
        assert_eq!(g["population"][0]["count"], 2 * n);
        let s = &g["stratifier"][0]["stratum"];
        assert_eq!(s.as_array().unwrap().len(), 2);
        assert_eq!(s[0]["value"]["text"], "Fixed");
        assert_eq!(s[1]["value"]["text"], "Outreach");
        assert_eq!(s[0]["population"][0]["count"], n);
    }
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(rep["counts"]["stratum_without_total"], 1);
}

#[test]
fn a_failed_request_stops_the_run_and_keeps_what_was_written() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 1);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "pe:2025")))),
        ])
        // Both indicator chunks may be in flight when the first fails.
        .times(1..=2)
        .respond_with(status_code(409).body(
            r#"{"httpStatusCode":409,"errorCode":"E7124","message":"Dimension is not valid"}"#,
        )),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(not(contains(("dimension", "pe:2025"))))),
        ])
        .times(0..)
        .respond_with(ok(analytics_for(vec![]))),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let out = dir.path().join("r.ndjson");
    let a = reports(&server, &locations, &out)
        .args(["--level", "2", "--chunk", "1", "--concurrency", "4"])
        .args([
            "--period", "202501", "--period", "2025", "--period", "202502",
        ])
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("DHIS2 rejected the analytics query for level 2 and period 2025"),
        "{}",
        stderr(&a)
    );
    assert!(
        stderr(&a).contains("run again with --resume"),
        "{}",
        stderr(&a)
    );
    assert!(!out.exists());
    assert!(dir.path().join("r.ndjson.tmp").exists());
    assert!(dir.path().join("r.ndjson.resume").exists());
}

/// Analytics asked for by date range: `startDate` and `endDate`, no period.
fn by_range(
    start: &'static str,
    end: &'static str,
) -> impl httptest::matchers::Matcher<httptest::http::Request<httptest::bytes::Bytes>> {
    all_of![
        request::method_path("GET", "/dhis/api/analytics.json"),
        request::query(url_decoded(contains(("startDate", start)))),
        request::query(url_decoded(contains(("endDate", end)))),
        request::query(url_decoded(not(contains(("dimension", matches("^pe:")))))),
    ]
}

#[test]
fn a_range_is_one_value_per_org_unit_with_breakdowns_and_the_campaign() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    let ous = ["O6uvpzGd5pu", "fdc6uOvgoji"];
    // Without a period dimension DHIS2 sends no pe column and no period items.
    server.expect(
        Expectation::matching(all_of![
            by_range("2026-04-05", "2026-05-20"),
            request::query(url_decoded(not(contains(("dimension", "fMZEcRHuamy"))))),
        ])
        .times(1)
        .respond_with(ok(json!({
            "headers": [{"name":"dx"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {}},
            "rows": [["tUIlpyeeX9N", ous[0], "50.0", "5.0", "10.0"],
                     ["tUIlpyeeX9N", ous[1], "25.0", "5.0", "20.0"]]
        }))),
    );
    server.expect(
        Expectation::matching(all_of![
            by_range("2026-04-05", "2026-05-20"),
            request::query(url_decoded(contains(("dimension", "fMZEcRHuamy")))),
        ])
        .times(1)
        .respond_with(ok(json!({
            "headers": [{"name":"dx"},{"name":"fMZEcRHuamy"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
            "metaData": {"items": {"fMZEcRHuamy": {"name": "Location Fixed/Outreach"}, "qkPbeWaFsnU": {"name": "Fixed"}}},
            "rows": [["tUIlpyeeX9N", "qkPbeWaFsnU", ous[0], "40.0", "4.0", "10.0"],
                     ["tUIlpyeeX9N", "qkPbeWaFsnU", ous[1], "20.0", "4.0", "20.0"]]
        }))),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &ous);
    let out = dir.path().join("r.ndjson");
    let a = reports(&server, &locations, &out)
        .args(["--level", "2", "--range", "2026-04-05:2026-05-20"])
        .args(["--breakdown", "fMZEcRHuamy"])
        .args(["--campaign", "CarePlan/bgd-mr-sia-2026"])
        .assert()
        .success();
    let s = stdout(&a);
    assert!(s.contains("2 breakdowns attached"), "{s}");
    assert!(s.contains("1 periods"), "{s}");
    assert!(
        stderr(&a).contains("[1/1] level 2 · 2026-04-05/2026-05-20 · 2 items: 4 values"),
        "{}",
        stderr(&a)
    );
    let r = read_ndjson(&out);
    assert_eq!(r.len(), 2);
    for (rep, ou) in r.iter().zip(ous) {
        assert_eq!(
            rep["id"],
            format!("sl-demo-tUIlpyeeX9N-{ou}-20260405-20260520")
        );
        assert_eq!(rep["period"]["start"], "2026-04-05");
        assert_eq!(rep["period"]["end"], "2026-05-20");
        assert_eq!(
            rep["extension"],
            json!([
                {"url": "https://icr.healthcampaigns.org/StructureDefinition/dhis2-period",
                 "valueString": "2026-04-05/2026-05-20"},
                {"url": "https://icr.healthcampaigns.org/StructureDefinition/campaign",
                 "valueReference": {"reference": "CarePlan/bgd-mr-sia-2026"}}
            ])
        );
        let st = &rep["group"][0]["stratifier"][0]["stratum"][0];
        assert_eq!(st["value"]["text"], "Fixed");
        assert_eq!(st["population"][0]["count"], 4);
    }
    assert_eq!(r[1]["group"][0]["measureScore"]["value"], 0.25);
}

#[test]
fn periods_and_ranges_together_each_get_the_campaign() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 1);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/analytics.json"),
            request::query(url_decoded(contains(("dimension", "pe:202601")))),
            request::query(url_decoded(not(contains(("startDate", any()))))),
        ])
        .times(1)
        .respond_with(ok(analytics_for(vec![json!([
            "tUIlpyeeX9N",
            "202601",
            "O6uvpzGd5pu",
            "50.0",
            "5.0",
            "10.0"
        ])]))),
    );
    server.expect(
        Expectation::matching(by_range("2026-01-01", "2026-03-31"))
            .times(1)
            .respond_with(ok(json!({
                "headers": [{"name":"dx"},{"name":"ou"},{"name":"value"},{"name":"numerator"},{"name":"denominator"}],
                "rows": [["FbKK4ofIv5R", "O6uvpzGd5pu", "30.0", "3.0", "10.0"]]
            }))),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let out = dir.path().join("r.ndjson");
    reports(&server, &locations, &out)
        .args(["--level", "2", "--period", "202601"])
        .args(["--range", "2026-01-01:2026-03-31"])
        .args(["--campaign", "CarePlan/bgd-mr-sia-2026"])
        .assert()
        .success();
    let r = read_ndjson(&out);
    let ids: Vec<&str> = r.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-202601",
            "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-20260101-20260331"
        ]
    );
    for rep in &r {
        assert_eq!(
            rep["extension"][1]["valueReference"]["reference"],
            "CarePlan/bgd-mr-sia-2026"
        );
    }
    assert_eq!(r[0]["extension"][0]["valueString"], "202601");
}

#[test]
fn bad_ranges_and_campaigns_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let cases: [(&[&str], &str); 5] = [
        (&["--range", "2026-05-20:2026-04-05"], "START is after END"),
        (
            &["--range", "2026-02-30:2026-03-05"],
            "\"2026-02-30\" is not a date",
        ),
        (&["--range", "2026-04-05"], "expected START:END"),
        (
            &["--period", "2025", "--campaign", "bgd-mr-sia-2026"],
            "expected a reference ResourceType/id",
        ),
        (
            &["--period", "2025", "--campaign", "CarePlan/bgd_mr"],
            "expected a reference ResourceType/id",
        ),
    ];
    for (args, want) in cases {
        let a = kiln()
            .args([
                "dhis2",
                "reports",
                "--dhis2",
                "https://dhis2.invalid",
                "--instance",
                "sl-demo",
            ])
            .args(["--indicator", "tUIlpyeeX9N", "--level", "2"])
            .args(args)
            .arg("--locations")
            .arg(&locations)
            .arg("--out")
            .arg(dir.path().join("r.ndjson"))
            .assert()
            .code(2);
        assert!(stderr(&a).contains(want), "{args:?}: {}", stderr(&a));
    }
    // Neither a period nor a range.
    let a = kiln()
        .args([
            "dhis2",
            "reports",
            "--dhis2",
            "https://dhis2.invalid",
            "--instance",
            "sl-demo",
        ])
        .args(["--indicator", "tUIlpyeeX9N", "--level", "2"])
        .arg("--locations")
        .arg(&locations)
        .arg("--out")
        .arg(dir.path().join("r.ndjson"))
        .assert()
        .code(2);
    assert!(stderr(&a).contains("--period"), "{}", stderr(&a));
}

#[test]
fn a_range_whose_ids_would_be_too_long_is_refused_before_any_analytics() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/dataSets.json")).respond_with(
            ok(json!({"dataSets": [{"id": "QX4ZTUbOt3a", "name": "Reproductive Health"}]})),
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let locations = dir.path().join("locations.ndjson");
    std::fs::write(
        &locations,
        format!(
            "{}\n",
            json!({"resourceType": "Location", "id": "x",
                   "identifier": [{"system": "https://icr.healthcampaigns.org/identifiers/dhis2/bangladesh-epi26/orgunit", "value": "O6uvpzGd5pu"}]})
        ),
    )
    .unwrap();
    let a = kiln()
        .args([
            "dhis2",
            "reports",
            "--dhis2",
            &base(&server),
            "--instance",
            "bangladesh-epi26",
        ])
        .args([
            "--data-set",
            "QX4ZTUbOt3a",
            "--level",
            "2",
            "--range",
            "2026-04-05:2026-05-20",
        ])
        .arg("--locations")
        .arg(&locations)
        .arg("--out")
        .arg(dir.path().join("r.ndjson"))
        .env("KILN_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("over FHIR's 64; use a shorter --instance"),
        "{}",
        stderr(&a)
    );
}

/// An analytics response that also lists the org units and periods it
/// covered, as DHIS2 does in `metaData.dimensions`.
fn analytics_covering(rows: Vec<Value>, ous: &[&str], pes: &[&str]) -> Value {
    let mut v = analytics_for(rows);
    v["metaData"]["dimensions"] = json!({"ou": ous, "pe": pes, "dx": [], "co": []});
    v
}

#[test]
fn a_rerun_writes_only_new_and_changed_reports_and_lists_removed_ids() {
    let (a, b) = ("O6uvpzGd5pu", "fdc6uOvgoji");
    let run = |built: &str, rows: Vec<Value>, out: &Path, extra: &[&str]| {
        let server = Server::run();
        expect_info_built(&server, built);
        expect_group(&server);
        expect_units(&server, 2, 2);
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/analytics.json"),
                request::query(url_decoded(contains(("dimension", "pe:202501")))),
            ])
            .times(1)
            .respond_with(ok(analytics_covering(rows, &[a, b], &["202501"]))),
        );
        let dir = out.parent().unwrap();
        let locations = locations_for(dir, &[a, b, "YuQRtpLP10I"]);
        let assert = reports(&server, &locations, out)
            .args(["--level", "2", "--period", "202501"])
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
    let dir = tempfile::tempdir().unwrap();
    let reports_file = dir.path().join("reports.ndjson");
    run(
        "2026-10-12T02:07:00.052",
        vec![
            row("tUIlpyeeX9N", a, 5.0),
            row("tUIlpyeeX9N", b, 3.0),
            row("FbKK4ofIv5R", a, 1.0),
        ],
        &reports_file,
        &[],
    );
    // Reports the next run does not ask for: another period, an org unit at
    // another level, another instance's Measure. None of them is removed.
    let first = read_ndjson(&reports_file);
    let mut other_period = first[2].clone();
    other_period["id"] = json!("sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-202412");
    other_period["extension"][0]["valueString"] = json!("202412");
    let mut other_level = first[2].clone();
    other_level["id"] = json!("sl-demo-tUIlpyeeX9N-YuQRtpLP10I-202501");
    other_level["subject"]["reference"] = json!("Location/sle-YuQRtpLP10I");
    let mut other_instance = first[2].clone();
    other_instance["id"] = json!("other-tUIlpyeeX9N-O6uvpzGd5pu-202501");
    other_instance["measure"] = json!("https://icr.healthcampaigns.org/Measure/other-tUIlpyeeX9N");
    let mut text = std::fs::read_to_string(&reports_file).unwrap();
    for r in [&other_period, &other_level, &other_instance] {
        text.push_str(&format!("{r}\n"));
    }
    std::fs::write(&reports_file, text).unwrap();

    // A day later, after another analytics build: one value the same, one
    // changed, one new and one gone. The baseline is rolled in place.
    let changed = dir.path().join("changed.ndjson");
    let removed = dir.path().join("removed.txt");
    let out = run(
        "2026-10-13T02:07:00.052",
        vec![
            row("tUIlpyeeX9N", a, 5.0),
            row("tUIlpyeeX9N", b, 4.0),
            row("FbKK4ofIv5R", b, 2.0),
        ],
        &reports_file,
        &[
            "--previous",
            reports_file.to_str().unwrap(),
            "--changed",
            changed.to_str().unwrap(),
            "--removed",
            removed.to_str().unwrap(),
        ],
    );
    assert!(
        out.contains("1 new, 1 changed, 1 unchanged, 1 removed"),
        "{out}"
    );
    assert!(
        out.contains("Wrote 2 new or changed MeasureReports"),
        "{out}"
    );
    let ids = |p: &Path| -> Vec<String> {
        read_ndjson(p)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&changed),
        [
            "sl-demo-FbKK4ofIv5R-fdc6uOvgoji-202501",
            "sl-demo-tUIlpyeeX9N-fdc6uOvgoji-202501"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&removed).unwrap(),
        "sl-demo-FbKK4ofIv5R-O6uvpzGd5pu-202501\n"
    );
    // --out has every report of this run, the baseline for the next.
    let all = read_ndjson(&reports_file);
    assert_eq!(all.len(), 3);
    assert_eq!(all[2]["date"], "2026-10-13T02:07:00.052Z");
}

#[test]
fn an_interrupted_run_resumes_to_the_same_file() {
    resumes_to_the_same_file("r.ndjson");
}

#[test]
fn an_interrupted_gzip_run_resumes_to_the_same_file() {
    let bytes = resumes_to_the_same_file("r.ndjson.gz");
    assert_eq!(gunzip(&bytes).lines().count(), 24);
}

/// The text of gzip bytes (any number of members).
fn gunzip(bytes: &[u8]) -> String {
    use std::io::Read;
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(bytes)
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// Fails a run at the seventh of twelve batches, cuts its temporary file
/// mid-line, then resumes it. The output must be the same bytes as an
/// uninterrupted run's, which are returned.
fn resumes_to_the_same_file(name: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(
        dir.path(),
        &["O6uvpzGd5pu", "fdc6uOvgoji", "YuQRtpLP10I", "vWbkYPRmKyS"],
    );
    let run = |server: &Server, out: &Path, extra: &[&str]| {
        let mut c = reports(server, &locations, out);
        c.args(["--level", "2", "--level", "3", "--chunk", "1"])
            .args([
                "--period", "202501", "--period", "202502", "--period", "202503",
            ])
            .args(extra);
        c.assert()
    };
    let full = dir.path().join(format!("full-{name}"));
    let whole = run(&stub_units(false, 0, None), &full, &["--max-rows", "1"]).success();

    // The seventh batch is refused: six are written and checkpointed. The
    // DHIS2 URL is part of the plan, so every later run uses this server.
    let mut server = Server::run();
    expect_units_batches(&server, false, 0, Some(6));
    let out = dir.path().join(name);
    let (tmp, checkpoint) = (
        dir.path().join(format!("{name}.tmp")),
        dir.path().join(format!("{name}.resume")),
    );
    let failed = run(&server, &out, &["--max-rows", "1", "--concurrency", "1"]).code(2);
    assert!(
        stderr(&failed).contains("6 of 12 batches written"),
        "{}",
        stderr(&failed)
    );
    assert!(!out.exists() && tmp.exists() && checkpoint.exists());
    // A crash halfway through writing the next batch.
    let mut partial = std::fs::read(&tmp).unwrap();
    partial.extend_from_slice(br#"{"resourceType":"MeasureRep"#);
    std::fs::write(&tmp, partial).unwrap();

    // Planned differently: refused, and nothing is lost.
    server.verify_and_clear();
    expect_info(&server);
    expect_group(&server);
    expect_units(&server, 2, 2);
    expect_units(&server, 3, 2);
    let refused = run(&server, &out, &["--max-rows", "2", "--resume"]).code(2);
    assert!(
        stderr(&refused).contains("was planned differently (max_rows changed)"),
        "{}",
        stderr(&refused)
    );
    assert!(tmp.exists() && checkpoint.exists());

    // Resumed: only the last six batches are asked for.
    server.verify_and_clear();
    expect_units_batches(&server, false, 6, None);
    let resumed = run(&server, &out, &["--max-rows", "1", "--resume"]).success();
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
        summary(&whole).contains("Wrote 24 MeasureReports to OUT (2 indicators, 4 Locations, 3 periods; 24 values from 12 analytics requests)"),
        "{}",
        summary(&whole)
    );
    std::fs::read(&out).unwrap()
}

#[test]
fn gzip_locations_previous_and_changed_files() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let plain = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let locations = dir.path().join("locations.ndjson.gz");
    let mut gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&locations).unwrap(),
        flate2::Compression::default(),
    );
    gz.write_all(&std::fs::read(plain).unwrap()).unwrap();
    gz.finish().unwrap();
    let out = dir.path().join("r.ndjson.gz");
    let changed = dir.path().join("c.ndjson.gz");
    let run = |extra: &[&str]| {
        let server = Server::run();
        expect_info(&server);
        expect_group(&server);
        expect_units(&server, 2, 1);
        server.expect(
            Expectation::matching(request::method_path("GET", "/dhis/api/analytics.json"))
                .respond_with(ok(analytics_for(vec![json!([
                    "tUIlpyeeX9N",
                    "202501",
                    "O6uvpzGd5pu",
                    "50.0",
                    "5.0",
                    "10.0"
                ])]))),
        );
        let a = reports(&server, &locations, &out)
            .args(["--level", "2", "--period", "202501"])
            .args(extra)
            .assert()
            .success();
        stdout(&a)
    };
    run(&[]);
    let first = gunzip(&std::fs::read(&out).unwrap());
    assert!(first.contains("sl-demo-tUIlpyeeX9N-O6uvpzGd5pu-202501"));
    let s = run(&[
        "--previous",
        out.to_str().unwrap(),
        "--changed",
        changed.to_str().unwrap(),
    ]);
    assert!(
        s.contains("0 new, 0 changed, 1 unchanged, 0 removed"),
        "{s}"
    );
    // Nothing changed: still a valid, empty gzip file.
    assert_eq!(gunzip(&std::fs::read(&changed).unwrap()), "");
    assert_eq!(gunzip(&std::fs::read(&out).unwrap()), first);
}

#[test]
fn an_old_analytics_build_is_refused_or_warned_about() {
    let dir = tempfile::tempdir().unwrap();
    let locations = locations_for(dir.path(), &["O6uvpzGd5pu"]);
    let out = dir.path().join("r.ndjson");
    let server = Server::run();
    expect_info_built(&server, "2020-01-01T00:00:00.000");
    let a = reports(&server, &locations, &out)
        .args([
            "--level",
            "2",
            "--period",
            "2025",
            "--max-analytics-age",
            "7d",
        ])
        .assert()
        .code(2);
    assert!(
        stderr(&a).contains("last built 2020-01-01T00:00:00.000")
            && stderr(&a).contains("older than --max-analytics-age 7 days"),
        "{}",
        stderr(&a)
    );
    assert!(!out.exists());

    let server = Server::run();
    expect_info_built(&server, "2020-01-01T00:00:00.000");
    expect_group(&server);
    expect_units(&server, 2, 1);
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/analytics.json"))
            .respond_with(ok(analytics_for(vec![]))),
    );
    let a = reports(&server, &locations, &out)
        .args(["--level", "2", "--period", "2025"])
        .assert()
        .success();
    assert!(
        stderr(&a).contains("warning: DHIS2 analytics tables were last built"),
        "{}",
        stderr(&a)
    );
    let bad = reports(&server, &locations, &out)
        .args([
            "--level",
            "2",
            "--period",
            "2025",
            "--max-analytics-age",
            "7",
        ])
        .assert()
        .code(2);
    assert!(
        stderr(&bad).contains("e.g. 36h, 7d, 2w"),
        "{}",
        stderr(&bad)
    );
}
