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
    use httptest::{Expectation, all_of, matchers::*, responders::*};
    use serde_json::json;
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

    /// Every row of `table` in `schema`, sorted, as one text.
    fn contents(schema: &str, table: &str) -> String {
        one(&format!(
            "SELECT coalesce(string_agg(t::text, '|' ORDER BY t::text), '') FROM \"{schema}\".\"{table}\" t"
        ))
    }

    fn upsert(schema: &str) -> String {
        format!("\n[database]\nurl_env = \"{URL}\"\nschema = \"{schema}\"\nmode = \"upsert\"\n")
    }

    /// A config beside `config`, for the same warehouse, that loads it with
    /// replace into `schema`.
    fn replace_config(
        config: &std::path::Path,
        upsert_schema: &str,
        schema: &str,
    ) -> std::path::PathBuf {
        let text = fs::read_to_string(config).unwrap().replace(
            &upsert(upsert_schema),
            &format!("\n[database]\nurl_env = \"{URL}\"\nschema = \"{schema}\"\n"),
        );
        let path = config.with_file_name("replace.toml");
        fs::write(&path, text).unwrap();
        path
    }

    fn assert_same(a: &str, b: &str, tables: &[&str]) {
        for t in tables {
            assert_eq!(contents(a, t), contents(b, t), "{t}");
        }
    }

    /// An incremental search with one changed Observation, and a count that
    /// shows one deleted: the full download that follows finds which. Upsert
    /// rewrites those two resources' rows and leaves the database as replace
    /// would.
    #[test]
    fn an_incremental_upsert_equals_a_replace() {
        if url().is_none() {
            return;
        }
        let (s, r) = (schema("upsert"), schema("upsert_replace"));
        let mut server = httptest::Server::run();
        for t in ["Observation", "Patient"] {
            server.expect(
                Expectation::matching(full_search(t)).times(1).respond_with(
                    status_code(200).body(bundle(stamped(t, "2026-10-01T00:00:00Z"))),
                ),
            );
        }
        let (dir, config) = setup(&server.url_str("/fhir"), &upsert(&s));
        cmd(&config)
            .args(["--only", "views"])
            .assert()
            .success()
            .stdout(has("== database"))
            .stdout(has("every table in full: no record of the last load"))
            .stdout(has(format!("obs → {s}.obs (3 rows)")));
        server.verify_and_clear();

        let mut changed = stamped("Observation", "2026-10-02T00:00:00Z").remove(0);
        changed["valueInteger"] = json!(30);
        let since = ("_lastUpdated", "ge2026-10-01T00:00:00Z");
        for (t, found) in [("Observation", vec![changed.clone()]), ("Patient", vec![])] {
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", format!("/fhir/{t}")),
                    request::query(url_decoded(contains(since))),
                ])
                .times(1)
                .respond_with(status_code(200).body(bundle(found))),
            );
        }
        // o3 was deleted on the server.
        for (t, n) in [("Observation", 2), ("Patient", 2)] {
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("GET", format!("/fhir/{t}")),
                    request::query(url_decoded(contains(("_summary", "count")))),
                ])
                .times(1..)
                .respond_with(status_code(200).body(
                    json!({"resourceType": "Bundle", "type": "searchset", "total": n}).to_string(),
                )),
            );
        }
        let mut remaining = stamped("Observation", "2026-10-01T00:00:00Z");
        remaining.truncate(2);
        remaining[0] = changed;
        server.expect(
            Expectation::matching(full_search("Observation"))
                .times(1)
                .respond_with(status_code(200).body(bundle(remaining))),
        );
        cmd(&config)
            .args(["--only", "views"])
            .assert()
            .success()
            .stdout(has(format!(
                "obs → {s}.obs (2 resources changed or deleted: 2 rows deleted, 1 copied)"
            )))
            .stdout(has(format!("patient → {s}.patient (unchanged)")));
        server.verify_and_clear();
        let log: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join("data/raw/test/.changes.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(log["running"], false);
        assert_eq!(log["types"], json!({}));

        let replace = replace_config(&config, &s, &r);
        cmd(&replace)
            .args(["--only", "database"])
            .assert()
            .success();
        assert_same(&s, &r, &["obs", "patient"]);
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || '=' || value::text, ',' ORDER BY id) FROM \"{s}\".obs"
            )),
            "o1=30,o2=5"
        );
        // Replace removes the log, so a later upsert starts in full.
        assert!(!dir.path().join("data/raw/test/.changes.json").exists());
        sql(&[
            &format!("DROP SCHEMA \"{s}\" CASCADE"),
            &format!("DROP SCHEMA \"{r}\" CASCADE"),
        ]);
    }

    /// From files: what differs from the last split is what changed. A
    /// changed view, an unfinished refresh and --only database each load in
    /// full.
    #[test]
    fn upsert_from_files_and_when_it_loads_in_full() {
        if url().is_none() {
            return;
        }
        let (s, r) = (schema("files"), schema("files_replace"));
        let (dir, config) = setup(NOWHERE, &upsert(&s));
        let patients = |born: &[(&str, &str)]| {
            lines(
                &born
                    .iter()
                    .map(|(id, b)| json!({"resourceType": "Patient", "id": id, "birthDate": b}))
                    .collect::<Vec<_>>(),
            )
        };
        let observations = lines(&[obs("o1", 3, None), obs("o2", 5, None)]);
        let write = |p: String| {
            inputs(
                dir.path(),
                &[
                    ("Patient.ndjson", p),
                    ("Observation.ndjson", observations.clone()),
                ],
            )
        };
        let input = write(patients(&[
            ("p1", "2020-01-01"),
            ("p2", "2020-02-02"),
            ("p3", "2020-03-03"),
        ]));
        let run = |config: &std::path::Path| {
            let mut c = cmd(config);
            c.arg("--input").arg(&input).args(["--only", "views"]);
            c
        };
        run(&config).assert().success();
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".patient")),
            "3"
        );

        // p2 changed, p3 is gone, p4 is new.
        write(patients(&[
            ("p1", "2020-01-01"),
            ("p2", "2021-02-02"),
            ("p4", "2020-04-04"),
        ]));
        run(&config)
            .assert()
            .success()
            .stdout(has(format!(
                "patient → {s}.patient (3 resources changed or deleted: 2 rows deleted, 2 copied)"
            )))
            .stdout(has(format!("obs → {s}.obs (unchanged)")));
        let replace = replace_config(&config, &s, &r);
        // Into another schema; the upsert's log is put back after.
        let log = fs::read(dir.path().join("data/raw/test/.changes.json")).unwrap();
        cmd(&replace)
            .args(["--only", "database"])
            .assert()
            .success();
        assert_same(&s, &r, &["obs", "patient"]);
        fs::write(dir.path().join("data/raw/test/.changes.json"), &log).unwrap();

        // A refresh that stopped part way: the next load is in full.
        let mut v: serde_json::Value = serde_json::from_slice(&log).unwrap();
        v["running"] = json!(true);
        fs::write(
            dir.path().join("data/raw/test/.changes.json"),
            v.to_string(),
        )
        .unwrap();
        run(&config)
            .assert()
            .success()
            .stdout(has("every table in full: the last refresh did not finish"))
            .stdout(has(format!("patient → {s}.patient (3 rows)")));

        // The same Patients from a gzip input, p1 changed: only p1 differs
        // from the last split.
        let text = patients(&[
            ("p1", "2019-01-01"),
            ("p2", "2021-02-02"),
            ("p4", "2020-04-04"),
        ]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, text.as_bytes()).unwrap();
        fs::remove_file(input.join("Patient.ndjson")).unwrap();
        fs::write(input.join("Patient.ndjson.gz"), gz.finish().unwrap()).unwrap();
        run(&config).assert().success().stdout(has(format!(
            "patient → {s}.patient (1 resources changed or deleted: 1 rows deleted, 1 copied)"
        )));

        // A changed view loads its table in full, the others as before.
        let views = views_with(dir.path(), &[], false);
        let path = views.join("Binary-IcrPatient.json");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("birthDate\"", "birthDate.toString()\"")).unwrap();
        run(&config)
            .arg("--views")
            .arg(&views)
            .assert()
            .success()
            .stdout(has(format!(
                "patient → {s}.patient (3 rows, in full: its view, its partition lookup or fhir-pipes changed)"
            )))
            .stdout(has(format!("obs → {s}.obs (unchanged)")));

        // Without the views step the Parquet may be older than the log.
        cmd(&config)
            .args(["--only", "database", "--views"])
            .arg(&views)
            .assert()
            .success()
            .stdout(has(
                "every table in full: the views step did not run in this refresh",
            ));
        sql(&[
            &format!("DROP SCHEMA \"{s}\" CASCADE"),
            &format!("DROP SCHEMA \"{r}\" CASCADE"),
        ]);
    }

    /// Upsert finds rows by the getResourceKey() column; a view without one
    /// is refused before anything runs.
    #[test]
    fn upsert_refuses_a_view_without_a_resource_key() {
        let (dir, config) = setup_with_raw(
            "\n[database]\nurl_env = \"FHIR_PIPES_NO_SUCH_URL\"\nmode = \"upsert\"\n",
        );
        let views = views_with(dir.path(), &[], false);
        fs::write(
            views.join("IcrNoKey.json"),
            r#"{"resourceType": "ViewDefinition", "name": "IcrNoKey", "status": "active", "resource": "Patient",
                "select": [{"forEach": "contained", "column": [{"name": "id", "path": "getResourceKey()"}]}]}"#,
        )
        .unwrap();
        cmd(&config)
            .args(["--only", "views", "--reuse-raw", "--views"])
            .arg(&views)
            .assert()
            .code(2)
            .stderr(has("view IcrNoKey (table no_key) has none"));
        // Left out of `tables`, it is not refused.
        let text = fs::read_to_string(&config).unwrap();
        fs::write(&config, text + "tables = [\"patient\"]\n").unwrap();
        cmd(&config)
            .args(["--check", "--views"])
            .arg(&views)
            .assert()
            .success()
            .stdout(has("schema public, upsert"));
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
