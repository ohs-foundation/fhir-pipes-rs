//! `import.toml`: the package's name, its steps in order, its data tests,
//! how records that do not fit their model exactly are read, the limit on a
//! map's values, whether views may read record files and the elements
//! `import run --previous` does not compare.
//! Parsing checks the manifest on its own: the keys, the shape of each step
//! and test, and that every stage step comes before the first library step.
//! What the names refer to is checked against the package (`plan`).

use serde::Deserialize;

use crate::error::{PipesError, Result};

/// The manifest file name in a package folder.
pub const FILE: &str = "import.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub title: Option<String>,
    pub steps: Vec<Step>,
    pub tests: Vec<DataTest>,
    /// `[compare] ignore`: elements left out when `import run --previous`
    /// compares a resource with the previous run's.
    pub ignore: Vec<IgnorePath>,
    /// `[records]`: how the records of the input folder are read.
    pub records: RecordRules,
    /// `[maps] max_values`: the values a map may create from one record.
    pub max_values: Option<usize>,
    /// `[views] records = "allow"`: a view may read a record type's file
    /// directly (a record view), which goes beyond SQL on FHIR.
    pub record_views: bool,
}

/// `[records]`: what a record of the input folder may hold beyond its
/// model. Both are off unless the manifest turns them on. They apply to the
/// record files, not to the rows of a Library result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordRules {
    /// `null = "absent"`: a member whose value is JSON null is read as if
    /// it were not there.
    pub null_absent: bool,
    /// `unknown = "ignore"`: a member the model does not describe is left
    /// out before the record is checked and mapped.
    pub ignore_unknown: bool,
}

/// `ResourceType.path`: an element of a resource type, by the object keys
/// that lead to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnorePath {
    pub resource_type: String,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Records through a map. `stage`: the resources are kept for the
    /// views, not written to the output.
    Map {
        map: String,
        from: Source,
        stage: bool,
    },
    /// A SQLQuery Library, by name or canonical URL, with the views and
    /// SQLQuery Libraries it depends on.
    Library { library: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A record type: `<in>/<type>.ndjson`.
    Records(String),
    /// The rows of a Library's result, by name or canonical URL.
    Library(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataTest {
    pub name: Option<String>,
    pub check: Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Checks on the columns of a Library's result.
    Columns {
        library: String,
        unique: Vec<String>,
        not_null: Vec<String>,
    },
    /// A query that must return no rows.
    Sql(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    title: Option<String>,
    #[serde(default)]
    step: Vec<StepFile>,
    #[serde(default)]
    test: Vec<TestFile>,
    compare: Option<CompareFile>,
    records: Option<RecordsFile>,
    maps: Option<MapsFile>,
    views: Option<ViewsFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewsFile {
    records: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordsFile {
    null: Option<String>,
    unknown: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapsFile {
    max_values: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompareFile {
    #[serde(default)]
    ignore: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    map: Option<String>,
    from: Option<toml::Value>,
    stage: Option<bool>,
    library: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestFile {
    name: Option<String>,
    library: Option<String>,
    unique: Option<Vec<String>>,
    not_null: Option<Vec<String>>,
    sql: Option<String>,
}

fn usage(message: String) -> PipesError {
    PipesError::Usage(message)
}

/// Parses manifest text; `origin` names it in messages.
pub fn parse(text: &str, origin: &str) -> Result<Manifest> {
    let file: File = toml::from_str(text).map_err(|e| usage(format!("{origin}: {e}")))?;
    if file.name.trim().is_empty() {
        return Err(usage(format!("{origin}: name is empty")));
    }
    let mut steps = Vec::new();
    for (i, s) in file.step.into_iter().enumerate() {
        let at = format!("{origin}: step {}", i + 1);
        steps.push(step(s, &at)?);
    }
    check_stage_order(&steps, origin)?;
    let mut tests = Vec::new();
    for (i, t) in file.test.into_iter().enumerate() {
        let at = format!("{origin}: test {}", i + 1);
        tests.push(test(t, &at)?);
    }
    if !tests.is_empty() && !steps.iter().any(|s| matches!(s, Step::Library { .. })) {
        return Err(usage(format!(
            "{origin}: data tests read Library results, and no step runs a Library"
        )));
    }
    let mut ignore = Vec::new();
    for p in file.compare.map(|c| c.ignore).unwrap_or_default() {
        ignore.push(ignore_path(&p).map_err(|m| {
            usage(format!(
                "{origin}: compare.ignore {p:?}: {m}; give ResourceType.path, such as \
                 MeasureReport.date or Observation.meta.tag"
            ))
        })?);
    }
    let records = record_rules(file.records, origin)?;
    let max_values = match file.maps.and_then(|m| m.max_values) {
        None => None,
        Some(n) if n >= 1 => Some(usize::try_from(n).unwrap_or(usize::MAX)),
        Some(n) => {
            return Err(usage(format!(
                "{origin}: maps.max_values is {n}; give a whole number of 1 or more"
            )));
        }
    };
    let record_views = match file.views.and_then(|v| v.records).as_deref() {
        None | Some("error") => false,
        Some("allow") => true,
        Some(v) => {
            return Err(usage(format!(
                "{origin}: views.records is {v:?}; give \"error\" or \"allow\""
            )));
        }
    };
    Ok(Manifest {
        name: file.name,
        title: file.title,
        steps,
        tests,
        ignore,
        records,
        max_values,
        record_views,
    })
}

/// `[records] null` is `error` (the default) or `absent`; `unknown` is
/// `error` (the default) or `ignore`.
fn record_rules(file: Option<RecordsFile>, origin: &str) -> Result<RecordRules> {
    let Some(file) = file else {
        return Ok(RecordRules::default());
    };
    let choose = |key: &str, value: Option<String>, on: &str| -> Result<bool> {
        match value.as_deref() {
            None | Some("error") => Ok(false),
            Some(v) if v == on => Ok(true),
            Some(v) => Err(usage(format!(
                "{origin}: records.{key} is {v:?}; give \"error\" or \"{on}\""
            ))),
        }
    };
    Ok(RecordRules {
        null_absent: choose("null", file.null, "absent")?,
        ignore_unknown: choose("unknown", file.unknown, "ignore")?,
    })
}

/// `ResourceType.path`: a resource type, then one or more element names
/// separated by dots. The path names object keys; an array on the way is
/// gone through, never indexed.
fn ignore_path(p: &str) -> std::result::Result<IgnorePath, String> {
    if p.contains(['[', ']']) {
        return Err("arrays are not indexed: a path applies to every item of an array".into());
    }
    let mut parts = p.split('.');
    let t = parts.next().unwrap_or_default();
    if !crate::fhir::valid_type(t) {
        return Err(format!("`{t}` is not a resource type"));
    }
    let path: Vec<String> = parts.map(str::to_string).collect();
    if path.is_empty() {
        return Err("names no element".into());
    }
    for name in &path {
        let mut chars = name.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("`{name}` is not an element name"));
        }
    }
    if path.len() == 1 && (path[0] == "id" || path[0] == "resourceType") {
        return Err(format!(
            "`{}` identifies the resource, so it is always compared",
            path[0]
        ));
    }
    Ok(IgnorePath {
        resource_type: t.to_string(),
        path,
    })
}

fn step(s: StepFile, at: &str) -> Result<Step> {
    match (s.map, s.library) {
        (Some(_), Some(_)) => Err(usage(format!(
            "{at}: has both map and library; a step runs a map or a Library"
        ))),
        (None, None) => Err(usage(format!("{at}: needs map (with from) or library"))),
        (None, Some(library)) => {
            if s.from.is_some() || s.stage.is_some() {
                return Err(usage(format!(
                    "{at}: a library step takes only library, not from or stage"
                )));
            }
            Ok(Step::Library { library })
        }
        (Some(map), None) => {
            let from = match s.from {
                None => return Err(usage(format!("{at}: map {map} needs from"))),
                Some(v) => source(v, at)?,
            };
            Ok(Step::Map {
                map,
                from,
                stage: s.stage.unwrap_or(false),
            })
        }
    }
}

/// `from`: a record type, or `{ library = "..." }`.
fn source(v: toml::Value, at: &str) -> Result<Source> {
    let wrong = || {
        usage(format!(
            "{at}: from must be a record type (from = \"DIGITTask\") or a Library result \
             (from = {{ library = \"DigitVisit\" }})"
        ))
    };
    match v {
        toml::Value::String(t) if !t.is_empty() => Ok(Source::Records(t)),
        toml::Value::Table(t) => {
            if let Some(key) = t.keys().find(|k| *k != "library") {
                return Err(usage(format!(
                    "{at}: from has an unknown key `{key}`; it takes only library"
                )));
            }
            match t.get("library") {
                Some(toml::Value::String(l)) if !l.is_empty() => Ok(Source::Library(l.clone())),
                _ => Err(wrong()),
            }
        }
        _ => Err(wrong()),
    }
}

fn test(t: TestFile, at: &str) -> Result<DataTest> {
    let check = match (t.sql, t.library) {
        (Some(_), Some(_)) => {
            return Err(usage(format!(
                "{at}: has both sql and library; a sql test names its tables in the SQL"
            )));
        }
        (Some(sql), None) => {
            if t.unique.is_some() || t.not_null.is_some() {
                return Err(usage(format!(
                    "{at}: unique and not_null need library, not sql"
                )));
            }
            if sql.trim().is_empty() {
                return Err(usage(format!("{at}: sql is empty")));
            }
            if crate::library::compose::has_several_statements(&sql) {
                return Err(usage(format!(
                    "{at}: sql has more than one statement; a test is one query"
                )));
            }
            Check::Sql(sql)
        }
        (None, Some(library)) => {
            let unique = t.unique.unwrap_or_default();
            let not_null = t.not_null.unwrap_or_default();
            if unique.is_empty() && not_null.is_empty() {
                return Err(usage(format!(
                    "{at}: library {library} needs unique or not_null columns to check"
                )));
            }
            Check::Columns {
                library,
                unique,
                not_null,
            }
        }
        (None, None) => {
            return Err(usage(format!(
                "{at}: needs library (with unique or not_null) or sql"
            )));
        }
    };
    Ok(DataTest {
        name: t.name,
        check,
    })
}

/// Views read only what is staged before them, so every stage step comes
/// before the first library step.
fn check_stage_order(steps: &[Step], origin: &str) -> Result<()> {
    let first_library = steps.iter().position(|s| matches!(s, Step::Library { .. }));
    let Some(first) = first_library else {
        return Ok(());
    };
    for (i, s) in steps.iter().enumerate().skip(first + 1) {
        if let Step::Map { stage: true, .. } = s {
            return Err(usage(format!(
                "{origin}: step {} stages resources after the library step {}; every \
                 stage step must come before the first library step, as the views read \
                 only what is staged before it",
                i + 1,
                first + 1
            )));
        }
    }
    Ok(())
}

impl DataTest {
    /// The name in messages: the one given, or one made from the check.
    pub fn label(&self, index: usize) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.check {
            Check::Columns { library, .. } => format!("test {} ({library})", index + 1),
            Check::Sql(_) => format!("test {} (sql)", index + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> String {
        match parse(text, "import.toml") {
            Err(PipesError::Usage(m)) => m,
            Err(e) => panic!("expected a usage error, got {e}"),
            Ok(m) => panic!("expected an error, got {m:?}"),
        }
    }

    #[test]
    fn a_full_manifest_parses() {
        let m = parse(
            r#"
name = "icr-digit"
title = "DIGIT to the ICR"

[[step]]
map = "DIGITFacilityToICR"
from = "DIGITFacility"

[[step]]
map = "StageDIGITTask"
from = "DIGITTask"
stage = true

[[step]]
library = "DigitVisit"

[[step]]
map = "DIGITVisitToICR"
from = { library = "DigitVisit" }

[[test]]
library = "DigitVisit"
unique = ["visitKey"]
not_null = ["householdId"]

[[test]]
name = "no task is dropped"
sql = "SELECT id FROM stg_task EXCEPT SELECT id FROM t;"
"#,
            "import.toml",
        )
        .unwrap();
        assert_eq!(m.name, "icr-digit");
        assert_eq!(m.title.as_deref(), Some("DIGIT to the ICR"));
        assert_eq!(
            m.steps,
            vec![
                Step::Map {
                    map: "DIGITFacilityToICR".into(),
                    from: Source::Records("DIGITFacility".into()),
                    stage: false
                },
                Step::Map {
                    map: "StageDIGITTask".into(),
                    from: Source::Records("DIGITTask".into()),
                    stage: true
                },
                Step::Library {
                    library: "DigitVisit".into()
                },
                Step::Map {
                    map: "DIGITVisitToICR".into(),
                    from: Source::Library("DigitVisit".into()),
                    stage: false
                },
            ]
        );
        assert_eq!(
            m.tests[0].check,
            Check::Columns {
                library: "DigitVisit".into(),
                unique: vec!["visitKey".into()],
                not_null: vec!["householdId".into()]
            }
        );
        assert_eq!(m.tests[0].label(0), "test 1 (DigitVisit)");
        assert_eq!(m.tests[1].label(1), "no task is dropped");
    }

    #[test]
    fn unknown_keys_are_refused_anywhere() {
        let m = message("name = \"x\"\nfhir = \"4.0.1\"\n");
        assert!(m.contains("unknown field `fhir`"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nmap = \"M\"\nfrom = \"T\"\nstaged = true\n");
        assert!(m.contains("unknown field `staged`"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nmap = \"M\"\nfrom = { library = \"L\", x = 1 }\n");
        assert!(m.contains("unknown key `x`"), "{m}");
        let m = message(
            "name = \"x\"\n[[step]]\nlibrary = \"L\"\n[[test]]\nlibrary = \"L\"\nuniq = [\"a\"]\n",
        );
        assert!(m.contains("unknown field `uniq`"), "{m}");
    }

    #[test]
    fn compare_ignore_paths_are_checked() {
        let m = parse(
            "name = \"x\"\n[compare]\nignore = [\"MeasureReport.date\", \"Observation.meta.tag\", \"Patient._birthDate\"]\n",
            "import.toml",
        )
        .unwrap();
        assert!(m.steps.is_empty());
        assert_eq!(
            m.ignore,
            vec![
                IgnorePath {
                    resource_type: "MeasureReport".into(),
                    path: vec!["date".into()]
                },
                IgnorePath {
                    resource_type: "Observation".into(),
                    path: vec!["meta".into(), "tag".into()]
                },
                IgnorePath {
                    resource_type: "Patient".into(),
                    path: vec!["_birthDate".into()]
                },
            ]
        );
        assert!(
            parse("name = \"x\"\n[compare]\n", "import.toml")
                .unwrap()
                .ignore
                .is_empty()
        );
        let ignore = |p: &str| message(&format!("name = \"x\"\n[compare]\nignore = [{p:?}]\n"));
        let m = ignore("MeasureReport.group[0].measureScore");
        assert!(
            m.contains("compare.ignore") && m.contains("arrays are not indexed"),
            "{m}"
        );
        assert!(ignore("measureReport.date").contains("`measureReport` is not a resource type"));
        assert!(ignore("MeasureReport").contains("names no element"));
        assert!(ignore("MeasureReport.").contains("`` is not an element name"));
        assert!(ignore("MeasureReport..date").contains("`` is not an element name"));
        assert!(ignore("MeasureReport.da te").contains("`da te` is not an element name"));
        assert!(ignore("MeasureReport.*").contains("is not an element name"));
        assert!(ignore("MeasureReport.id").contains("identifies the resource"));
        assert!(ignore("Patient.resourceType").contains("identifies the resource"));
        // Unknown keys are refused, as elsewhere.
        let m = message("name = \"x\"\n[compare]\nignored = [\"MeasureReport.date\"]\n");
        assert!(m.contains("unknown field `ignored`"), "{m}");
        let m = message("name = \"x\"\n[compare]\nignore = \"MeasureReport.date\"\n");
        assert!(m.contains("invalid type"), "{m}");
    }

    #[test]
    fn records_and_maps_settings_are_read_and_checked() {
        let m = parse("name = \"x\"\n", "import.toml").unwrap();
        assert_eq!(m.records, RecordRules::default());
        assert_eq!(m.max_values, None);
        let m = parse(
            "name = \"x\"\n[records]\nnull = \"absent\"\nunknown = \"ignore\"\n[maps]\nmax_values = 10_000_000\n",
            "import.toml",
        )
        .unwrap();
        assert_eq!(
            m.records,
            RecordRules {
                null_absent: true,
                ignore_unknown: true
            }
        );
        assert_eq!(m.max_values, Some(10_000_000));
        let m = parse(
            "name = \"x\"\n[records]\nnull = \"error\"\nunknown = \"error\"\n",
            "import.toml",
        )
        .unwrap();
        assert_eq!(m.records, RecordRules::default());
        let m = parse(
            "name = \"x\"\n[records]\nunknown = \"ignore\"\n",
            "import.toml",
        )
        .unwrap();
        assert!(m.records.ignore_unknown && !m.records.null_absent);

        let m = message("name = \"x\"\n[records]\nnull = \"skip\"\n");
        assert!(
            m.contains("records.null is \"skip\"; give \"error\" or \"absent\""),
            "{m}"
        );
        let m = message("name = \"x\"\n[records]\nunknown = \"absent\"\n");
        assert!(
            m.contains("records.unknown is \"absent\"; give \"error\" or \"ignore\""),
            "{m}"
        );
        let m = message("name = \"x\"\n[records]\nnulls = \"absent\"\n");
        assert!(m.contains("unknown field `nulls`"), "{m}");
        let m = message("name = \"x\"\n[maps]\nmax_values = 0\n");
        assert!(m.contains("maps.max_values is 0"), "{m}");
        let m = message("name = \"x\"\n[maps]\nmax_rules = 5\n");
        assert!(m.contains("unknown field `max_rules`"), "{m}");
    }

    #[test]
    fn views_may_be_allowed_to_read_records() {
        assert!(!parse("name = \"x\"\n", "import.toml").unwrap().record_views);
        let m = parse(
            "name = \"x\"\n[views]\nrecords = \"allow\"\n",
            "import.toml",
        )
        .unwrap();
        assert!(m.record_views);
        let m = parse(
            "name = \"x\"\n[views]\nrecords = \"error\"\n",
            "import.toml",
        )
        .unwrap();
        assert!(!m.record_views);
        assert!(
            !parse("name = \"x\"\n[views]\n", "import.toml")
                .unwrap()
                .record_views
        );
        let m = message("name = \"x\"\n[views]\nrecords = \"yes\"\n");
        assert!(
            m.contains("views.records is \"yes\"; give \"error\" or \"allow\""),
            "{m}"
        );
        let m = message("name = \"x\"\n[views]\nrecord = \"allow\"\n");
        assert!(m.contains("unknown field `record`"), "{m}");
        let m = message("name = \"x\"\n[views]\nrecords = true\n");
        assert!(m.contains("invalid type"), "{m}");
    }

    #[test]
    fn a_name_is_required() {
        assert!(message("title = \"x\"\n").contains("missing field `name`"));
        assert!(message("name = \" \"\n").contains("name is empty"));
    }

    #[test]
    fn a_step_is_a_map_with_from_or_a_library() {
        let m = message("name = \"x\"\n[[step]]\nmap = \"M\"\nlibrary = \"L\"\n");
        assert!(m.contains("step 1: has both map and library"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nmap = \"M\"\n");
        assert!(m.contains("map M needs from"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nstage = true\n");
        assert!(m.contains("needs map"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nlibrary = \"L\"\nstage = true\n");
        assert!(m.contains("takes only library"), "{m}");
        let m = message("name = \"x\"\n[[step]]\nmap = \"M\"\nfrom = 3\n");
        assert!(m.contains("from must be a record type"), "{m}");
    }

    #[test]
    fn a_stage_step_after_a_library_step_is_refused() {
        let m = message(
            "name = \"x\"\n[[step]]\nlibrary = \"L\"\n[[step]]\nmap = \"M\"\nfrom = \"T\"\nstage = true\n",
        );
        assert!(
            m.contains("step 2 stages resources after the library step 1"),
            "{m}"
        );
        // An unstaged step may come anywhere.
        parse(
            "name = \"x\"\n[[step]]\nlibrary = \"L\"\n[[step]]\nmap = \"M\"\nfrom = \"T\"\n",
            "import.toml",
        )
        .unwrap();
    }

    #[test]
    fn a_test_is_columns_of_a_library_or_one_query() {
        let lib = "name = \"x\"\n[[step]]\nlibrary = \"L\"\n";
        let m = message(&format!("{lib}[[test]]\nlibrary = \"L\"\n"));
        assert!(m.contains("needs unique or not_null"), "{m}");
        let m = message(&format!(
            "{lib}[[test]]\nsql = \"SELECT 1\"\nunique = [\"a\"]\n"
        ));
        assert!(m.contains("need library, not sql"), "{m}");
        let m = message(&format!(
            "{lib}[[test]]\nsql = \"SELECT 1\"\nlibrary = \"L\"\n"
        ));
        assert!(m.contains("both sql and library"), "{m}");
        let m = message(&format!("{lib}[[test]]\nsql = \"SELECT 1; SELECT 2\"\n"));
        assert!(m.contains("more than one statement"), "{m}");
        let m = message(&format!("{lib}[[test]]\nname = \"n\"\n"));
        assert!(m.contains("needs library"), "{m}");
        let m = message("name = \"x\"\n[[test]]\nsql = \"SELECT 1\"\n");
        assert!(m.contains("no step runs a Library"), "{m}");
    }
}
