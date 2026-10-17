//! End-to-end runs of `fhir-pipes refresh`: the database step, which loads the
//! view tables into PostgreSQL.

mod common;

use predicates::str::contains as has;

use common::*;

#[test]
#[cfg(not(feature = "postgres"))]
fn the_database_step_needs_the_postgres_feature() {
    let (_dir, config) = setup_with_raw("\n[database]\n");
    cmd(&config)
        .args(["--only", "views", "--reuse-raw"])
        .assert()
        .failure()
        .stderr(has("--features postgres"));
    cmd(&config)
        .args(["--only", "views", "--reuse-raw", "--skip", "database"])
        .assert()
        .success();
}

/// Runs against the PostgreSQL in $FHIR_PIPES_TEST_DATABASE_URL, each test in
/// a schema of its own; skipped without it.
#[cfg(feature = "postgres")]
mod database {
    use super::*;
    use sqlx_core::connection::Connection;
    use sqlx_core::row::Row;
    use sqlx_core::sql_str::AssertSqlSafe;
    use std::fs;

    const URL: &str = "FHIR_PIPES_TEST_DATABASE_URL";

    fn url() -> Option<String> {
        std::env::var(URL).ok().filter(|u| !u.is_empty())
    }

    /// A schema name no other test uses.
    fn schema(test: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("t_{test}_{}_{nanos}", std::process::id())
    }

    /// Run SQL statements; the rows of the last one, every value as text.
    fn sql(statements: &[&str]) -> Vec<Vec<Option<String>>> {
        let url = url().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut conn = sqlx_postgres::PgConnection::connect(&url).await.unwrap();
                let mut rows = Vec::new();
                for s in statements {
                    rows = sqlx_core::query::query(AssertSqlSafe(s.to_string()))
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

    fn one(statement: &str) -> String {
        sql(&[statement])[0][0].clone().unwrap_or_default()
    }

    #[test]
    fn view_tables_are_loaded_and_replaced() {
        if url().is_none() {
            return;
        }
        let s = schema("load");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("obs → {s}.obs (3 rows)")))
            .stdout(has(format!("patient → {s}.patient (2 rows)")));
        // Analyzed after the commit: the planner's row estimate is the count
        // just loaded (a table only copied into would still say 0).
        assert_eq!(
            one(&format!(
                "SELECT reltuples::bigint::text FROM pg_class WHERE oid = '\"{s}\".obs'::regclass"
            )),
            "3"
        );
        // Only view tables: not locations.
        assert_eq!(
            one(&format!(
                "SELECT string_agg(table_name::text, ',' ORDER BY table_name) FROM information_schema.tables WHERE table_schema = '{s}'"
            )),
            "obs,patient"
        );
        // Types from the Parquet files, values as written.
        assert_eq!(
            one(&format!(
                "SELECT data_type::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'patient' AND column_name = 'born'"
            )),
            "date"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || '=' || born::text || '@' || coalesce(country, 'null'), ',' ORDER BY id) FROM \"{s}\".patient"
            )),
            "p1=2020-05-01@null,p2=2021-01-01@null"
        );

        // A view someone built on a table survives the next run, which
        // replaces the rows.
        sql(&[&format!(
            "CREATE VIEW \"{s}\".patient_ids AS SELECT id FROM \"{s}\".patient"
        )]);
        let raw = dir.path().join("data/raw/test/Patient.ndjson");
        let first = fs::read_to_string(&raw)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        fs::write(&raw, format!("{first}\n")).unwrap();
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success()
            .stdout(has(format!("patient → {s}.patient (1 rows)")));
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',') FROM \"{s}\".patient_ids"
            )),
            "p1"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn a_table_with_other_columns_is_recreated() {
        if url().is_none() {
            return;
        }
        let s = schema("recreate");
        sql(&[
            &format!("CREATE SCHEMA \"{s}\""),
            &format!("CREATE TABLE \"{s}\".obs (stale text)"),
        ]);
        let (_dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\ntables = [\"obs\"]\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.columns WHERE table_schema = '{s}' AND table_name = 'obs' AND column_name = 'stale'"
            )),
            "0"
        );
        // `tables` limited the load to obs.
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM information_schema.tables WHERE table_schema = '{s}' AND table_name = 'patient'"
            )),
            "0"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// A missing or unreadable warehouse fails before the database changes.
    #[test]
    fn a_missing_warehouse_leaves_the_database_alone() {
        if url().is_none() {
            return;
        }
        let s = schema("missing");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        cmd(&config)
            .args(["--only", "database", "--data"])
            .arg(dir.path().join("nowhere"))
            .assert()
            .failure()
            .stderr(has("the database was not changed"));
        assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// An empty view gets its table; tables named t, t_id and t_id_idx do
    /// not clash with the index names PostgreSQL picks.
    #[test]
    fn empty_and_similarly_named_tables_load() {
        if url().is_none() {
            return;
        }
        let s = schema("names");
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        let views = views_with(
            dir.path(),
            &["IcrAlpha", "IcrAlphaId", "IcrAlphaIdIdx"],
            true,
        );
        for _ in 0..2 {
            cmd(&config)
                .args(["--only", "views", "--reuse-raw", "--views"])
                .arg(&views)
                .assert()
                .success()
                .stdout(has(format!("empty → {s}.empty (0 rows)")))
                .stdout(has(format!("alpha_id → {s}.alpha_id (2 rows)")));
        }
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".empty")),
            "0"
        );
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".alpha")),
            "2"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// Two loads at once: the second waits for the first, so the rows are
    /// there once.
    #[test]
    fn overlapping_loads_do_not_duplicate_rows() {
        if url().is_none() {
            return;
        }
        let s = schema("overlap");
        let (_dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .success();
        let run = || {
            let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_fhir-pipes"));
            c.current_dir(config.parent().unwrap())
                .args(["refresh", "--config"])
                .arg(&config)
                .args(["--only", "database"])
                .env_remove("FHIR_PIPES_DATA")
                .env_remove("FHIR_PIPES_VIEWS")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        for _ in 0..3 {
            let mut a = run();
            let mut b = run();
            assert!(a.wait().unwrap().success() && b.wait().unwrap().success());
            assert_eq!(one(&format!("SELECT count(*)::text FROM \"{s}\".obs")), "3");
        }
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    #[test]
    fn database_settings_are_checked() {
        let (_dir, config) = setup_with_raw(
            "\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\ntables = [\"nope\"]\n",
        );
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .assert()
            .failure()
            .stderr(has("`nope` is not a view table (obs, patient)"));
        let (_dir, config) = setup_with_raw("\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\n");
        cmd(&config)
            .args(["--only", "views", "--reuse-raw"])
            .env_remove("FHIR_PIPES_NO_SUCH_URL")
            .assert()
            .failure()
            .stderr(has("$FHIR_PIPES_NO_SUCH_URL"));
    }
}
