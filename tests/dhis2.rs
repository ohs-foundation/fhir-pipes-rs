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
            "lastAnalyticsTableSuccess": "2026-10-12T02:07:00.052"
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

#[test]
fn reports_split_a_too_large_request_and_skip_unmapped_org_units() {
    let server = Server::run();
    expect_info(&server);
    expect_group(&server);
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
