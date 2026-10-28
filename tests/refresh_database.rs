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
    use predicates::prelude::PredicateBooleanExt;
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

    /// Statements run when dropped, failures ignored: the cleanup of a test
    /// that may fail part way.
    struct Cleanup(Vec<String>);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let Some(url) = url() else { return };
            let statements = std::mem::take(&mut self.0);
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    if let Ok(mut conn) = sqlx_postgres::PgConnection::connect(&url).await {
                        for s in statements {
                            let _ = sqlx_core::query::query(AssertSqlSafe(s))
                                .execute(&mut conn)
                                .await;
                        }
                    }
                });
            }
        }
    }

    fn one(statement: &str) -> String {
        sql(&[statement])[0][0].clone().unwrap_or_default()
    }

    /// A stop that comes after the database's commit waits for the publish,
    /// so the files match the database.
    #[test]
    #[cfg(unix)]
    fn sigterm_after_the_database_commit_finishes_the_publish() {
        if url().is_none() {
            return;
        }
        let s = schema("stop");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS {s} CASCADE")]);
        let (dir, config) = setup_with_raw(&format!(
            "\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n"
        ));
        let data = dir.path().join("data");
        let mut child = spawn_refresh(
            &config,
            &["--only", "views", "--reuse-raw"],
            &[
                ("FHIR_PIPES_TEST_PAUSE", "database-committed"),
                (URL, &url().unwrap()),
            ],
        );
        wait_for(&data.join(".paused"));
        sigterm(&child);
        assert_eq!(exit_within(&mut child, 20), Some(1));
        let r = last_run(&data);
        assert_eq!(r["committed"], true, "{r}");
        assert_eq!(r["published"], true, "{r}");
        assert!(
            data.join("parquet/obs/country=SLE/part-0.parquet")
                .is_file()
        );
        assert_eq!(one(&format!("SELECT count(*)::text FROM {s}.obs")), "3");
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

        // A refresh that stopped after the load, in a hook: the database
        // and the log agree, so the next load is not in full.
        let text = fs::read_to_string(&config).unwrap();
        fs::write(
            &config,
            format!("{text}\n[[hooks]]\nname = \"fails\"\nrun = [\"sh\", \"-c\", \"exit 3\"]\n"),
        )
        .unwrap();
        cmd(&config)
            .arg("--input")
            .arg(&input)
            .args(["--only", "views,hooks"])
            .assert()
            .failure()
            .stderr(has("hook fails"));
        fs::write(&config, text).unwrap();
        run(&config)
            .assert()
            .success()
            .stdout(has(format!("patient → {s}.patient (unchanged)")));

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

    /// A views directory under `dir` with one Patient view, `IcrPeople`,
    /// whose columns are `id` (getResourceKey()) and `columns`.
    fn people_view(dir: &std::path::Path, columns: &str) -> std::path::PathBuf {
        let views = dir.join("people-views");
        fs::create_dir_all(&views).unwrap();
        fs::write(
            views.join("IcrPeople.json"),
            format!(
                r#"{{"resourceType": "ViewDefinition", "name": "IcrPeople", "status": "active", "resource": "Patient",
                    "select": [{{"column": [{{"name": "id", "path": "getResourceKey()"}}, {columns}]}}]}}"#
            ),
        )
        .unwrap();
        views
    }

    /// Run `config` on the files in `dir/input` with `views`.
    fn from_files(config: &std::path::Path, views: &std::path::Path) -> assert_cmd::Command {
        let mut c = cmd(config);
        c.arg("--input")
            .arg(config.with_file_name("input"))
            .args(["--only", "views", "--views"])
            .arg(views);
        c
    }

    fn write_patients(dir: &std::path::Path, patients: &[serde_json::Value]) {
        inputs(dir, &[("Patient.ndjson", lines(patients))]);
    }

    /// A resource without an id gives rows without a key, which no id names:
    /// they are rewritten whenever there are any, so a change to one is
    /// loaded although the change log lists no id.
    #[test]
    fn rows_without_an_id_are_rewritten() {
        if url().is_none() {
            return;
        }
        let s = schema("no_id");
        let (dir, config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            dir.path(),
            r#"{"name": "active", "path": "active", "type": "boolean"}"#,
        );
        let patients = |active: bool| {
            vec![
                json!({"resourceType": "Patient", "id": "p1", "active": true}),
                json!({"resourceType": "Patient", "active": active}),
            ]
        };
        write_patients(dir.path(), &patients(true));
        from_files(&config, &views).assert().success();
        write_patients(dir.path(), &patients(false));
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "people → {s}.people (rows without an id: 1 deleted, 1 copied)"
            )));
        assert_eq!(
            one(&format!(
                "SELECT string_agg(coalesce(id, 'null') || '=' || active::text, ',' ORDER BY id) FROM \"{s}\".people"
            )),
            "p1=true,null=false"
        );
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// Two warehouses loading the same schema, or a replace load into it:
    /// each table carries the mark of the load that wrote it last, and a
    /// table with another load's mark is loaded in full.
    #[test]
    fn a_table_another_load_wrote_is_loaded_in_full() {
        if url().is_none() {
            return;
        }
        let s = schema("other_load");
        let (one_dir, one_config) = setup(NOWHERE, &upsert(&s));
        let (other_dir, other_config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            one_dir.path(),
            r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
        );
        let patient =
            |id: &str| json!({"resourceType": "Patient", "id": id, "birthDate": "2020-01-01"});
        write_patients(one_dir.path(), &[patient("p1")]);
        write_patients(other_dir.path(), &[patient("q1"), patient("q2")]);
        from_files(&one_config, &views).assert().success();
        from_files(&other_config, &views).assert().success();
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',' ORDER BY id) FROM \"{s}\".people"
            )),
            "q1,q2"
        );
        from_files(&one_config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "people → {s}.people (1 rows, in full: the database's copy was not last loaded from this warehouse)"
            )));
        assert_eq!(one(&format!("SELECT id FROM \"{s}\".people")), "p1");
        // A replace load removes the mark.
        let replace = other_config.with_file_name("replace.toml");
        fs::write(
            &replace,
            fs::read_to_string(&other_config)
                .unwrap()
                .replace("mode = \"upsert\"\n", ""),
        )
        .unwrap();
        from_files(&replace, &views).assert().success();
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM \"{s}\".fhir_pipes_loads WHERE table_name = 'people'"
            )),
            "0"
        );
        from_files(&one_config, &views)
            .assert()
            .success()
            .stdout(has(
                "in full: the database's copy was not last loaded from this warehouse",
            ));
        assert_eq!(one(&format!("SELECT id FROM \"{s}\".people")), "p1");
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// A table dropped or altered since the last load is made again from the
    /// Parquet and loaded in full, though the change log lists no change.
    #[test]
    fn a_dropped_or_altered_table_is_loaded_in_full() {
        if url().is_none() {
            return;
        }
        let s = schema("recreated");
        let (dir, config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            dir.path(),
            r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
        );
        write_patients(
            dir.path(),
            &[json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-01-01"})],
        );
        from_files(&config, &views).assert().success();
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!("people → {s}.people (unchanged)")));
        sql(&[&format!("DROP TABLE \"{s}\".people")]);
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "people → {s}.people (1 rows, in full: a new table)"
            )));
        sql(&[&format!("ALTER TABLE \"{s}\".people ADD COLUMN extra text")]);
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "people → {s}.people (1 rows, in full: its columns changed)"
            )));
        assert_eq!(one(&format!("SELECT id FROM \"{s}\".people")), "p1");
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!("people → {s}.people (unchanged)")));
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// A view that calls now() can give other rows from the same resources:
    /// its table is loaded in full every run, however the call is written.
    #[test]
    fn a_view_of_the_time_is_loaded_in_full() {
        if url().is_none() {
            return;
        }
        for path in [
            "now().toString()",
            "now /* a comment */ ().toString()",
            "`now`().toString()",
        ] {
            let s = schema("now");
            let (dir, config) = setup(NOWHERE, &upsert(&s));
            let views = people_view(
                dir.path(),
                &format!(
                    r#"{{"name": "at", "path": {}, "type": "string"}}"#,
                    json!(path)
                ),
            );
            write_patients(
                dir.path(),
                &[json!({"resourceType": "Patient", "id": "p1"})],
            );
            from_files(&config, &views).assert().success();
            let first = one(&format!("SELECT at FROM \"{s}\".people"));
            std::thread::sleep(std::time::Duration::from_millis(1100));
            from_files(&config, &views)
                .assert()
                .success()
                .stdout(has(format!(
                    "people → {s}.people (1 rows, in full: its view calls now(), today() or timeOfDay())"
                )));
            assert_ne!(
                one(&format!("SELECT at FROM \"{s}\".people")),
                first,
                "{path}"
            );
            sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
        }
    }

    /// A replace load that empties a table for want of files removes the
    /// table's mark too, so the upsert that wrote it last reloads it.
    #[test]
    fn a_table_emptied_by_another_load_is_loaded_in_full() {
        if url().is_none() {
            return;
        }
        let s = schema("emptied");
        let (one_dir, one_config) = setup(NOWHERE, &upsert(&s));
        let (other_dir, other_config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            one_dir.path(),
            r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
        );
        write_patients(
            one_dir.path(),
            &[json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-01-01"})],
        );
        from_files(&one_config, &views).assert().success();
        // The other warehouse has the table only as an empty directory.
        fs::create_dir_all(other_dir.path().join("data/parquet/people")).unwrap();
        let replace = other_config.with_file_name("replace.toml");
        fs::write(
            &replace,
            fs::read_to_string(&other_config)
                .unwrap()
                .replace("mode = \"upsert\"\n", ""),
        )
        .unwrap();
        cmd(&replace)
            .args(["--only", "database", "--views"])
            .arg(&views)
            .assert()
            .success()
            .stdout(has(format!("people → {s}.people (0 rows)")));
        from_files(&one_config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "people → {s}.people (1 rows, in full: the database's copy was not last loaded from this warehouse)"
            )));
        sql(&[&format!("DROP SCHEMA \"{s}\" CASCADE")]);
    }

    /// A relation named fhir_pipes_loads that upsert did not create (a table
    /// of other columns, an updatable view, a table of the same columns
    /// without upsert's comment) is never written to: replace leaves it
    /// alone with a note, and upsert refuses to load with exit 2.
    #[test]
    fn a_relation_named_like_the_records_table_is_left_alone() {
        if url().is_none() {
            return;
        }
        let cases = [
            (
                "a table",
                "CREATE TABLE {s}.fhir_pipes_loads (table_name text PRIMARY KEY, mark text, payload text); \
                 INSERT INTO {s}.fhir_pipes_loads VALUES ('people', 'x', 'keep')",
                "SELECT string_agg(table_name || mark || payload, ',') FROM {s}.fhir_pipes_loads",
                "peoplexkeep",
            ),
            (
                "a view",
                "CREATE TABLE {s}.business (table_name text PRIMARY KEY, mark text NOT NULL); \
                 INSERT INTO {s}.business VALUES ('people', 'x'); \
                 CREATE VIEW {s}.fhir_pipes_loads AS SELECT table_name, mark FROM {s}.business",
                "SELECT string_agg(table_name || mark, ',') FROM {s}.business",
                "peoplex",
            ),
            (
                "a table",
                "CREATE TABLE {s}.fhir_pipes_loads (table_name text PRIMARY KEY, mark text NOT NULL); \
                 INSERT INTO {s}.fhir_pipes_loads VALUES ('people', 'x')",
                "SELECT string_agg(table_name || mark, ',') FROM {s}.fhir_pipes_loads",
                "peoplex",
            ),
        ];
        for (what, create, check, rows) in cases {
            let s = schema("collision");
            let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
            let quoted = format!("\"{s}\"");
            let mut statements = vec![format!("CREATE SCHEMA {quoted}")];
            statements.extend(create.split("; ").map(|c| c.replace("{s}", &quoted)));
            sql(&statements.iter().map(String::as_str).collect::<Vec<_>>());
            let check = check.replace("{s}", &quoted);
            let (dir, config) = setup(NOWHERE, &upsert(&s));
            let views = people_view(
                dir.path(),
                r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
            );
            write_patients(
                dir.path(),
                &[json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-01-01"})],
            );
            let replace = config.with_file_name("replace.toml");
            fs::write(
                &replace,
                fs::read_to_string(&config)
                    .unwrap()
                    .replace("mode = \"upsert\"\n", ""),
            )
            .unwrap();
            from_files(&replace, &views)
                .assert()
                .success()
                .stdout(has(format!(
                    "note: {s}.fhir_pipes_loads is {what}, not fhir-pipes' load records; left alone"
                )));
            assert_eq!(one(&check), rows, "{create}");
            from_files(&config, &views)
                .assert()
                .code(2)
                .stderr(has(format!(
                    "{s}.fhir_pipes_loads is {what}, not the table where [database] mode = \"upsert\" records its loads"
                )));
            assert_eq!(one(&check), rows, "{create}");
        }
    }

    /// The records table is checked under a lock that a rename waits for:
    /// a replace paused in its COPY (a trigger waits on an advisory lock the
    /// test holds) keeps the table it checked, and another relation put in
    /// its place meanwhile is never written to.
    #[test]
    fn the_records_table_cannot_be_swapped_during_a_load() {
        let Some(url) = url() else {
            return;
        };
        let s = schema("swap");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let (dir, config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            dir.path(),
            r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
        );
        write_patients(
            dir.path(),
            &[json!({"resourceType": "Patient", "id": "p1", "birthDate": "2020-01-01"})],
        );
        from_files(&config, &views).assert().success();
        let key = 0x5157_0000_i64 + i64::from(std::process::id() % 0xffff);
        sql(&[
            &format!(
                "CREATE FUNCTION \"{s}\".wait() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN PERFORM pg_advisory_lock({key}); PERFORM pg_advisory_unlock({key}); \
                 RETURN NULL; END $$"
            ),
            &format!(
                "CREATE TRIGGER wait BEFORE INSERT ON \"{s}\".people \
                 FOR EACH STATEMENT EXECUTE FUNCTION \"{s}\".wait()"
            ),
        ]);
        let replace = config.with_file_name("replace.toml");
        fs::write(
            &replace,
            fs::read_to_string(&config)
                .unwrap()
                .replace("mode = \"upsert\"\n", ""),
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let run = |conn: &mut sqlx_postgres::PgConnection, q: &str| {
            rt.block_on(sqlx_core::query::query(AssertSqlSafe(q.to_string())).execute(&mut *conn))
        };
        let mut holder = rt
            .block_on(sqlx_postgres::PgConnection::connect(&url))
            .unwrap();
        let mut other = rt
            .block_on(sqlx_postgres::PgConnection::connect(&url))
            .unwrap();
        run(&mut holder, &format!("SELECT pg_advisory_lock({key})")).unwrap();
        let mut load = std::process::Command::new(env!("CARGO_BIN_EXE_fhir-pipes"))
            .current_dir(replace.parent().unwrap())
            .args(["refresh", "--config"])
            .arg(&replace)
            .args(["--only", "database", "--views"])
            .arg(&views)
            .env_remove("FHIR_PIPES_DATA")
            .env_remove("FHIR_PIPES_VIEWS")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Until the load waits in the trigger, past its check of the table.
        let waiting = format!(
            "SELECT count(*)::text FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
             AND objid = {key}"
        );
        let started = std::time::Instant::now();
        while one(&waiting) == "0" {
            assert!(
                started.elapsed().as_secs() < 60,
                "the load never reached the trigger"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        run(&mut other, "SET lock_timeout = '1s'").unwrap();
        let renamed = run(
            &mut other,
            &format!("ALTER TABLE \"{s}\".fhir_pipes_loads RENAME TO fhir_pipes_loads_old"),
        );
        if renamed.is_ok() {
            for q in [
                format!(
                    "CREATE TABLE \"{s}\".fhir_pipes_loads (table_name text PRIMARY KEY, mark text, payload text)"
                ),
                format!("INSERT INTO \"{s}\".fhir_pipes_loads VALUES ('people', 'x', 'keep')"),
            ] {
                run(&mut other, &q).unwrap();
            }
        }
        run(&mut holder, &format!("SELECT pg_advisory_unlock({key})")).unwrap();
        assert!(load.wait().unwrap().success());
        match renamed {
            Ok(_) => assert_eq!(
                one(&format!(
                    "SELECT string_agg(payload, ',') FROM \"{s}\".fhir_pipes_loads"
                )),
                "keep",
                "the load wrote to the relation put in place of the records table"
            ),
            Err(e) => {
                assert!(e.to_string().contains("lock timeout"), "{e}");
                // The load removed its table's mark from the table it checked.
                assert_eq!(
                    one(&format!(
                        "SELECT count(*)::text FROM \"{s}\".fhir_pipes_loads WHERE table_name = 'people'"
                    )),
                    "0"
                );
            }
        }
    }

    /// The records table is made after the view tables, so a view table
    /// may take the name its primary key's index would have had.
    #[test]
    fn a_view_table_may_be_named_like_the_records_index() {
        if url().is_none() {
            return;
        }
        let s = schema("pkey");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let (dir, config) = setup(NOWHERE, &upsert(&s));
        let views = dir.path().join("pkey-views");
        fs::create_dir_all(&views).unwrap();
        fs::write(
            views.join("FhirPipesLoadsPkey.json"),
            r#"{"resourceType": "ViewDefinition", "name": "FhirPipesLoadsPkey", "status": "active",
                "resource": "Patient", "select": [{"column": [{"name": "id", "path": "getResourceKey()"}]}]}"#,
        )
        .unwrap();
        write_patients(
            dir.path(),
            &[json!({"resourceType": "Patient", "id": "p1"})],
        );
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "fhir_pipes_loads_pkey → {s}.fhir_pipes_loads_pkey (1 rows)"
            )));
        from_files(&config, &views)
            .assert()
            .success()
            .stdout(has(format!(
                "fhir_pipes_loads_pkey → {s}.fhir_pipes_loads_pkey (unchanged)"
            )));
    }

    /// A role with table privileges, not the owner, loads in either mode.
    /// Upsert's records table needs privileges of its own, and a replace
    /// that cannot remove its tables' marks stops, naming the grant, rather
    /// than leave a mark that would let upsert keep stale rows.
    #[test]
    fn a_role_that_does_not_own_the_tables_can_load() {
        let Some(owner_url) = url() else {
            return;
        };
        let s = schema("writer");
        let role = format!("fhir_pipes_upsert_writer_{}", std::process::id());
        let mut writer_url = url::Url::parse(&owner_url).unwrap();
        writer_url.set_username(&role).unwrap();
        writer_url.set_password(Some("writer")).unwrap();
        const WRITER: &str = "FHIR_PIPES_UPSERT_WRITER_URL";
        let _cleanup = Cleanup(vec![
            format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"),
            format!("DROP OWNED BY {role}"),
            format!("DROP ROLE {role}"),
        ]);
        // No CREATE on the database: the schema exists by the time the
        // writer loads, and a load needs only table privileges then.
        sql(&[&format!("CREATE ROLE {role} LOGIN PASSWORD 'writer'")]);

        let (owner_dir, owner_config) = setup(NOWHERE, &upsert(&s));
        let views = people_view(
            owner_dir.path(),
            r#"{"name": "born", "path": "birthDate", "type": "date"}"#,
        );
        let patient =
            |id: &str| json!({"resourceType": "Patient", "id": id, "birthDate": "2020-01-01"});
        write_patients(owner_dir.path(), &[patient("p1")]);
        from_files(&owner_config, &views).assert().success();
        sql(&[
            &format!("GRANT USAGE ON SCHEMA \"{s}\" TO {role}"),
            &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON \"{s}\".people TO {role}"),
        ]);

        let (writer_dir, writer_config) = setup(
            NOWHERE,
            &format!("\n[database]\nurl_env = \"{WRITER}\"\nschema = \"{s}\"\n"),
        );
        write_patients(writer_dir.path(), &[patient("q1"), patient("q2")]);
        let writer = |config: &std::path::Path| {
            let mut c = from_files(config, &views);
            c.env(WRITER, writer_url.as_str());
            c
        };
        writer(&writer_config)
            .assert()
            .failure()
            .stderr(has(format!(
                "GRANT SELECT, DELETE ON \"{s}\".\"fhir_pipes_loads\" TO the role in the connection string"
            )));
        assert_eq!(
            one(&format!("SELECT string_agg(id, ',') FROM \"{s}\".people")),
            "p1"
        );
        // SELECT and DELETE are enough for replace, the lock included.
        sql(&[&format!(
            "GRANT SELECT, DELETE ON \"{s}\".fhir_pipes_loads TO {role}"
        )]);
        writer(&writer_config).assert().success();
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',' ORDER BY id) FROM \"{s}\".people"
            )),
            "q1,q2"
        );

        // Upsert as the writer: its mark, then nothing to do.
        sql(&[&format!(
            "GRANT INSERT, UPDATE ON \"{s}\".fhir_pipes_loads TO {role}"
        )]);
        let text = fs::read_to_string(&writer_config).unwrap();
        fs::write(&writer_config, text + "mode = \"upsert\"\n").unwrap();
        writer(&writer_config).assert().success();
        writer(&writer_config)
            .assert()
            .success()
            .stdout(has(format!("people → {s}.people (unchanged)")));
        from_files(&owner_config, &views)
            .assert()
            .success()
            .stdout(has(
                "in full: the database's copy was not last loaded from this warehouse",
            ));
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

    // Libraries: the derived tables (location_hierarchy and the SQLQuery
    // results) and the SQLViews, which become PostgreSQL views.

    const SQLVIEW: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";
    const SQLQUERY: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";
    const HIERARCHY: &str = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy";
    const READING: &str = "https://example.org/ViewDefinition/IcrReading";

    /// With PostgreSQL and duckdb, which runs the SQLQuery Libraries.
    fn ready(test: &str) -> bool {
        let duckdb = std::process::Command::new("duckdb")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !duckdb {
            eprintln!("skipped without duckdb: {test}");
        }
        url().is_some() && duckdb
    }

    /// A SQLView (`view`) or SQLQuery Library named `name`; `deps` are (label,
    /// canonical) pairs.
    fn library(name: &str, view: bool, deps: &[(&str, &str)], sql: &str) -> serde_json::Value {
        use base64::Engine;
        let related: Vec<serde_json::Value> = deps
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
            "content": [{"contentType": "application/sql",
                "data": base64::engine::general_purpose::STANDARD.encode(sql)}],
        })
    }

    /// IcrReading: Observations with their location and value, plus `extra`
    /// columns.
    fn reading_view(views: &std::path::Path, extra: &[serde_json::Value]) {
        let mut columns = vec![
            json!({"name": "id", "path": "getResourceKey()", "type": "id"}),
            json!({"name": "location_id", "path": "subject.getReferenceKey(Location)", "type": "string"}),
            json!({"name": "value", "path": "value.ofType(integer)", "type": "integer"}),
        ];
        columns.extend(extra.iter().cloned());
        let reading = json!({"resourceType": "ViewDefinition", "url": READING,
            "name": "IcrReading", "status": "active", "resource": "Observation",
            "select": [{"column": columns}]});
        fs::write(views.join("IcrReading.json"), reading.to_string()).unwrap();
    }

    fn write_libraries(root: &std::path::Path, libraries: &[serde_json::Value]) {
        let libs = root.join("libs");
        let _ = fs::remove_dir_all(&libs);
        fs::create_dir_all(&libs).unwrap();
        for (i, l) in libraries.iter().enumerate() {
            fs::write(libs.join(format!("{i}.json")), l.to_string()).unwrap();
        }
    }

    /// An Observation at Location `loc`.
    fn at(id: &str, loc: &str, v: i64) -> serde_json::Value {
        json!({"resourceType": "Observation", "id": id, "status": "final", "code": {"text": "x"},
            "subject": {"reference": format!("Location/{loc}")}, "valueInteger": v})
    }

    /// The hierarchy fixture's Locations and `observations`, as input files.
    fn library_input(
        root: &std::path::Path,
        observations: &[serde_json::Value],
    ) -> std::path::PathBuf {
        let locations = fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/hierarchy/locations.ndjson"),
        )
        .unwrap();
        let _ = fs::remove_dir_all(root.join("input"));
        inputs(
            root,
            &[
                ("locations.ndjson", locations),
                ("obs.ndjson", lines(observations)),
            ],
        )
    }

    /// A warehouse loading into `database` (a [database] section), whose
    /// config names `libs/` as a library source holding `libraries`, with the
    /// fixture views and IcrReading. Returns (dir, config, views directory).
    fn library_setup(
        database: &str,
        libraries: &[serde_json::Value],
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let (dir, config) = setup(NOWHERE, database);
        let root = dir.path();
        let text = fs::read_to_string(&config).unwrap();
        fs::write(&config, format!("libraries = ['libs']\n{text}")).unwrap();
        write_libraries(root, libraries);
        let views = root.join("my-views");
        fs::create_dir_all(&views).unwrap();
        for f in fs::read_dir(fixtures().join("views")).unwrap() {
            let f = f.unwrap().path();
            fs::copy(&f, views.join(f.file_name().unwrap())).unwrap();
        }
        reading_view(&views, &[]);
        (dir, config, views)
    }

    fn library_run(
        config: &std::path::Path,
        views: &std::path::Path,
        only: &str,
    ) -> assert_cmd::Command {
        let mut c = cmd(config);
        c.args(["--only", only, "--views"]).arg(views);
        if only != "libraries" {
            c.arg("--input").arg(config.parent().unwrap().join("input"));
        }
        c
    }

    fn replace_section(s: &str) -> String {
        format!("\n[database]\nurl_env = \"{URL}\"\nschema = \"{s}\"\n")
    }

    /// A ReadingByArea-style SQLView over the reading view and the hierarchy.
    fn by_area() -> serde_json::Value {
        library(
            "ReadingByArea",
            true,
            &[("r", READING), ("h", HIERARCHY)],
            "SELECT r.id, r.value, h.admin0_name, h.admin1_name, h.admin2_name \
             FROM r LEFT JOIN h ON h.id = r.location_id",
        )
    }

    fn type_of(s: &str, table: &str, column: &str) -> String {
        one(&format!(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = '\"{s}\".\"{table}\"'::regclass AND attname = '{column}'"
        ))
    }

    /// DuckDB's types beyond the views': DECIMAL, SMALLINT and REAL load as
    /// numeric, smallint and real. `sum(bigint)` is a HUGEINT in DuckDB,
    /// which it writes to Parquet as a DOUBLE (DuckDB 1.5 and 2.0) or a
    /// DECIMAL(38,0): either loads.
    #[test]
    fn derived_tables_with_decimal_and_smallint_load() {
        if !ready("derived_tables_with_decimal_and_smallint_load") {
            return;
        }
        let s = schema("derived");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let sums = library(
            "ReadingSums",
            false,
            &[("r", READING)],
            "SELECT country, sum(CAST(value AS BIGINT)) AS total, \
             CAST(sum(value) AS DECIMAL(38,0)) AS exact, CAST(1.5 AS DECIMAL(10,2)) AS price, \
             CAST(1 AS SMALLINT) AS one, CAST(2 AS TINYINT) AS two, CAST(0.5 AS REAL) AS half, \
             count(*) AS n FROM r GROUP BY country",
        );
        let (dir, config, views) = library_setup(&replace_section(&s), &[sums]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 30), at("o2", "aa-r1", 5)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success()
            .stdout(has(format!(
                "location_hierarchy → {s}.location_hierarchy (21 rows)"
            )))
            .stdout(has(format!("reading_sums → {s}.reading_sums (1 rows)")));
        for (column, wanted) in [
            ("exact", "numeric(38,0)"),
            ("price", "numeric(10,2)"),
            ("one", "smallint"),
            ("two", "smallint"),
            ("half", "real"),
            ("n", "bigint"),
            // The partition column is in the files too.
            ("country", "text"),
        ] {
            assert_eq!(type_of(&s, "reading_sums", column), wanted, "{column}");
        }
        assert!(
            ["numeric(38,0)", "double precision"]
                .contains(&type_of(&s, "reading_sums", "total").as_str())
        );
        assert_eq!(
            one(&format!(
                "SELECT concat_ws(' ', coalesce(country, 'none'), total::bigint, exact, price, \
                 one, two, half, n) FROM \"{s}\".reading_sums"
            )),
            // The partition lookup finds no country for these Locations.
            "none 35 35 1.50 1 2 0.5 2"
        );
        // A second run finds the tables as it made them.
        library_run(&config, &views, "locations,views")
            .assert()
            .success()
            .stdout(has(format!("reading_sums → {s}.reading_sums (1 rows)")));
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM \"{s}\".location_hierarchy"
            )),
            "21"
        );
    }

    /// Each SQLView is a PostgreSQL view over the loaded tables; one SQLView
    /// read by another is that one's view, not inlined.
    #[test]
    fn sqlviews_are_created_as_views() {
        if !ready("sqlviews_are_created_as_views") {
            return;
        }
        let s = schema("sqlviews");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let by_region = library(
            "ReadingByRegion",
            true,
            &[("b", "https://example.org/Library/ReadingByArea")],
            "SELECT admin1_name, sum(value) AS total FROM b GROUP BY admin1_name",
        );
        let areas = library(
            "ReadingAreas",
            false,
            &[("b", "https://example.org/Library/ReadingByArea")],
            "SELECT * FROM b ORDER BY id",
        );
        let (dir, config, views) =
            library_setup(&replace_section(&s), &[by_region, by_area(), areas]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4), at("o2", "aa-r1", 9)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success()
            .stdout(has(format!("ReadingByArea → {s}.reading_by_area (view)")));
        assert_eq!(
            one(&format!(
                "SELECT string_agg(c.relname || ':' || c.relkind::text || ':' || \
                 coalesce(obj_description(c.oid, 'pg_class'), ''), ',' ORDER BY c.relname) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = '{s}' AND c.relkind IN ('r', 'v')"
            )),
            "location_hierarchy:r:,obs:r:,patient:r:,reading:r:,reading_areas:r:,\
             reading_by_area:v:fhir-pipes SQLView https://example.org/Library/ReadingByArea,\
             reading_by_region:v:fhir-pipes SQLView https://example.org/Library/ReadingByRegion"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(concat_ws(':', id, value, admin0_name, admin1_name, admin2_name), \
                 ',' ORDER BY id) FROM \"{s}\".reading_by_area"
            )),
            "o1:4:Country A:Region 1:District 1,o2:9:Country A:Region 1"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(admin1_name || '=' || total, ',') FROM \"{s}\".reading_by_region"
            )),
            "Region 1=13"
        );
        // ReadingByRegion reads the view reading_by_area.
        assert!(
            one(&format!(
                "SELECT pg_get_viewdef('\"{s}\".reading_by_region'::regclass)"
            ))
            .contains("reading_by_area")
        );
        assert_eq!(
            one(&format!("SELECT count(*)::text FROM \"{s}\".reading_areas")),
            "2"
        );
    }

    /// `--only libraries` loads the derived tables and the views, and leaves
    /// the view tables alone.
    #[test]
    fn only_libraries_leaves_the_view_tables_alone() {
        if !ready("only_libraries_leaves_the_view_tables_alone") {
            return;
        }
        let s = schema("onlylib");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let totals = |plus: i64| {
            library(
                "ReadingTotals",
                false,
                &[("r", READING)],
                &format!("SELECT sum(value) + {plus} AS total FROM r"),
            )
        };
        let (dir, config, views) = library_setup(&replace_section(&s), &[totals(0), by_area()]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success();
        sql(&[&format!(
            "INSERT INTO \"{s}\".reading (id, value) VALUES ('marker', 1)"
        )]);
        write_libraries(dir.path(), &[totals(100), by_area()]);
        library_run(&config, &views, "libraries")
            .assert()
            .success()
            .stdout(has(format!("reading_totals → {s}.reading_totals (1 rows)")))
            .stdout(has(format!("location_hierarchy → {s}.location_hierarchy")))
            .stdout(has(format!("{s}.reading ")).not());
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',' ORDER BY id) FROM \"{s}\".reading"
            )),
            "marker,o1"
        );
        assert_eq!(
            one(&format!("SELECT total::text FROM \"{s}\".reading_totals")),
            "104"
        );
        // The view was made again, over the table with the marker.
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',' ORDER BY id) FROM \"{s}\".reading_by_area"
            )),
            "marker,o1"
        );
    }

    /// The input view gains a column: its table is recreated, which the
    /// views on it would block, so they are dropped first and made again.
    #[test]
    fn a_column_change_recreates_the_views() {
        if !ready("a_column_change_recreates_the_views") {
            return;
        }
        let s = schema("colchange");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let everything = library("ReadingAll", true, &[("r", READING)], "SELECT * FROM r");
        let (dir, config, views) = library_setup(&replace_section(&s), &[by_area(), everything]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success();
        reading_view(
            &views,
            &[json!({"name": "status", "path": "status", "type": "code"})],
        );
        library_run(&config, &views, "views").assert().success();
        assert_eq!(type_of(&s, "reading", "status"), "text");
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || ':' || admin1_name, ',') FROM \"{s}\".reading_by_area"
            )),
            "o1:Region 1"
        );
        // `SELECT *` is expanded when the view is made: the new one has the
        // new column.
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id || ':' || status, ',') FROM \"{s}\".reading_all"
            )),
            "o1:final"
        );
    }

    /// A SQLView PostgreSQL cannot make rolls the whole load back: the tables
    /// keep the last run's rows and the views are still there.
    #[test]
    fn a_view_that_fails_rolls_back_the_load() {
        if !ready("a_view_that_fails_rolls_back_the_load") {
            return;
        }
        let s = schema("viewfail");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let (dir, config, views) = library_setup(&replace_section(&s), &[by_area()]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success();
        // list_value is DuckDB's; PostgreSQL has no such function.
        let broken = library(
            "Broken",
            true,
            &[("r", READING)],
            "SELECT id, list_value(value) AS l FROM r",
        );
        write_libraries(dir.path(), &[by_area(), broken]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4), at("o2", "aa-r1", 9)]);
        library_run(&config, &views, "views")
            .assert()
            .failure()
            .stderr(has("Library Broken"))
            .stderr(has("list_value"));
        assert_eq!(
            one(&format!("SELECT string_agg(id, ',') FROM \"{s}\".reading")),
            "o1"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',') FROM \"{s}\".reading_by_area"
            )),
            "o1"
        );
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = '{s}' AND c.relname = 'broken'"
            )),
            "0"
        );
    }

    /// Under upsert the derived tables have no resource key and are loaded in
    /// full; a run without the views step does not make the next one a full
    /// load.
    #[test]
    fn derived_tables_load_in_full_under_upsert() {
        if !ready("derived_tables_load_in_full_under_upsert") {
            return;
        }
        let s = schema("derived_upsert");
        let _cleanup = Cleanup(vec![format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE")]);
        let totals = library(
            "ReadingTotals",
            false,
            &[("r", READING)],
            "SELECT sum(value) AS total FROM r",
        );
        let (dir, config, views) = library_setup(&upsert(&s), &[totals, by_area()]);
        library_input(dir.path(), &[at("o1", "aa-hf1", 4)]);
        library_run(&config, &views, "locations,views")
            .assert()
            .success()
            .stdout(has(format!("reading_totals → {s}.reading_totals (1 rows)")));
        library_run(&config, &views, "libraries")
            .assert()
            .success()
            .stdout(has(format!("reading_totals → {s}.reading_totals (1 rows)")));
        library_input(dir.path(), &[at("o1", "aa-hf1", 4), at("o2", "aa-r1", 9)]);
        library_run(&config, &views, "views")
            .assert()
            .success()
            .stdout(has("every table in full").not())
            .stdout(has(format!(
                "reading → {s}.reading (1 resources changed or deleted: 0 rows deleted, 1 copied)"
            )))
            .stdout(has(format!("reading_totals → {s}.reading_totals (1 rows)")));
        assert_eq!(
            one(&format!("SELECT total::text FROM \"{s}\".reading_totals")),
            "13"
        );
        assert_eq!(
            one(&format!(
                "SELECT string_agg(id, ',' ORDER BY id) FROM \"{s}\".reading_by_area"
            )),
            "o1,o2"
        );
    }
}
