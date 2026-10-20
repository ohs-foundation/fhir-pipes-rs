//! `fhir-pipes views run --db`, against the PostgreSQL in
//! $FHIR_PIPES_TEST_DATABASE_URL; skipped without it.

#![cfg(feature = "postgres")]

use assert_cmd::Command;
use sqlx_core::connection::Connection;
use sqlx_core::sql_str::AssertSqlSafe;

fn url() -> Option<String> {
    std::env::var("FHIR_PIPES_TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

fn run_sql(url: &str, statements: &[String]) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = sqlx_postgres::PgConnection::connect(url).await.unwrap();
            for s in statements {
                sqlx_core::query::query(AssertSqlSafe(s.clone()))
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("{s}: {e}"));
            }
        });
}

/// Drops the test's schema when the test ends, failed or not.
struct DropSchema(String, String);

impl Drop for DropSchema {
    fn drop(&mut self) {
        run_sql(
            &self.0,
            &[format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1)],
        );
    }
}

#[test]
fn every_column_type_is_read_back() {
    let Some(url) = url() else { return };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("t_views_db_{}_{nanos}", std::process::id());
    let _drop = DropSchema(url.clone(), schema.clone());
    let observation = r#"{"resourceType":"Observation","id":"o1","status":"final",
        "effectiveDateTime":"2026-01-02","valueQuantity":{"value":2.50,"unit":"kg"},
        "component":[{"valueInteger":3},{"valueBoolean":true}]}"#;
    run_sql(
        &url,
        &[
            format!("CREATE SCHEMA {schema}"),
            format!("CREATE TABLE {schema}.observation (id text, resource jsonb, status text)"),
            format!(
                "INSERT INTO {schema}.observation VALUES ('o1', '{observation}', 'created'), \
                 ('gone', '{{\"resourceType\":\"Observation\",\"id\":\"gone\"}}', 'deleted')"
            ),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let view = dir.path().join("view.json");
    std::fs::write(
        &view,
        r#"{"resourceType":"ViewDefinition","name":"obs","resource":"Observation","status":"active",
          "select":[{"column":[
            {"name":"id","path":"id","type":"id"},
            {"name":"value","path":"value.ofType(Quantity).value","type":"decimal"},
            {"name":"count","path":"component.value.ofType(integer).first()","type":"integer"},
            {"name":"flag","path":"component.value.ofType(boolean).first()","type":"boolean"},
            {"name":"effective","path":"effective.ofType(dateTime)","type":"dateTime"},
            {"name":"units","path":"value.ofType(Quantity).unit","type":"string","collection":true}
          ]}]}"#,
    )
    .unwrap();
    // The view's table is found through the search path.
    let sep = if url.contains('?') { '&' } else { '?' };
    let db = format!("{url}{sep}options=-c%20search_path%3D{schema}");
    let out = Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env_remove("FHIR_PIPES_DATABASE_URL")
        .args(["views", "run", "--output", "ndjson", "--db", &db])
        .arg(&view)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![serde_json::json!({
            "id": "o1", "value": 2.5, "count": 3, "flag": true,
            "effective": "2026-01-02", "units": ["kg"]
        })]
    );
}
