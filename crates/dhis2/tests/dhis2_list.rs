//! `fhir-pipes-dhis2 list KIND` against the fake DHIS2 of
//! tests/common/dhis2.rs: every page read, sorted by name, as a table or
//! JSON.

use assert_cmd::prelude::*;
use httptest::{Expectation, Server, all_of, matchers::*};
use serde_json::{Value, json};

#[path = "common/dhis2.rs"]
mod stub;
use stub::*;

fn list(server: &Server, args: &[&str]) -> assert_cmd::assert::Assert {
    fhir_pipes_dhis2()
        .arg("list")
        .args(args)
        .args(["--dhis2", &base(server)])
        .env("FHIR_PIPES_DHIS2_TOKEN", "d2pat_test")
        .assert()
}

/// Two pages of indicator groups, the second asked for after the first.
fn expect_group_pages(server: &Server) {
    let page = |n: &'static str, groups: Value| {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/indicatorGroups.json"),
                request::query(url_decoded(contains(("page", n)))),
                request::query(url_decoded(contains(("fields", "id,name,indicators~size")))),
            ])
            .times(1)
            .respond_with(ok(json!({
                "pager": {"page": n.parse::<u32>().unwrap(), "pageCount": 2, "total": 3},
                "indicatorGroups": groups
            }))),
        );
    };
    page(
        "1",
        json!([{"id": "srxUNt2w5jn", "name": "Immunization", "indicators": 12},
               {"id": "oehv9EO3vP7", "name": "ANC", "indicators": 4}]),
    );
    page(
        "2",
        json!([{"id": "AoTN7VMBLVx", "name": "malaria", "indicators": 3}]),
    );
}

#[test]
fn a_table_of_every_page_sorted_by_name() {
    let server = Server::run();
    expect_info(&server);
    expect_group_pages(&server);
    let a = list(&server, &["indicator-groups"]).success();
    assert_eq!(
        stdout(&a),
        "UID          INDICATORS  NAME\n\
         oehv9EO3vP7          4  ANC\n\
         srxUNt2w5jn         12  Immunization\n\
         AoTN7VMBLVx          3  malaria\n\
         \n\
         3 indicator groups; use with --indicator-group or [indicators] indicator_groups\n"
    );
}

#[test]
fn json_and_the_filter() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/indicators.json"),
            request::query(url_decoded(contains(("filter", "name:ilike:cov")))),
            request::query(url_decoded(contains((
                "filter",
                "indicatorGroups.id:eq:srxUNt2w5jn"
            )))),
        ])
        .times(1)
        .respond_with(ok(json!({"pager": {"page": 1, "pageCount": 1},
            "indicators": [{"id": "tUIlpyeeX9N", "name": "Penta 3 Coverage <1y"},
                           {"id": "FbKK4ofIv5R", "name": "Measles Coverage <1y"}]}))),
    );
    let a = list(
        &server,
        &[
            "indicators",
            "--group",
            "srxUNt2w5jn",
            "--filter",
            "cov",
            "--format",
            "json",
        ],
    )
    .success();
    let v: Value = serde_json::from_str(&stdout(&a)).unwrap();
    assert_eq!(
        v,
        json!([{"id": "FbKK4ofIv5R", "name": "Measles Coverage <1y"},
               {"id": "tUIlpyeeX9N", "name": "Penta 3 Coverage <1y"}])
    );
}

#[test]
fn org_unit_levels_with_their_counts() {
    let server = Server::run();
    expect_info(&server);
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/organisationUnitLevels.json",
        ))
        .times(2)
        .respond_with(ok(json!({"organisationUnitLevels": [
            {"level": 2, "name": "District"}, {"level": 1, "name": "National"}]}))),
    );
    expect_units(&server, 1, 1);
    expect_units(&server, 2, 13);
    let a = list(&server, &["org-unit-levels"]).success();
    let s = stdout(&a);
    assert!(
        s.starts_with(
            "LEVEL  ORG UNITS  NAME\n1              1  National\n2             13  District\n"
        ),
        "{s}"
    );
    assert!(s.contains("use with --level or [reports] levels"), "{s}");
    let a = list(
        &server,
        &["org-unit-levels", "--format", "json", "--filter", "dist"],
    )
    .success();
    let v: Value = serde_json::from_str(&stdout(&a)).unwrap();
    assert_eq!(v, json!([{"level": 2, "name": "District", "count": 13}]));
}

/// The server comes from the settings file; no instance name is needed;
/// a bad option exits 2 before any request.
#[test]
fn the_server_from_a_settings_file_and_usage_errors() {
    let server = Server::run();
    expect_info(&server);
    expect_group_pages(&server);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sl.toml");
    std::fs::write(&file, format!("url = \"{}\"\n", base(&server))).unwrap();
    fhir_pipes_dhis2()
        .args(["list", "indicator-groups", "--config"])
        .arg(&file)
        .env("FHIR_PIPES_DHIS2_TOKEN", "d2pat_test")
        .assert()
        .success();
    let a = fhir_pipes_dhis2()
        .args([
            "list",
            "data-sets",
            "--group",
            "srxUNt2w5jn",
            "--dhis2",
            "https://dhis2.invalid",
        ])
        .assert()
        .code(2);
    assert!(stderr(&a).contains("--group is for"), "{}", stderr(&a));
    let a = fhir_pipes_dhis2()
        .args(["list", "data-sets"])
        .assert()
        .code(2);
    assert!(stderr(&a).contains("no DHIS2 URL"), "{}", stderr(&a));
}
