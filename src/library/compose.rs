//! Composing a Library's SQL with its inputs. Each `depends-on` label becomes
//! a CTE local to the Library, so two Libraries can use one label for
//! different inputs, and no label replaces a warehouse table:
//!
//! ```sql
//! WITH "<label1>" AS (<input1>), "<label2>" AS (<input2>)
//! SELECT * FROM (
//! <library sql>
//! ) AS q
//! ```

use anyhow::{Context, Result, anyhow, bail};

use super::resolve::{Artifact, Catalog, HIERARCHY_URL, LOCATION_URL, location_dependency};
use super::{Kind, Library};
use crate::warehouse::sql::ident;

/// Where an input is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// A SQL relation expression: a table name or a table function call.
    Relation(String),
}

/// The SQL that runs `lib` in `dialect`, with each input read from what
/// `bind` returns for it. `bind` returning `None` inlines a SQLView's own
/// composed SQL; every other input must be bound to a relation.
pub fn compose(
    lib: &Library,
    dialect: &str,
    catalog: &Catalog,
    bind: &dyn Fn(&Artifact) -> Result<Option<Bind>>,
) -> Result<String> {
    let sql = lib.sql(dialect).ok_or_else(|| {
        anyhow!(
            "Library {} has no application/sql content and none for {dialect}",
            lib.name
        )
    })?;
    let mut inputs = Vec::new();
    for dep in &lib.depends_on {
        let what = || format!("Library {} depends-on {}", lib.name, dep.label);
        // Only the built-in hierarchy reads the Location view.
        if dep.canonical == LOCATION_URL && lib.url != HIERARCHY_URL {
            bail!("{}: {}", what(), location_dependency());
        }
        let input = catalog
            .get(&dep.canonical, dep.version.as_deref())
            .with_context(what)?;
        let query = match bind(input).with_context(what)? {
            Some(Bind::Relation(relation)) => format!("SELECT * FROM {relation}"),
            None => match input {
                Artifact::Library(l) if l.kind == Kind::SqlView && l.url != HIERARCHY_URL => {
                    compose(l, dialect, catalog, bind)?
                }
                _ => bail!(
                    "{}: {} must be bound to a table; only a SQLView's SQL can be inlined",
                    what(),
                    input.url()
                ),
            },
        };
        inputs.push(format!("{} AS ({query})", ident(&dep.label)));
    }
    let with = if inputs.is_empty() {
        String::new()
    } else {
        format!("WITH {}\n", inputs.join(", "))
    };
    // The newline before `)` keeps a trailing `--` comment from swallowing it.
    Ok(format!(
        "{with}SELECT * FROM (\n{}\n) AS q",
        strip_terminator(sql)
    ))
}

/// `sql` without its trailing `;` statement terminators and trailing
/// whitespace. A `;` followed only by comments is a terminator too, and is
/// cut with the comments after it. A `;` inside a string, quoted identifier
/// or comment is left alone.
fn strip_terminator(sql: &str) -> &str {
    let mut text = sql;
    while let Some(i) = last_code_byte(text).filter(|&i| text.as_bytes()[i] == b';') {
        text = &text[..i];
    }
    text.trim_end()
}

/// Whether `sql` holds more than one statement: a `;` in its code (not in a
/// string, quoted identifier or comment) before its trailing terminators. A
/// Library's SQL is one query; a second statement would run as well.
pub fn has_several_statements(sql: &str) -> bool {
    let text = strip_terminator(sql);
    code_bytes(text)
        .into_iter()
        .any(|i| text.as_bytes()[i] == b';')
}

/// The byte index of the last byte of `sql` that is SQL code: not
/// whitespace, not in a comment, and not inside a string or quoted
/// identifier (a closing quote counts as code).
fn last_code_byte(sql: &str) -> Option<usize> {
    code_bytes(sql).last().copied()
}

/// The byte indexes of `sql` that are SQL code, in order: not whitespace,
/// not in a comment, and not inside a string or quoted identifier (a closing
/// quote counts as code).
fn code_bytes(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut code = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            q @ (b'\'' | b'"') => {
                // A doubled quote inside is two quoted runs back to back.
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                code.push(i.min(bytes.len() - 1));
                i += 1;
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                code.push(i);
                i += 1;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;

    use serde_json::{Value, json};

    use super::*;
    use crate::library::builtin;
    use crate::library::resolve::{HIERARCHY_URL, LOCATION_URL};
    use crate::library::{Kind, SQLQUERY_PROFILE, SQLVIEW_PROFILE};

    const BASE: &str = "https://example.org/Library/";
    const V: &str = "https://example.org/ViewDefinition/V";

    /// Library JSON with portable `sql`; `deps` are (label, resource) pairs.
    fn lib_json(name: &str, view: bool, sql: &str, deps: &[(&str, &str)]) -> Value {
        let profile = if view {
            SQLVIEW_PROFILE
        } else {
            SQLQUERY_PROFILE
        };
        let related: Vec<Value> = deps
            .iter()
            .map(|(l, r)| json!({"type": "depends-on", "label": l, "resource": r}))
            .collect();
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(sql);
        json!({
            "resourceType": "Library",
            "meta": {"profile": [profile]},
            "url": format!("{BASE}{name}"),
            "name": name,
            "status": "draft",
            "relatedArtifact": related,
            "content": [{"contentType": "application/sql", "data": data}],
        })
    }

    fn lib(name: &str, view: bool, sql: &str, deps: &[(&str, &str)]) -> Library {
        Library::from_json(&lib_json(name, view, sql, deps)).unwrap()
    }

    /// A catalog of the built-ins, view V and `libs`.
    fn catalog(libs: &[Value]) -> (tempfile::TempDir, Catalog) {
        let d = tempfile::tempdir().unwrap();
        let v = json!({
            "resourceType": "ViewDefinition", "url": V, "name": "V",
            "resource": "Patient", "status": "active",
            "select": [{"column": [{"name": "id", "path": "id"}]}],
        });
        fs::write(d.path().join("v.json"), v.to_string()).unwrap();
        for (i, l) in libs.iter().enumerate() {
            fs::write(d.path().join(format!("{i}.json")), l.to_string()).unwrap();
        }
        let sources = [d.path().to_str().unwrap().to_string()];
        let cat = Catalog::load(&sources, Path::new("/"), &[]).unwrap();
        (d, cat)
    }

    /// Binds every view and Library to `tbl_<table>`; SQLViews are inlined.
    fn bind_tables(a: &Artifact) -> Result<Option<Bind>> {
        match a {
            Artifact::Library(l) if l.kind == Kind::SqlView && l.url != HIERARCHY_URL => Ok(None),
            _ => Ok(Some(Bind::Relation(format!("tbl_{}", a.table(&[]))))),
        }
    }

    #[test]
    fn labels_are_local_ctes() {
        let (_d, cat) = catalog(&[]);
        let q = lib("Q", false, "SELECT id FROM t", &[("t", V)]);
        let sql = compose(&q, "duckdb", &cat, &bind_tables).unwrap();
        assert!(
            sql.starts_with(r#"WITH "t" AS (SELECT * FROM tbl_v)"#),
            "{sql}"
        );
        assert!(
            sql.ends_with("SELECT * FROM (\nSELECT id FROM t\n) AS q"),
            "{sql}"
        );
    }

    #[test]
    fn a_sqlview_dependency_is_inlined_with_its_own_labels() {
        let s = lib_json("S", true, "SELECT id FROM t", &[("t", V)]);
        let (_d, cat) = catalog(&[s]);
        let q = lib(
            "Q",
            false,
            "SELECT id FROM s",
            &[("s", &format!("{BASE}S"))],
        );
        let sql = compose(&q, "duckdb", &cat, &bind_tables).unwrap();
        assert!(sql.contains(r#""s" AS (WITH "t" AS ("#), "{sql}");
        assert!(sql.contains("SELECT * FROM tbl_v"), "{sql}");
    }

    #[test]
    fn a_view_or_the_hierarchy_must_be_bound() {
        let (_d, cat) = catalog(&[]);
        let none = |_: &Artifact| Ok(None);
        for dep in [V, HIERARCHY_URL] {
            let q = lib("Q", false, "SELECT 1", &[("x", dep)]);
            let err = compose(&q, "duckdb", &cat, &none).unwrap_err();
            assert!(format!("{err:#}").contains("must be bound"), "{err:#}");
        }
    }

    #[test]
    fn location_hierarchy_is_always_bound_to_its_table() {
        let (_d, cat) = catalog(&[]);
        let q = lib("Q", false, "SELECT id FROM h", &[("h", HIERARCHY_URL)]);
        let seen = RefCell::new(Vec::new());
        let bind = |a: &Artifact| {
            seen.borrow_mut().push(a.url().to_string());
            Ok(Some(Bind::Relation("hier_table".into())))
        };
        let sql = compose(&q, "duckdb", &cat, &bind).unwrap();
        assert!(
            sql.starts_with(r#"WITH "h" AS (SELECT * FROM hier_table)"#),
            "{sql}"
        );
        assert_eq!(*seen.borrow(), [HIERARCHY_URL]);
    }

    /// Runs `sql` in the duckdb CLI and returns its output; None without
    /// duckdb.
    fn duckdb(sql: &str) -> Option<String> {
        use std::io::Write;
        let found = std::process::Command::new("duckdb")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            let _ = writeln!(std::io::stderr(), "skipped without duckdb");
            return None;
        }
        let mut child = std::process::Command::new("duckdb")
            .args(["-noheader", "-list"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{sql};\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success() && out.stderr.is_empty(),
            "duckdb failed on:\n{sql}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn compose_handles_trailing_semicolon_and_comment() {
        let (_d, cat) = catalog(&[]);
        for text in [
            "SELECT 1 AS x; -- end",
            "SELECT 1 AS x;\n\n",
            "SELECT 1 AS x -- end",
            "SELECT 1 AS x; /* end; */ ;",
            "SELECT ';' AS y, 1 AS x -- it's;",
        ] {
            let q = lib("Q", false, text, &[]);
            let sql = compose(&q, "duckdb", &cat, &bind_tables).unwrap();
            assert!(sql.contains("\n) AS q"), "{sql}");
            if let Some(out) = duckdb(&format!("SELECT x FROM ({sql}) AS r")) {
                assert_eq!(out.trim(), "1", "{text:?} composed to:\n{sql}");
            }
        }
    }

    #[test]
    fn a_direct_dependency_on_the_location_view_is_refused() {
        let (_d, cat) = catalog(&[]);
        let q = lib("Q", false, "SELECT 1", &[("loc", LOCATION_URL)]);
        let err = compose(&q, "duckdb", &cat, &bind_tables).unwrap_err();
        assert!(
            format!("{err:#}").contains("depend on LocationHierarchy"),
            "{err:#}"
        );
    }

    #[test]
    fn the_hierarchy_itself_composes_over_the_location_view() {
        let (_d, cat) = catalog(&[]);
        let Artifact::Library(h) = cat.get(HIERARCHY_URL, None).unwrap() else {
            panic!("LocationHierarchy is not a Library");
        };
        let sql = compose(h, "duckdb", &cat, &bind_tables).unwrap();
        assert!(
            sql.starts_with(r#"WITH "location" AS (SELECT * FROM tbl_location)"#),
            "{sql}"
        );
    }

    #[test]
    fn the_builtins_parse() {
        let all = builtin::artifacts();
        let loc = all.iter().find(|a| a.url() == LOCATION_URL).unwrap();
        let Artifact::View(v) = loc else {
            panic!("Location is not a view");
        };
        assert_eq!(v.resource, "Location");
        let cols: Vec<&str> = v.json["select"][0]["column"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            cols,
            [
                "id",
                "name",
                "status",
                "part_of",
                "type_code",
                "is_admin",
                "admin_code"
            ]
        );
        let h = all.iter().find(|a| a.url() == HIERARCHY_URL).unwrap();
        let Artifact::Library(h) = h else {
            panic!("LocationHierarchy is not a Library");
        };
        assert_eq!(h.kind, Kind::SqlView);
        assert_eq!(h.depends_on[0].label, "location");
        assert_eq!(h.depends_on[0].canonical, LOCATION_URL);
        assert!(h.sql("duckdb").unwrap().contains("WITH RECURSIVE"));
        assert_eq!(h.table(&[]), "location_hierarchy");
    }
}
