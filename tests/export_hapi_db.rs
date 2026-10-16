//! `fhir-pipes export --hapi-db` against a minimal copy of HAPI's schema
//! (only the tables and columns read), in a schema of its own in the
//! PostgreSQL at $FHIR_PIPES_TEST_DATABASE_URL. Skipped without it.
#![cfg(feature = "postgres")]

use std::io::Write;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use serde_json::{Value, json};
use sqlx_core::connection::Connection;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;

const URL: &str = "FHIR_PIPES_TEST_DATABASE_URL";

fn url() -> Option<String> {
    std::env::var(URL).ok().filter(|u| !u.is_empty())
}

/// A name no other test uses.
fn unique(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("t_{test}_{}_{nanos}", std::process::id())
}

/// Run SQL statements on one connection; the rows of the last one, every
/// value as text.
fn sql(url: &str, statements: &[String]) -> Vec<Vec<Option<String>>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = sqlx_postgres::PgConnection::connect(url).await.unwrap();
            let mut rows = Vec::new();
            for s in statements {
                rows = sqlx_core::query::query(AssertSqlSafe(s.clone()))
                    .fetch_all(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("{s}: {e}"));
            }
            rows.iter()
                .map(|r| {
                    (0..r.len())
                        .map(|i| r.try_get::<Option<String>, _>(i).unwrap())
                        .collect()
                })
                .collect()
        })
}

/// `url` with `schema` first on the search path (and, given, another user).
fn url_for(url: &str, schema: &str, user: Option<(&str, &str)>) -> String {
    let mut u = url::Url::parse(url).unwrap();
    if let Some((name, password)) = user {
        u.set_username(name).unwrap();
        u.set_password(Some(password)).unwrap();
    }
    u.query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    u.to_string()
}

fn gzip(text: &str) -> String {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(text.as_bytes()).unwrap();
    let bytes = gz.finish().unwrap();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn export(db_url: &str, out: &Path, extra: &[&str]) -> std::process::Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env("FHIR_PIPES_HAPI_DATABASE_URL", db_url)
        .args(["export", "--hapi-db", "--type", "Patient", "--out"])
        .arg(out)
        .args(extra)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn by_id(path: &Path) -> std::collections::BTreeMap<String, Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (v["id"].as_str().unwrap().to_string(), v)
        })
        .collect()
}

/// HAPI 8's tables, the columns read.
fn hapi8_schema(s: &str) -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {s}"),
        format!(
            "CREATE TABLE {s}.hfj_resource (res_id bigint PRIMARY KEY, res_type varchar(40) NOT NULL, \
             fhir_id varchar(64), res_ver bigint NOT NULL, res_updated timestamp(6) NOT NULL, \
             res_deleted_at timestamp(6))"
        ),
        format!(
            "CREATE TABLE {s}.hfj_res_ver (pid bigserial PRIMARY KEY, res_id bigint NOT NULL, \
             res_ver bigint NOT NULL, res_encoding varchar(5) NOT NULL, res_text oid, \
             res_text_vc text, source_uri varchar(768), request_id varchar(16))"
        ),
        format!(
            "CREATE TABLE {s}.hfj_tag_def (tag_id bigint PRIMARY KEY, tag_type integer NOT NULL, \
             tag_system varchar(200), tag_code varchar(200), tag_display varchar(200), \
             tag_version varchar(30), tag_user_selected boolean)"
        ),
        format!(
            "CREATE TABLE {s}.hfj_res_tag (pid bigserial PRIMARY KEY, res_id bigint, tag_id bigint)"
        ),
    ]
}

fn drop_schema(url: &str, s: &str) {
    sql(
        url,
        &[
            format!("SELECT lo_unlink(res_text) FROM {s}.hfj_res_ver WHERE res_text IS NOT NULL"),
            format!("DROP SCHEMA {s} CASCADE"),
        ],
    );
}

#[test]
fn reads_current_versions_then_changes_and_deletions() {
    let Some(url) = url() else { return };
    let s = unique("hapi8");
    let mut setup = hapi8_schema(&s);
    let patient = |body: &str| format!(r#"{{"resourceType":"Patient",{body}}}"#);
    setup.extend([
        // abc: a client-assigned id, two versions, a tag and a profile.
        format!(
            "INSERT INTO {s}.hfj_resource VALUES \
             (1, 'Patient', 'abc', 2, '2026-01-01 00:00:01.5', NULL), \
             (2, 'Patient', '2', 1, '2026-01-01 00:00:02', NULL), \
             (3, 'Patient', 'gone', 2, '2026-01-01 00:00:03', '2026-01-01 00:00:03'), \
             (4, 'Observation', 'o1', 1, '2026-01-01 00:00:04', NULL)"
        ),
        format!(
            "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text, res_text_vc, source_uri, request_id) VALUES \
             (1, 1, 'JSON', NULL, '{}', NULL, NULL), \
             (1, 2, 'JSON', NULL, '{}', NULL, NULL), \
             (2, 1, 'JSONC', lo_from_bytea(0, decode('{}', 'hex')), NULL, 'http://src', 'r1'), \
             (3, 1, 'JSON', NULL, '{}', NULL, NULL), \
             (3, 2, 'DEL', NULL, NULL, NULL, NULL), \
             (4, 1, 'JSON', NULL, '{{\"resourceType\":\"Observation\",\"status\":\"final\"}}', NULL, NULL)",
            patient(r#""gender":"male""#),
            patient(r#""name":[{"family":"Abc"}],"gender":"female""#),
            gzip(&patient(r#""active":true"#)),
            patient(r#""active":false"#),
        ),
        format!(
            "INSERT INTO {s}.hfj_tag_def VALUES \
             (1, 0, 'http://example.org/tags', 't1', 'Tag one', NULL, NULL), \
             (2, 1, 'https://github.com/hapifhir/hapi-fhir/ns/jpa/profile', 'http://example.org/P', NULL, NULL, NULL), \
             (3, 2, 'http://terminology.hl7.org/CodeSystem/v3-Confidentiality', 'R', NULL, NULL, NULL)"
        ),
        format!("INSERT INTO {s}.hfj_res_tag (res_id, tag_id) VALUES (1, 1), (1, 2), (2, 3)"),
    ]);
    sql(&url, &setup);
    let db = url_for(&url, &s, None);
    let dir = tempfile::tempdir().unwrap();

    let got = export(&db, dir.path(), &[]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    assert!(
        text(&got.stdout).contains("Patient: 2 resources from the database"),
        "{}",
        text(&got.stdout)
    );
    let file = dir.path().join("Patient.ndjson");
    let lines = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        lines.lines().next().unwrap(),
        json!({
            "resourceType": "Patient",
            "id": "abc",
            "meta": {
                "versionId": "2",
                "lastUpdated": "2026-01-01T00:00:01.500+00:00",
                "profile": ["http://example.org/P"],
                "tag": [{"system": "http://example.org/tags", "code": "t1", "display": "Tag one"}]
            },
            "name": [{"family": "Abc"}],
            "gender": "female"
        })
        .to_string()
    );
    let all = by_id(&file);
    assert_eq!(all.keys().collect::<Vec<_>>(), ["2", "abc"]);
    assert_eq!(
        all["2"],
        json!({
            "resourceType": "Patient",
            "id": "2",
            "meta": {
                "versionId": "1",
                "lastUpdated": "2026-01-01T00:00:02.000+00:00",
                "source": "http://src#r1",
                "security": [{"system": "http://terminology.hl7.org/CodeSystem/v3-Confidentiality", "code": "R"}]
            },
            "active": true
        })
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".export-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["hapi_db"]["Patient"]["watermark"],
        "2026-01-01T00:00:02.000000"
    );
    // The database, without the user or password.
    let server = state["server"].as_str().unwrap();
    assert!(
        server.starts_with("hapi-db:") && !server.contains('@'),
        "{server}"
    );

    // A day later: abc updated, 2 deleted, gone created again, new added.
    sql(
        &url,
        &[
            format!(
                "UPDATE {s}.hfj_resource SET res_ver = 3, res_updated = '2026-01-02 00:00:01' WHERE res_id = 1"
            ),
            format!(
                "UPDATE {s}.hfj_resource SET res_ver = 2, res_updated = '2026-01-02 00:00:02', \
                 res_deleted_at = '2026-01-02 00:00:02' WHERE res_id = 2"
            ),
            format!(
                "UPDATE {s}.hfj_resource SET res_ver = 3, res_updated = '2026-01-02 00:00:03', \
                 res_deleted_at = NULL WHERE res_id = 3"
            ),
            format!(
                "INSERT INTO {s}.hfj_resource VALUES (5, 'Patient', 'new', 1, '2026-01-02 00:00:05', NULL)"
            ),
            format!(
                "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text_vc) VALUES \
                 (1, 3, 'JSON', '{}'), (2, 2, 'DEL', NULL), (3, 3, 'JSON', '{}'), (5, 1, 'JSON', '{}')",
                patient(r#""gender":"other""#),
                patient(r#""active":true"#),
                patient(r#""birthDate":"2000-01-01""#),
            ),
        ],
    );
    let got = export(&db, dir.path(), &["--incremental"]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    let out = text(&got.stdout);
    assert!(
        out.contains("Patient: 2 new, 1 updated or read again, 1 deleted since 2026-01-01T00:00:02.000000, 3 in all"),
        "{out}"
    );
    let all = by_id(&file);
    assert_eq!(all.keys().collect::<Vec<_>>(), ["abc", "gone", "new"]);
    assert_eq!(all["abc"]["gender"], "other");
    assert_eq!(all["abc"]["meta"]["versionId"], "3");
    // Its tags are the resource's, not the version's.
    assert_eq!(all["abc"]["meta"]["tag"][0]["code"], "t1");
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".export-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["hapi_db"]["Patient"]["watermark"],
        "2026-01-02T00:00:05.000000"
    );
    assert_eq!(state["hapi_db"]["Patient"]["count"], 3);

    // An expunged resource is not a row any more: the count differs, and
    // the type is read in full.
    sql(
        &url,
        &[
            format!("DELETE FROM {s}.hfj_res_tag WHERE res_id = 1"),
            format!("DELETE FROM {s}.hfj_res_ver WHERE res_id = 1"),
            format!("DELETE FROM {s}.hfj_resource WHERE res_id = 1"),
        ],
    );
    let got = export(&db, dir.path(), &["--incremental"]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    let out = text(&got.stdout);
    assert!(
        out.contains("the database has 2 resources and the merged file 3"),
        "{out}"
    );
    assert!(
        out.contains("Patient: 2 resources from the database"),
        "{out}"
    );
    assert_eq!(by_id(&file).keys().collect::<Vec<_>>(), ["gone", "new"]);

    drop_schema(&url, &s);
}

#[test]
fn the_layout_before_hapi_7_takes_client_ids_from_hfj_forced_id() {
    let Some(url) = url() else { return };
    let s = unique("forced");
    let plain: String =
        r#"{"resourceType":"Patient","active":true}"#.bytes().map(|b| format!("{b:02x}")).collect();
    sql(
        &url,
        &[
            format!("CREATE SCHEMA {s}"),
            format!(
                "CREATE TABLE {s}.hfj_resource (res_id bigint PRIMARY KEY, res_type varchar(40) NOT NULL, \
                 res_ver bigint NOT NULL, res_updated timestamp NOT NULL, res_deleted_at timestamp)"
            ),
            format!(
                "CREATE TABLE {s}.hfj_res_ver (pid bigserial PRIMARY KEY, res_id bigint NOT NULL, \
                 res_ver bigint NOT NULL, res_encoding varchar(5) NOT NULL, res_text bytea)"
            ),
            format!(
                "CREATE TABLE {s}.hfj_forced_id (pid bigserial PRIMARY KEY, forced_id varchar(100) NOT NULL, \
                 resource_pid bigint NOT NULL, resource_type varchar(100))"
            ),
            format!(
                "INSERT INTO {s}.hfj_resource VALUES (10, 'Patient', 1, '2024-05-01 10:00:00', NULL), \
                 (11, 'Patient', 1, '2024-05-01 10:00:01', NULL)"
            ),
            format!(
                "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text) VALUES \
                 (10, 1, 'JSON', decode('{plain}', 'hex')), \
                 (11, 1, 'JSONC', decode('{}', 'hex'))",
                gzip(r#"{"resourceType":"Patient","gender":"male"}"#)
            ),
            format!(
                "INSERT INTO {s}.hfj_forced_id (forced_id, resource_pid, resource_type) VALUES ('client-a', 10, 'Patient')"
            ),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let got = export(&url_for(&url, &s, None), dir.path(), &[]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    assert!(text(&got.stdout).contains("ids from hfj_forced_id"));
    let all = by_id(&dir.path().join("Patient.ndjson"));
    assert_eq!(all.keys().collect::<Vec<_>>(), ["11", "client-a"]);
    assert_eq!(all["client-a"]["active"], true);
    assert_eq!(all["11"]["gender"], "male");
    // No tag tables: meta is the row's only.
    assert_eq!(
        all["11"]["meta"],
        json!({"versionId": "1", "lastUpdated": "2024-05-01T10:00:01.000+00:00"})
    );
    sql(&url, &[format!("DROP SCHEMA {s} CASCADE")]);
}

#[test]
fn a_database_without_hapis_schema_is_refused() {
    let Some(url) = url() else { return };
    let s = unique("other");
    sql(
        &url,
        &[
            format!("CREATE SCHEMA {s}"),
            format!("CREATE TABLE {s}.hfj_resource (res_id bigint, res_type text)"),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let got = export(&url_for(&url, &s, None), dir.path(), &[]);
    assert_eq!(got.status.code(), Some(1));
    let err = text(&got.stderr);
    assert!(
        err.contains("does not have the HAPI FHIR JPA schema fhir-pipes reads"),
        "{err}"
    );
    assert!(err.contains("hfj_res_ver.res_id"), "{err}");
    assert!(err.contains("found tables: hfj_resource"), "{err}");
    assert!(!dir.path().join("Patient.ndjson").exists());
    sql(&url, &[format!("DROP SCHEMA {s} CASCADE")]);
}

#[test]
fn a_read_only_role_reads_large_objects_it_may_select() {
    let Some(url) = url() else { return };
    let s = unique("role");
    let role = s.clone();
    let mut setup = hapi8_schema(&s);
    setup.extend([
        format!(
            "INSERT INTO {s}.hfj_resource VALUES (1, 'Patient', 'p1', 1, '2026-01-01 00:00:01', NULL)"
        ),
        format!(
            "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text) VALUES \
             (1, 1, 'JSONC', lo_from_bytea(0, decode('{}', 'hex')))",
            gzip(r#"{"resourceType":"Patient","active":true}"#)
        ),
        format!("CREATE ROLE {role} LOGIN PASSWORD 'pw'"),
        format!("GRANT USAGE ON SCHEMA {s} TO {role}"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA {s} TO {role}"),
        format!("ALTER ROLE {role} SET default_transaction_read_only = on"),
    ]);
    sql(&url, &setup);
    let db = url_for(&url, &s, Some((&role, "pw")));
    let dir = tempfile::tempdir().unwrap();

    // Without SELECT on the large object (or on pg_largeobject): refused,
    // with what to grant.
    let got = export(&db, dir.path(), &[]);
    assert_eq!(got.status.code(), Some(1));
    let err = text(&got.stderr);
    assert!(err.contains("SELECT on pg_catalog.pg_largeobject"), "{err}");

    // With SELECT on the large object, lo_get reads it.
    let oid = sql(
        &url,
        &[format!("SELECT res_text::text FROM {s}.hfj_res_ver")],
    )[0][0]
        .clone()
        .unwrap();
    sql(
        &url,
        &[format!("GRANT SELECT ON LARGE OBJECT {oid} TO {role}")],
    );
    let got = export(&db, dir.path(), &[]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    assert_eq!(
        by_id(&dir.path().join("Patient.ndjson"))["p1"]["active"],
        true
    );

    drop_schema(&url, &s);
    sql(
        &url,
        &[format!("DROP OWNED BY {role}"), format!("DROP ROLE {role}")],
    );
}

#[test]
fn refresh_reads_the_views_resources_from_the_hapi_database() {
    let Some(url) = url() else { return };
    let s = unique("refresh");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh");
    let mut setup = hapi8_schema(&s);
    let mut res_id = 0;
    for t in ["Observation", "Patient"] {
        let lines = std::fs::read_to_string(fixtures.join(format!("raw/{t}.ndjson"))).unwrap();
        for line in lines.lines().filter(|l| !l.trim().is_empty()) {
            let mut r: serde_json::Map<String, Value> = serde_json::from_str(line).unwrap();
            let id = r.remove("id").unwrap();
            res_id += 1;
            setup.push(format!(
                "INSERT INTO {s}.hfj_resource VALUES ({res_id}, '{t}', '{}', 1, '2026-01-01 00:00:00', NULL)",
                id.as_str().unwrap()
            ));
            setup.push(format!(
                "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text_vc) VALUES ({res_id}, 1, 'JSON', '{}')",
                Value::Object(r).to_string().replace('\'', "''")
            ));
        }
    }
    sql(&url, &setup);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let loc = root.join("data/parquet/locations/country=SLE");
    std::fs::create_dir_all(&loc).unwrap();
    std::fs::copy(
        fixtures.join("parquet/locations/country=SLE/part-0.parquet"),
        loc.join("part-0.parquet"),
    )
    .unwrap();
    let config = root.join("fhir-pipes.toml");
    std::fs::write(
        &config,
        format!(
            r#"data = "data"
views = '{views}'
profile = '{profile}'

[server]
base = "http://127.0.0.1:9/fhir"
name = "test"
fetch = "hapi-db"

[tables]
strip_prefix = ["icr_"]

[[partition.lookup]]
key = "location_id"
table = "locations"
table_key = "id"
"#,
            views = fixtures.join("views").display(),
            profile = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/profiles/icr.toml")
                .display()
        ),
    )
    .unwrap();
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.current_dir(root);
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_VIEWS",
        "FHIR_PIPES_SERVER",
        "FHIR_PIPES_SERVER_NAME",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_PROFILE",
    ] {
        c.env_remove(var);
    }
    let got = c
        .env("FHIR_PIPES_HAPI_DATABASE_URL", url_for(&url, &s, None))
        .args(["refresh", "--only", "views", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let out = text(&got.stdout);
    assert!(got.status.success(), "{out}\n{}", text(&got.stderr));
    assert!(
        out.contains("export Observation, Patient (HAPI database)"),
        "{out}"
    );
    assert!(
        out.contains("IcrObs (Observation) → parquet/obs (3 rows)"),
        "{out}"
    );
    assert!(
        out.contains("IcrPatient (Patient) → parquet/patient (2 rows)"),
        "{out}"
    );
    sql(&url, &[format!("DROP SCHEMA {s} CASCADE")]);
}

/// Two HAPI schemas in one database, each with one Patient `p` (the second
/// schema's older): switching the search path, by the connection string or
/// by the role's default, must read the other schema in full rather than
/// apply the first schema's watermark to it.
#[test]
fn another_schema_in_the_same_database_is_read_in_full() {
    let Some(url) = url() else { return };
    let a = unique("schema_a");
    let b = unique("schema_b");
    let role = unique("schema_role");
    let mut setup = Vec::new();
    for (s, updated, gender) in [
        (&a, "2026-03-01 00:00:00", "female"),
        (&b, "2026-02-01 00:00:00", "male"),
    ] {
        setup.extend(hapi8_schema(s));
        setup.push(format!(
            "INSERT INTO {s}.hfj_resource VALUES (1, 'Patient', 'p', 1, '{updated}', NULL)"
        ));
        setup.push(format!(
            "INSERT INTO {s}.hfj_res_ver (res_id, res_ver, res_encoding, res_text_vc) VALUES \
             (1, 1, 'JSON', '{{\"resourceType\":\"Patient\",\"gender\":\"{gender}\"}}')"
        ));
    }
    setup.extend([
        format!("CREATE ROLE {role} LOGIN PASSWORD 'pw'"),
        format!("GRANT USAGE ON SCHEMA {a}, {b} TO {role}"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA {a} TO {role}"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA {b} TO {role}"),
        format!("ALTER ROLE {role} SET search_path = {a}"),
    ]);
    sql(&url, &setup);
    let gender = |dir: &Path| by_id(&dir.join("Patient.ndjson"))["p"]["gender"].clone();
    let server = |dir: &Path| {
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".export-state.json")).unwrap())
                .unwrap();
        state["server"].as_str().unwrap().to_string()
    };

    // By the connection string's options.
    let dir = tempfile::tempdir().unwrap();
    let got = export(&url_for(&url, &a, None), dir.path(), &[]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    assert_eq!(gender(dir.path()), "female");
    assert!(server(dir.path()).ends_with(&format!("/{a}")));
    let got = export(&url_for(&url, &b, None), dir.path(), &["--incremental"]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    let out = text(&got.stdout);
    assert!(
        out.contains("Patient: 1 resources from the database"),
        "{out}"
    );
    assert_eq!(gender(dir.path()), "male");
    assert!(server(dir.path()).ends_with(&format!("/{b}")));

    // By the role's default search path, with nothing in the URL.
    let mut role_url = url::Url::parse(&url).unwrap();
    role_url.set_username(&role).unwrap();
    role_url.set_password(Some("pw")).unwrap();
    let role_url = role_url.to_string();
    let dir = tempfile::tempdir().unwrap();
    let got = export(&role_url, dir.path(), &[]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    assert_eq!(gender(dir.path()), "female");
    sql(&url, &[format!("ALTER ROLE {role} SET search_path = {b}")]);
    let got = export(&role_url, dir.path(), &["--incremental"]);
    assert!(got.status.success(), "{}", text(&got.stderr));
    let out = text(&got.stdout);
    assert!(
        out.contains("Patient: 1 resources from the database"),
        "{out}"
    );
    assert_eq!(gender(dir.path()), "male");

    sql(
        &url,
        &[
            format!("DROP SCHEMA {a} CASCADE"),
            format!("DROP SCHEMA {b} CASCADE"),
            format!("DROP OWNED BY {role}"),
            format!("DROP ROLE {role}"),
        ],
    );
}
