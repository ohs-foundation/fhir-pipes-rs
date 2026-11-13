//! The fake DHIS2 Web API the fhir-pipes-dhis2 tests run against: the
//! responses, the Locations files they read, and readers for what the
//! commands write. Used by tests/dhis2.rs and tests/dhis2_import.rs.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::Assert;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use serde_json::{Value, json};

/// The repository's root, where examples/ is.
pub const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// `fhir-pipes-dhis2` with the example ICR profile and none of the DHIS2
/// environment variables, so `--maps` and the credentials are only what a
/// test passes. No dhis2 command reads the profile.
pub fn fhir_pipes_dhis2() -> Command {
    with_env(Command::new(env!("CARGO_BIN_EXE_fhir-pipes-dhis2")))
}

/// The program fhir-pipes, for `import run` and for running the connector
/// as `fhir-pipes dhis2`. Cargo writes it to the folder fhir-pipes-dhis2 is
/// in when it builds the workspace, as `cargo test --workspace` does.
pub fn fhir_pipes() -> Command {
    with_env(Command::new(fhir_pipes_exe()))
}

/// Where fhir-pipes is: beside fhir-pipes-dhis2.
pub fn fhir_pipes_exe() -> PathBuf {
    let exe = Path::new(env!("CARGO_BIN_EXE_fhir-pipes-dhis2"))
        .with_file_name(format!("fhir-pipes{}", std::env::consts::EXE_SUFFIX));
    assert!(
        exe.is_file(),
        "{} is missing: run `cargo test --workspace`, or `cargo build -p fhir-pipes` first",
        exe.display()
    );
    exe
}

fn with_env(mut c: Command) -> Command {
    // ICR-shaped fixtures: the example ICR profile.
    c.env(
        "FHIR_PIPES_PROFILE",
        format!("{ROOT}/examples/profiles/icr.toml"),
    );
    for var in [
        "FHIR_PIPES_DHIS2_URL",
        "FHIR_PIPES_DHIS2_TOKEN",
        "FHIR_PIPES_DHIS2_USER",
        "FHIR_PIPES_DHIS2_PASSWORD",
        "FHIR_PIPES_DHIS2_MAPS",
    ] {
        c.env_remove(var);
    }
    c
}

pub const ORGUNIT_SYSTEM: &str =
    "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo/orgunit";

pub fn stdout(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

pub fn stderr(a: &Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

pub fn read_ndjson(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

pub fn ok(v: Value) -> impl httptest::responders::Responder {
    status_code(200)
        .insert_header("content-type", "application/json")
        .body(v.to_string())
}

/// `system/info`, signed in with the personal access token.
pub fn expect_info(server: &Server) {
    expect_info_built(server, "2026-10-12T02:07:00.052");
}

/// `system/info` with the analytics tables last built at `built`.
pub fn expect_info_built(server: &Server, built: &str) {
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

pub fn base(server: &Server) -> String {
    server.url("/dhis").to_string()
}

pub fn indicators() -> Value {
    json!({"indicators": [
        {"id":"tUIlpyeeX9N","code":"IN_1","name":"Penta 3 Coverage <1y","annualized":true,
         "indicatorType":{"name":"Per cent","factor":100,"number":false},
         "numerator":"#{a}","denominator":"#{b}","indicatorGroups":[{"id":"srxUNt2w5jn","name":"Immunization"}]},
        {"id":"FbKK4ofIv5R","name":"Measles Coverage <1y","annualized":true,
         "indicatorType":{"name":"Per cent","factor":100,"number":false},
         "numerator":"#{c}","denominator":"#{b}","indicatorGroups":[{"id":"srxUNt2w5jn","name":"Immunization"}]}
    ]})
}

pub fn expect_group(server: &Server) {
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

pub fn analytics(rows: Value) -> Value {
    json!({
        "headers": [{"name":"dx"},{"name":"pe"},{"name":"ou"},{"name":"value"},{"name":"numerator"},
                    {"name":"denominator"},{"name":"factor"},{"name":"multiplier"},{"name":"divisor"}],
        "metaData": {"items": {
            "2025": {"startDate": "2025-01-01T00:00:00.000", "endDate": "2025-12-31T00:00:00.000"}
        }},
        "rows": rows
    })
}

/// The org unit count fhir-pipes sizes analytics requests from.
pub fn expect_units(server: &Server, level: u32, total: u64) {
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

/// A Locations file mapping each org unit to `sle-<uid>`.
pub fn locations_for(dir: &Path, org_units: &[&str]) -> std::path::PathBuf {
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

/// The org units of the Locations file `locations` as the `DHIS2OrgUnit`
/// records `dhis2 extract locations` writes for them, at `out`: what
/// `dhis2 extract reports --org-units` reads in place of `--locations`.
/// Each Location `<prefix>-<uid>` carrying `<uid>` under `ORGUNIT_SYSTEM`
/// gives a record with that `idPrefix` and `uid`, its `name` when it has
/// one, and the `parentUid` its `partOf` names. Only the fields `extract
/// reports` reads are written.
pub fn org_units_from_locations(locations: &Path, out: &Path) -> std::path::PathBuf {
    let list = read_ndjson(locations);
    let uid_of = |l: &Value| -> String {
        l["identifier"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["system"] == ORGUNIT_SYSTEM)
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let by_id: std::collections::HashMap<String, String> = list
        .iter()
        .map(|l| (l["id"].as_str().unwrap().to_string(), uid_of(l)))
        .collect();
    let lines: String = list
        .iter()
        .map(|l| {
            let uid = uid_of(l);
            let id = l["id"].as_str().unwrap();
            let prefix = id.strip_suffix(&format!("-{uid}")).unwrap();
            let mut r = json!({"resourceType": "DHIS2OrgUnit", "instance": "sl-demo",
                               "idPrefix": prefix, "uid": uid});
            if let Some(name) = l.get("name") {
                r["name"] = name.clone();
            }
            if let Some(parent) = l.pointer("/partOf/reference").and_then(Value::as_str) {
                r["parentUid"] = json!(by_id[parent.strip_prefix("Location/").unwrap()]);
            }
            format!("{r}\n")
        })
        .collect();
    std::fs::write(out, lines).unwrap();
    out.to_path_buf()
}

/// An analytics response with dates for every period in `rows`.
pub fn analytics_for(rows: Vec<Value>) -> Value {
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

pub fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Analytics asked for by date range: `startDate` and `endDate`, no period.
pub fn by_range(
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

/// The fixture: a country, two districts, four facilities.
pub const C: &str = "ImspTQPwCqd";

pub const D1: &str = "O6uvpzGd5pu";

pub const D2: &str = "fdc6uOvgoji";

pub const F: [&str; 4] = ["DiszpKrYNg8", "YuQRtpLP10I", "vWbkYPRmKyS", "Rp268JB6Ne4"];

/// Doses (SUM, by Fixed/Outreach), population (SUM, no categories), and
/// coverage = doses ÷ population × 100.
pub const DOSES: &str = "s46m5MS0hxu";

pub const POP: &str = "pikOziyCXbM";

pub const COVERAGE: &str = "Uvn6LCg7dVU";

pub const FIXED: &str = "qkPbeWaFsnU";

pub const OUTREACH: &str = "VXJy4z1XYPl";

pub const COC_FIXED: &str = "cocFixed001";

pub const COC_OUTREACH: &str = "cocOutrch01";

pub const COC_DEFAULT: &str = "HllvX50cXC0";

/// Locations with `partOf`: country ← districts ← facilities.
pub fn hierarchy(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("locations.ndjson");
    let loc = |ou: &str, parent: Option<&str>| {
        let mut l = json!({"resourceType": "Location", "id": format!("sle-{ou}"), "name": ou,
                           "identifier": [{"system": ORGUNIT_SYSTEM, "value": ou}]});
        if let Some(p) = parent {
            l["partOf"] = json!({"reference": format!("Location/sle-{p}")});
        }
        format!("{l}\n")
    };
    let mut text = loc(C, None) + &loc(D1, Some(C)) + &loc(D2, Some(C));
    for (i, f) in F.iter().enumerate() {
        text += &loc(f, Some(if i < 2 { D1 } else { D2 }));
    }
    std::fs::write(&p, text).unwrap();
    p
}

/// The indicator and data elements, as `fetch_indicators` and the
/// data-values metadata read them.
pub fn expect_fixture_metadata(server: &Server, doses_aggregation: &str) {
    expect_fixture_metadata_with(
        server,
        doses_aggregation,
        json!({"id": COVERAGE, "name": "Coverage", "indicatorType": {"factor": 100},
               "numerator": format!("#{{{DOSES}}}"), "denominator": format!("#{{{POP}}}")}),
    );
}

pub fn expect_fixture_metadata_with(server: &Server, doses_aggregation: &str, indicator: Value) {
    server.expect(
        Expectation::matching(request::method_path("GET", "/dhis/api/indicators.json"))
            .respond_with(ok(json!({"indicators": [indicator]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/dataElements.json"),
            request::query(url_decoded(contains((
                "fields",
                matches("dataElementGroups")
            )))),
        ])
        .times(0..)
        .respond_with(ok(json!({"dataElements": [
            {"id": DOSES, "name": "Doses", "aggregationType": doses_aggregation},
            {"id": POP, "name": "Population", "aggregationType": "SUM"}]}))),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/dataElements.json"),
            request::query(url_decoded(contains(("fields", matches("categoryCombo"))))),
        ])
        .times(0..)
        .respond_with(ok(json!({"dataElements": [
            {"id": DOSES, "name": "Doses", "aggregationType": doses_aggregation,
             "categoryCombo": {"categories": [{"id": "fMZEcRHuamy"}], "categoryOptionCombos": [
                 {"id": COC_FIXED, "categoryOptions": [{"id": FIXED}]},
                 {"id": COC_OUTREACH, "categoryOptions": [{"id": OUTREACH}]}]}},
            {"id": POP, "name": "Population", "aggregationType": "SUM",
             "categoryCombo": {"categories": [{"id": "GLevLNI9wkl"}], "categoryOptionCombos": [
                 {"id": COC_DEFAULT, "categoryOptions": [{"id": "xYerKDKCefk"}]}]}}]}))),
    );
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            "/dhis/api/categories/fMZEcRHuamy.json",
        ))
        .times(0..)
        .respond_with(ok(json!({"id": "fMZEcRHuamy", "name": "Location Fixed/Outreach",
            "categoryOptions": [{"id": FIXED, "name": "Fixed"}, {"id": OUTREACH, "name": "Outreach"}]}))),
    );
}

/// Each level's org units, listed in full (data values) or counted (analytics).
pub fn expect_fixture_levels(server: &Server) {
    for (level, ous) in [(1u32, vec![C]), (2, vec![D1, D2]), (3, F.to_vec())] {
        let filter = leak(format!("level:eq:{level}"));
        let list: Vec<Value> = ous.iter().map(|o| json!({"id": o})).collect();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/organisationUnits.json"),
                request::query(url_decoded(contains(("filter", filter)))),
                request::query(url_decoded(contains(("page", "1")))),
            ])
            .times(0..)
            .respond_with(ok(
                json!({"pager": {"page": 1, "pageCount": 1, "total": ous.len()},
                                    "organisationUnits": list}),
            )),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/dhis/api/organisationUnits.json"),
                request::query(url_decoded(contains(("filter", filter)))),
                request::query(url_decoded(contains(("pageSize", "1")))),
            ])
            .times(0..)
            .respond_with(ok(
                json!({"pager": {"page": 1, "pageCount": ous.len(), "total": ous.len()},
                                    "organisationUnits": [{"id": ous[0]}]}),
            )),
        );
    }
}

pub fn dv(de: &str, pe: &str, ou: &str, coc: &str, value: &str) -> Value {
    json!({"dataElement": de, "period": pe, "orgUnit": ou, "categoryOptionCombo": coc,
           "attributeOptionCombo": COC_DEFAULT, "value": value})
}

/// The raw data values behind the fixture.
pub fn fixture_values() -> Vec<Value> {
    let mut v = vec![
        dv(DOSES, "20250105", F[0], COC_FIXED, "3"),
        dv(DOSES, "20250120", F[0], COC_OUTREACH, "2"),
        dv(DOSES, "202501", F[1], COC_FIXED, "4"),
        dv(DOSES, "20250210", F[2], COC_OUTREACH, "6"),
        // Outside the periods asked for: left out.
        dv(DOSES, "20250301", F[0], COC_FIXED, "100"),
    ];
    for pe in ["202501", "202502"] {
        for (i, f) in F.iter().enumerate() {
            v.push(dv(POP, pe, f, COC_DEFAULT, if i < 2 { "10" } else { "20" }));
        }
    }
    v
}

/// A data-values stub: metadata, levels, and `values` for any span asked.
pub fn values_server(doses_aggregation: &str, indicator: Option<Value>) -> Server {
    let server = Server::run();
    expect_info(&server);
    match indicator {
        Some(i) => expect_fixture_metadata_with(&server, doses_aggregation, i),
        None => expect_fixture_metadata(&server, doses_aggregation),
    }
    expect_fixture_levels(&server);
    server
}

pub fn expect_values(server: &Server, start: &'static str, end: &'static str, values: Vec<Value>) {
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/dhis/api/dataValueSets.json"),
            request::query(url_decoded(contains(("startDate", start)))),
            request::query(url_decoded(contains(("endDate", end)))),
            request::query(url_decoded(not(contains(("lastUpdated", any()))))),
        ])
        .times(1)
        .respond_with(ok(json!({"dataValues": values}))),
    );
}

/// The same twelve batches, in planned order: none before `from` may be
/// asked for, and the one at `fail_at` is refused (those after it may or
/// may not be asked for before the run stops).
pub fn stub_units(slow: bool, from: usize, fail_at: Option<usize>) -> Server {
    let server = Server::run();
    expect_units_batches(&server, slow, from, fail_at);
    server
}

pub fn expect_units_batches(server: &Server, slow: bool, from: usize, fail_at: Option<usize>) {
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
