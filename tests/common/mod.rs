//! What the refresh tests share: the fixtures, a warehouse to run in, the
//! command, and readers for what it writes. Each test file uses some of it.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use httptest::{Expectation, Server, all_of, matchers::*, responders::*};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use serde_json::{Value, json};

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refresh")
}

pub fn read_ndjson(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

pub fn bundle(resources: Vec<Value>) -> String {
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Serve each fixture type, once, as a single search page.
pub fn serve_types(server: &Server, types: &[&str]) {
    for t in types {
        let resources = read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")));
        server.expect(
            Expectation::matching(request::method_path("GET", format!("/fhir/{t}")))
                .times(1)
                .respond_with(status_code(200).body(bundle(resources))),
        );
    }
}

/// A warehouse directory with the fixture locations table and a config file
/// pointing at `base`. Returns (tempdir, config path).
pub fn setup(base: &str, extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let loc = root.join("data/parquet/locations/country=SLE");
    fs::create_dir_all(&loc).unwrap();
    fs::copy(
        fixtures().join("parquet/locations/country=SLE/part-0.parquet"),
        loc.join("part-0.parquet"),
    )
    .unwrap();
    let config = root.join("fhir-pipes.toml");
    fs::write(
        &config,
        format!(
            r#"# Literal strings: a Windows path has backslashes.
data = "data"
views = '{views}'
profile = '{profile}'

[server]
base = "{base}"
name = "test"

[tables]
strip_prefix = ["icr_"]

[[partition.lookup]]
key = "location_id"
table = "locations"
table_key = "id"
{extra_config}"#,
            views = fixtures().join("views").display(),
            profile = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/profiles/icr.toml")
                .display()
        ),
    )
    .unwrap();
    (dir, config)
}

/// A server address nothing listens on, for runs that must not reach a server.
pub const NOWHERE: &str = "http://127.0.0.1:9/fhir";

pub fn cmd(config: &Path) -> Command {
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.current_dir(config.parent().unwrap());
    for var in [
        "FHIR_PIPES_CONFIG",
        "FHIR_PIPES_DATA",
        "FHIR_PIPES_VIEWS",
        "FHIR_PIPES_SERVER",
        "FHIR_PIPES_SERVER_NAME",
        "FHIR_PIPES_REUSE_RAW",
        "FHIR_PIPES_TOKEN",
        "FHIR_PIPES_CLIENT_ID",
        "FHIR_PIPES_CLIENT_SECRET",
        "FHIR_PIPES_TOKEN_URL",
        "FHIR_PIPES_USERNAME",
        "FHIR_PIPES_PASSWORD",
        "FHIR_PIPES_GOOGLE_AUTH",
        "FHIR_PIPES_PROFILE",
    ] {
        c.env_remove(var);
    }
    c.arg("refresh").arg("--config").arg(config);
    c
}

/// Rows of a parquet file as (column → display string) maps.
pub fn rows(path: &Path) -> Vec<Vec<(String, String)>> {
    let reader = SerializedFileReader::new(fs::File::open(path).unwrap()).unwrap();
    reader
        .get_row_iter(None)
        .unwrap()
        .map(|r| {
            r.unwrap()
                .get_column_iter()
                .map(|(name, field)| {
                    let v = match field {
                        Field::Str(s) => s.clone(),
                        Field::Null => "null".into(),
                        other => other.to_string(),
                    };
                    (name.clone(), v)
                })
                .collect()
        })
        .collect()
}

pub fn get<'a>(row: &'a [(String, String)], col: &str) -> &'a str {
    &row.iter().find(|(n, _)| n == col).unwrap().1
}

/// The fixture resources of `t`, each with a meta.lastUpdated.
pub fn stamped(t: &str, at: &str) -> Vec<Value> {
    read_ndjson(&fixtures().join(format!("raw/{t}.ndjson")))
        .into_iter()
        .map(|mut r| {
            r["meta"] = json!({"lastUpdated": at});
            r
        })
        .collect()
}

pub fn full_search(
    t: &str,
) -> impl httptest::matchers::Matcher<httptest::http::Request<httptest::bytes::Bytes>> + use<> {
    all_of![
        request::method_path("GET", format!("/fhir/{t}")),
        request::query(url_decoded(not(contains(key("_lastUpdated"))))),
        request::query(url_decoded(not(contains(key("_summary"))))),
    ]
}

/// A fake rclone that copies into $MIRROR as if it were the bucket, and logs
/// each call to $LOG.
#[cfg(unix)]
pub fn fake_rclone(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rclone");
    fs::write(
        &path,
        r#"#!/bin/sh
echo "$1 $(eval echo \${$#})" >> "$LOG"
if [ "$1" = lsd ]; then echo "          -1 2026-01-01 00:00:00        -1 bkt"; exit 0; fi
for a; do src=$dst; dst=$a; done
rest=${dst#*:}
rest=${rest#*/}
case "$1" in
  sync) mkdir -p "$MIRROR/$rest" && cp -R "$src/." "$MIRROR/$rest/" ;;
  copyto) mkdir -p "$(dirname "$MIRROR/$rest")" && cp "$src" "$MIRROR/$rest" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A warehouse with the fixture downloads in raw/, so `--reuse-raw` runs the
/// views without a server.
pub fn setup_with_raw(extra_config: &str) -> (tempfile::TempDir, PathBuf) {
    let (dir, config) = setup(NOWHERE, extra_config);
    let raw = dir.path().join("data/raw/test");
    fs::create_dir_all(&raw).unwrap();
    for t in ["Observation", "Patient"] {
        fs::copy(
            fixtures().join(format!("raw/{t}.ndjson")),
            raw.join(format!("{t}.ndjson")),
        )
        .unwrap();
    }
    (dir, config)
}

/// The fixture views plus Patient views named `names` (each with an `id`
/// column), and, if `empty`, a Patient view that selects nothing.
pub fn views_with(dir: &Path, names: &[&str], empty: bool) -> PathBuf {
    let views = dir.join("more-views");
    fs::create_dir_all(&views).unwrap();
    for f in fs::read_dir(fixtures().join("views")).unwrap() {
        let f = f.unwrap().path();
        fs::copy(&f, views.join(f.file_name().unwrap())).unwrap();
    }
    let view = |name: &str, filter: &str| {
        format!(
            r#"{{"resourceType": "ViewDefinition", "name": "{name}", "status": "active", "resource": "Patient",{filter}
                "select": [{{"column": [{{"name": "id", "path": "getResourceKey()", "type": "id"}},
                                         {{"name": "born", "path": "birthDate", "type": "date"}}]}}]}}"#
        )
    };
    for n in names {
        fs::write(views.join(format!("{n}.json")), view(n, "")).unwrap();
    }
    if empty {
        fs::write(
            views.join("IcrEmpty.json"),
            view("IcrEmpty", r#" "where": [{"path": "false"}],"#),
        )
        .unwrap();
    }
    views
}

/// Every file of the warehouse but raw/, by path relative to it, with its
/// contents.
pub fn warehouse(data: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, data: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(data).unwrap();
            if rel.starts_with("raw") {
                continue;
            }
            if p.is_dir() {
                walk(&p, data, out);
            } else {
                let name = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(name, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(data, data, &mut out);
    out
}

/// The paths of `warehouse`, for assertion messages.
pub fn paths(files: &std::collections::BTreeMap<String, Vec<u8>>) -> Vec<&str> {
    files.keys().map(String::as_str).collect()
}

pub fn assert_nothing_staged(data: &Path) {
    assert!(!data.join(".staging").exists());
    assert!(!data.join("parquet/.staging").exists());
}

/// Write `files` (relative path → text) under `dir/input`; returns that
/// directory.
pub fn inputs(dir: &Path, files: &[(&str, String)]) -> PathBuf {
    let input = dir.join("input");
    for (name, text) in files {
        let path = input.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    input
}

pub fn lines(resources: &[Value]) -> String {
    resources.iter().map(|r| format!("{r}\n")).collect()
}

pub fn obs(id: &str, value: i64, updated: Option<&str>) -> Value {
    let mut o = json!({"resourceType": "Observation", "id": id, "status": "final",
        "code": {"text": "x"}, "subject": {"reference": "Location/loc-bo"}, "valueInteger": value});
    if let Some(at) = updated {
        o["meta"] = json!({"lastUpdated": at});
    }
    o
}

/// The rows of every part of a table, with their partition directory.
pub fn table_rows(table: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    let mut parts: Vec<PathBuf> = fs::read_dir(table)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    parts.sort();
    for part in parts {
        let name = part.file_name().unwrap().to_string_lossy().into_owned();
        for row in rows(&part.join("part-0.parquet")) {
            out.push((name.clone(), row));
        }
    }
    out
}

pub fn snapshot_locations() -> Vec<Value> {
    read_ndjson(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson"),
    )
}

/// A view with an integer and a date column and Observations that do not
/// fit them: 5.0, 5.5 and 2.5, and two dateTimes with a time. Returns (views
/// directory, input directory).
pub fn mismatch_setup(root: &Path) -> (PathBuf, PathBuf) {
    let views = root.join("mismatch-views");
    fs::create_dir_all(&views).unwrap();
    fs::write(
        views.join("IcrMeasures.json"),
        json!({"resourceType": "ViewDefinition", "name": "IcrMeasures", "status": "active",
        "resource": "Observation", "select": [{"column": [
            {"name": "id", "path": "getResourceKey()", "type": "id"},
            {"name": "location_id", "path": "subject.getReferenceKey(Location)", "type": "string"},
            {"name": "n", "path": "value.ofType(Quantity).value", "type": "integer"},
            {"name": "period_start", "path": "effective.ofType(Period).start", "type": "date"},
        ]}]})
        .to_string(),
    )
    .unwrap();
    let measure = |id: &str, value: &str, start: &str| {
        json!({"resourceType": "Observation", "id": id, "status": "final", "code": {"text": "x"},
            "subject": {"reference": "Location/loc-bo"},
            "valueQuantity": {"value": serde_json::from_str::<Value>(value).unwrap()},
            "effectivePeriod": {"start": start}})
    };
    let input = inputs(
        root,
        &[(
            "obs.ndjson",
            lines(&[
                measure("o1", "5.0", "2026-06-15T08:30:00+01:00"),
                measure("o2", "5.5", "2026-06-15T23:30:00-05:00"),
                measure("o3", "2.5", "2026-06-15"),
            ]),
        )],
    );
    (views, input)
}
