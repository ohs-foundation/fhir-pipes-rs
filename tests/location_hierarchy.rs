//! The built-in `LocationHierarchy` SQLView (views/location/) against the
//! `location_hierarchy` table fhir-pipes writes from its own partOf walk
//! (`transform locations --hierarchy-out`). Other engines run the SQL instead
//! of the walk, so on clean data the two must agree in every row and column,
//! in DuckDB and in PostgreSQL.
//!
//! The fixture (tests/fixtures/hierarchy/locations.ndjson) is clean: no
//! duplicate ids, no cycles, no chain deeper than 12, which the walk drops and
//! the SQL does not. It has a three-level admin tree, a non-admin Location
//! between two admin units, an admin unit with no pcode, orphans whose partOf
//! names a missing Location, a root that is not an admin unit, a Location
//! with no name and a chain of 6 admin levels.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

const HIERARCHY: &str = "https://healthcampaigns.org/fhir/analytics/Library/LocationHierarchy";
/// The fixture is ICR-shaped: `admin-unit` is the admin type and `pcode`
/// (system https://icr.healthcampaigns.org/identifiers/pcode) the admin code,
/// as the Location view's default constants have it.
const ICR_PROFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/profiles/icr.toml");

/// The columns of both tables, sorted: the Rust table puts `country` last
/// (it is the partition column), the SQL puts it before `path`.
const COLUMNS: [&str; 18] = [
    "admin0_code",
    "admin0_name",
    "admin1_code",
    "admin1_name",
    "admin2_code",
    "admin2_name",
    "admin3_code",
    "admin3_name",
    "admin4_code",
    "admin4_name",
    "admin_level",
    "country",
    "depth",
    "id",
    "is_admin",
    "name",
    "part_of",
    "path",
];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hierarchy")
}

fn fixture_rows() -> usize {
    fs::read_to_string(fixture().join("locations.ndjson"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn pipes() -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env_remove("FHIR_PIPES_PROFILE");
    c
}

/// A warehouse with both inputs: `parquet/location/part-0.parquet` from the
/// Location view and `parquet/location_hierarchy/country=*/` from the walk.
struct Warehouse {
    dir: tempfile::TempDir,
}

impl Warehouse {
    fn build() -> Warehouse {
        let dir = tempfile::tempdir().unwrap();
        let w = Warehouse { dir };
        // transform saves the profile it ran with in the snapshot directory,
        // so the snapshot is a copy, not the fixture directory.
        let snapshot = w.root().join("snapshot");
        fs::create_dir_all(&snapshot).unwrap();
        fs::copy(
            fixture().join("locations.ndjson"),
            snapshot.join("locations.ndjson"),
        )
        .unwrap();
        pipes()
            .args([
                "transform",
                "locations",
                "--profile",
                ICR_PROFILE,
                "--snapshot",
            ])
            .arg(&snapshot)
            .arg("--out")
            .arg(w.dir.path().join("transform"))
            .arg("--hierarchy-out")
            .arg(w.hierarchy())
            .assert()
            .success();
        let location = w.location();
        fs::create_dir_all(location.parent().unwrap()).unwrap();
        pipes()
            .args(["views", "run"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("views/location/Location.json"))
            .arg("--input")
            .arg(fixture().join("locations.ndjson"))
            .args(["--output", "parquet", "--out"])
            .arg(&location)
            .assert()
            .success();
        w
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn location(&self) -> PathBuf {
        self.root().join("parquet/location/part-0.parquet")
    }

    fn hierarchy(&self) -> PathBuf {
        self.root().join("parquet/location_hierarchy")
    }

    /// The Parquet files of the walk's table, one per country.
    #[cfg(feature = "postgres")]
    fn hierarchy_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for part in fs::read_dir(self.hierarchy()).unwrap() {
            let part = part.unwrap().path();
            for f in fs::read_dir(&part).unwrap() {
                files.push(f.unwrap().path());
            }
        }
        files.sort();
        files
    }

    /// LocationHierarchy's portable SQL (views/location/LocationHierarchy.sql)
    /// with the label `location` bound to this warehouse's Location Parquet,
    /// composed as fhir-pipes composes a Library: the input in a CTE named
    /// by the label. `libraries sql` does not print this for the built-in:
    /// it reads the `location_hierarchy` table, as the refresh does.
    /// What `libraries sql` prints for LocationHierarchy: a read of this
    /// warehouse's `location_hierarchy`, without the trailing `;`.
    fn printed_sql(&self) -> String {
        let out = pipes()
            .args(["libraries", "sql", HIERARCHY, "--warehouse"])
            .arg(self.root())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        text.trim_end().trim_end_matches(';').to_string()
    }

    fn library_sql(&self) -> String {
        let sql = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("views/location/LocationHierarchy.sql"),
        )
        .unwrap();
        format!(
            "WITH \"location\" AS (SELECT * FROM read_parquet({}, hive_partitioning = true, \
             union_by_name = true, hive_types_autocast = false))\nSELECT * FROM (\n{}\n) AS q",
            sql_string(&self.root().join("parquet/location/**/*.parquet")),
            sql.trim_end().trim_end_matches(';')
        )
    }
}

/// The comma-separated column list, in `COLUMNS` order.
fn column_list() -> String {
    COLUMNS.join(", ")
}

/// How many rows of table `a` are not in table `b`. Both engines compare
/// table `s` (the SQL) with table `r` (the walk) this way, in both directions.
fn rows_only_in(a: &str, b: &str) -> String {
    let cols = column_list();
    format!("SELECT count(*) FROM (SELECT {cols} FROM {a} EXCEPT SELECT {cols} FROM {b}) AS d")
}

/// The rows of table `a` that are not in table `b`, for a failure message.
fn diff_rows(a: &str, b: &str) -> String {
    let cols = column_list();
    format!("SELECT {cols} FROM {a} EXCEPT SELECT {cols} FROM {b}")
}

/// Both tables have the columns in `COLUMNS` with the same types: BIGINT for
/// depth and admin_level, BOOLEAN for is_admin, text for the rest. `sql` and
/// `rust` are (name, type) pairs sorted by name.
fn assert_same_columns(engine: &str, sql: &[(String, String)], rust: &[(String, String)]) {
    let names: Vec<&str> = sql.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, COLUMNS, "{engine}: the SQL's columns");
    assert_eq!(sql, rust, "{engine}: column types, SQL then Rust");
    for (name, ty) in sql {
        let want = match name.as_str() {
            "depth" | "admin_level" => "bigint",
            "is_admin" => "boolean",
            _ => "text",
        };
        assert_eq!(ty, want, "{engine}: {name}");
    }
}

/// Whether the duckdb command is installed; the test is named on stderr when
/// it is not (written directly, so a plain `cargo test` shows it).
fn have_duckdb(skipped: &str) -> bool {
    let found = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
}

/// Run `sql` in the DuckDB database `db`; stdout, one row per line.
fn duckdb(db: &Path, sql: &str) -> String {
    let mut child = std::process::Command::new("duckdb")
        .arg(db)
        .args(["-noheader", "-csv"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{sql}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sql_string(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// DuckDB's type names in PostgreSQL's spelling, so both tests check the
/// same names.
fn duckdb_columns(db: &Path, table: &str) -> Vec<(String, String)> {
    duckdb(
        db,
        &format!(
            "SELECT column_name, lower(data_type) FROM information_schema.columns \
             WHERE table_name = '{table}' ORDER BY column_name;"
        ),
    )
    .lines()
    .map(|l| {
        let (n, t) = l.split_once(',').unwrap();
        let t = if t == "varchar" { "text" } else { t };
        (n.to_string(), t.to_string())
    })
    .collect()
}

#[test]
fn the_sql_matches_the_rust_in_duckdb() {
    if !have_duckdb("the_sql_matches_the_rust_in_duckdb") {
        return;
    }
    let w = Warehouse::build();
    let db = w.root().join("check.duckdb");
    duckdb(
        &db,
        &format!(
            "CREATE TABLE s AS\n{};\n\
             CREATE TABLE r AS SELECT * FROM read_parquet({}, hive_partitioning = true, union_by_name = true);",
            w.library_sql(),
            sql_string(&w.hierarchy().join("**/*.parquet"))
        ),
    );
    assert_same_columns(
        "duckdb",
        &duckdb_columns(&db, "s"),
        &duckdb_columns(&db, "r"),
    );
    let n = fixture_rows().to_string();
    assert_eq!(duckdb(&db, "SELECT count(*) FROM s;"), n);
    assert_eq!(duckdb(&db, "SELECT count(*) FROM r;"), n);
    // `libraries sql` for the built-in reads the walk's table.
    let printed = w.printed_sql();
    assert!(!printed.contains("WITH RECURSIVE"), "{printed}");
    assert_eq!(
        duckdb(&db, &format!("SELECT count(*) FROM ({printed}) AS p;")),
        n
    );
    for (a, b) in [("s", "r"), ("r", "s")] {
        assert_eq!(
            duckdb(&db, &format!("{};", rows_only_in(a, b))),
            "0",
            "rows only in {a} ({}):\n{}",
            column_list(),
            duckdb(&db, &format!("{};", diff_rows(a, b)))
        );
    }
}

/// Runs against the PostgreSQL in $FHIR_PIPES_TEST_DATABASE_URL, in a schema
/// of its own; skipped without it.
#[cfg(feature = "postgres")]
mod postgresql {
    use super::*;
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::DataType;
    use sqlx_core::connection::Connection;
    use sqlx_core::row::Row;
    use sqlx_core::sql_str::AssertSqlSafe;

    const URL: &str = "FHIR_PIPES_TEST_DATABASE_URL";

    fn url() -> Option<String> {
        std::env::var(URL).ok().filter(|u| !u.is_empty())
    }

    fn schema() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("t_hierarchy_{}_{nanos}", std::process::id())
    }

    /// Run SQL statements; the rows of the last one, every value as text.
    fn sql(statements: &[String]) -> Vec<Vec<Option<String>>> {
        let url = url().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut conn = sqlx_postgres::PgConnection::connect(&url).await.unwrap();
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

    fn one(statement: String) -> String {
        sql(&[statement])[0][0].clone().unwrap_or_default()
    }

    /// Drops the schema, failures ignored: the cleanup of a test that may
    /// fail part way.
    struct DropSchema(String);

    impl Drop for DropSchema {
        fn drop(&mut self) {
            let Some(url) = url() else { return };
            let s = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.0);
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    if let Ok(mut conn) = sqlx_postgres::PgConnection::connect(&url).await {
                        let _ = sqlx_core::query::query(AssertSqlSafe(s))
                            .execute(&mut conn)
                            .await;
                    }
                });
            }
        }
    }

    /// The PostgreSQL type of a Parquet column, as the database step maps it
    /// (src/warehouse/database.rs).
    fn pg_type(t: &DataType) -> &'static str {
        match t {
            DataType::Utf8 | DataType::LargeUtf8 => "text",
            DataType::Int64 => "bigint",
            DataType::Int32 => "integer",
            DataType::Boolean => "boolean",
            other => panic!("no PostgreSQL type for {other}"),
        }
    }

    fn literal(t: &DataType, value: &str) -> String {
        match t {
            DataType::Utf8 | DataType::LargeUtf8 => format!("'{}'", value.replace('\'', "''")),
            _ => value.to_string(),
        }
    }

    /// CREATE TABLE and INSERT statements that load `files` (Parquet with one
    /// schema) into `table`.
    fn load(table: &str, files: &[PathBuf]) -> Vec<String> {
        let mut statements = Vec::new();
        for (k, file) in files.iter().enumerate() {
            let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
                fs::File::open(file).unwrap(),
            )
            .unwrap()
            .build()
            .unwrap();
            for batch in reader {
                let batch = batch.unwrap();
                let schema = batch.schema();
                if k == 0 && statements.is_empty() {
                    let cols: Vec<String> = schema
                        .fields()
                        .iter()
                        .map(|f| format!("{} {}", f.name(), pg_type(f.data_type())))
                        .collect();
                    statements.push(format!("CREATE TABLE {table} ({})", cols.join(", ")));
                }
                let text: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|c| arrow::compute::cast(c, &DataType::Utf8).unwrap())
                    .collect();
                let mut rows = Vec::new();
                for r in 0..batch.num_rows() {
                    let values: Vec<String> = schema
                        .fields()
                        .iter()
                        .zip(&text)
                        .map(|(f, col)| {
                            if col.is_null(r) {
                                "NULL".to_string()
                            } else {
                                literal(f.data_type(), col.as_string::<i32>().value(r))
                            }
                        })
                        .collect();
                    rows.push(format!("({})", values.join(", ")));
                }
                if !rows.is_empty() {
                    let names: Vec<&str> =
                        schema.fields().iter().map(|f| f.name().as_str()).collect();
                    statements.push(format!(
                        "INSERT INTO {table} ({}) VALUES {}",
                        names.join(", "),
                        rows.join(", ")
                    ));
                }
            }
        }
        statements
    }

    /// The composed SQL (`Warehouse::library_sql`) with its one input, the
    /// `read_parquet(...)` that `location` is bound to, replaced by `table`.
    fn bind_location(composed: &str, table: &str) -> String {
        let start = composed
            .find("read_parquet(")
            .unwrap_or_else(|| panic!("no read_parquet in\n{composed}"));
        let end = start + composed[start..].find(')').unwrap() + 1;
        assert!(
            !composed[end..].contains("read_parquet("),
            "more than one input:\n{composed}"
        );
        format!("{}{table}{}", &composed[..start], &composed[end..])
    }

    fn columns(schema: &str, table: &str) -> Vec<(String, String)> {
        sql(&[format!(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = '{schema}' AND table_name = '{table}' ORDER BY column_name"
        )])
        .into_iter()
        .map(|r| {
            let n = r[0].clone().unwrap();
            let t = r[1].clone().unwrap();
            // CAST(... AS VARCHAR) is character varying; the database step
            // loads strings as text. Both are unbounded strings.
            let t = if t == "character varying" {
                "text".to_string()
            } else {
                t
            };
            (n, t)
        })
        .collect()
    }

    #[test]
    fn the_sql_matches_the_rust_in_postgresql() {
        if url().is_none() {
            let _ = writeln!(
                std::io::stderr(),
                "skipped without ${URL}: the_sql_matches_the_rust_in_postgresql"
            );
            return;
        }
        let w = Warehouse::build();
        let s = schema();
        let _cleanup = DropSchema(s.clone());
        let mut statements = vec![format!("CREATE SCHEMA {s}")];
        statements.extend(load(&format!("{s}.location"), &[w.location()]));
        statements.extend(load(&format!("{s}.r"), &w.hierarchy_files()));
        let query = bind_location(&w.library_sql(), &format!("{s}.location"));
        statements.push(format!("CREATE TABLE {s}.s AS\n{query}"));
        sql(&statements);

        assert_same_columns("postgresql", &columns(&s, "s"), &columns(&s, "r"));
        let n = fixture_rows().to_string();
        assert_eq!(one(format!("SELECT count(*)::text FROM {s}.s")), n);
        assert_eq!(one(format!("SELECT count(*)::text FROM {s}.r")), n);
        for (a, b) in [("s", "r"), ("r", "s")] {
            let (a, b) = (format!("{s}.{a}"), format!("{s}.{b}"));
            let only = one(format!("SELECT ({})::text", rows_only_in(&a, &b)));
            if only != "0" {
                let rows = sql(&[format!(
                    "SELECT row_to_json(d)::text FROM ({}) AS d",
                    diff_rows(&a, &b)
                )]);
                panic!("rows only in {a} ({}):\n{rows:#?}", column_list());
            }
        }
    }
}
