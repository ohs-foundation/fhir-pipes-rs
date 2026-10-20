use serde_json::{Value, json};

use super::*;

const CS: &str = "http://example.org/CodeSystem/kinds";

fn content(resources: Vec<Value>) -> Content {
    let mut c = Content::default();
    for r in resources {
        c.add(r);
    }
    c
}

/// A complete code system: `a` above `b` above `c` (by nesting), `d` under
/// `a` by a `parent` property, `x` retired, `group` not selectable.
fn kinds() -> Value {
    json!({
        "resourceType": "CodeSystem", "url": CS, "version": "2", "content": "complete",
        "concept": [
            {"code": "a", "display": "A", "concept": [
                {"code": "b", "display": "B", "concept": [{"code": "c", "display": "C"}]}
            ]},
            {"code": "d", "display": "D", "property": [{"code": "parent", "valueCode": "a"}]},
            {"code": "x", "display": "X", "property": [{"code": "status", "valueCode": "retired"}]},
            {"code": "group", "property": [{"code": "notSelectable", "valueBoolean": true}]}
        ]
    })
}

fn vs(id: &str, compose: Value) -> Value {
    json!({"resourceType": "ValueSet", "url": format!("http://example.org/ValueSet/{id}"),
           "compose": compose})
}

/// (code, version, display, inactive) of a member.
type Row = (String, Option<String>, Option<String>, Option<bool>);

/// Each member, in table order.
fn rows(c: &Content, id: &str) -> Vec<Row> {
    c.value_set_members(&format!("http://example.org/ValueSet/{id}"))
        .unwrap()
        .into_iter()
        .map(|m| (m.code, m.version, m.display, m.inactive))
        .collect()
}

fn codes(c: &Content, id: &str) -> Vec<String> {
    rows(c, id).into_iter().map(|r| r.0).collect()
}

fn error(c: &Content, id: &str) -> String {
    format!(
        "{:#}",
        c.value_set_members(&format!("http://example.org/ValueSet/{id}"))
            .unwrap_err()
    )
}

#[test]
fn an_expansion_gives_one_row_per_member() {
    let c = content(vec![json!({
        "resourceType": "ValueSet", "url": "http://example.org/ValueSet/e",
        "expansion": {"total": 5, "contains": [
            {"abstract": true, "display": "heading", "contains": [
                {"system": CS, "version": "1", "code": "a", "display": "A"},
                {"system": CS, "version": "1", "code": "old", "inactive": true}
            ]},
            {"system": CS, "version": "1", "code": "a", "display": "A again"},
            {"system": CS, "code": "b"}
        ]}
    })]);
    assert_eq!(
        rows(&c, "e"),
        [
            ("b".into(), None, None, None),
            ("a".into(), Some("1".into()), Some("A".into()), None),
            ("old".into(), Some("1".into()), None, Some(true)),
        ]
    );
}

#[test]
fn a_page_of_an_expansion_fails() {
    let paged = |expansion: Value| {
        content(vec![json!({"resourceType": "ValueSet",
            "url": "http://example.org/ValueSet/p", "expansion": expansion})])
    };
    let one = json!([{"system": CS, "code": "a"}]);
    assert!(error(&paged(json!({"offset": 10, "contains": one})), "p").contains("offset"));
    assert!(error(&paged(json!({"total": 2, "contains": one})), "p").contains("total is 2"));
    assert_eq!(
        codes(
            &paged(json!({"offset": 0, "total": 1, "contains": one})),
            "p"
        ),
        ["a"]
    );
}

#[test]
fn listed_codes_take_their_display_and_version_from_the_code_system() {
    let c = content(vec![
        kinds(),
        vs(
            "l",
            json!({"include": [{"system": CS, "concept": [
            {"code": "b"}, {"code": "c", "display": "Mine"}, {"code": "zz"}]}]}),
        ),
    ]);
    assert_eq!(
        rows(&c, "l"),
        [
            ("b".into(), Some("2".into()), Some("B".into()), None),
            ("c".into(), Some("2".into()), Some("Mine".into()), None),
            ("zz".into(), Some("2".into()), None, None),
        ]
    );
}

#[test]
fn listed_codes_need_no_code_system() {
    let c = content(vec![vs(
        "l",
        json!({"include": [{"system": "http://loinc.org", "version": "2.77",
                            "concept": [{"code": "1234-5", "display": "Test"}]}]}),
    )]);
    assert_eq!(
        rows(&c, "l"),
        [(
            "1234-5".into(),
            Some("2.77".into()),
            Some("Test".into()),
            None
        )]
    );
}

#[test]
fn a_whole_code_system_leaves_out_codes_that_are_not_selectable() {
    let c = content(vec![
        kinds(),
        vs("all", json!({"include": [{"system": CS}]})),
    ]);
    assert_eq!(codes(&c, "all"), ["a", "b", "c", "d", "x"]);
    let x = rows(&c, "all").into_iter().find(|r| r.0 == "x").unwrap();
    assert_eq!(x.3, Some(true), "retired is inactive");
}

#[test]
fn hierarchy_filters_follow_nesting_and_parent_properties() {
    let filter = |op: &str, value: &str| {
        vs(
            "f",
            json!({"include": [{"system": CS,
                "filter": [{"property": "concept", "op": op, "value": value}]}]}),
        )
    };
    let run = |op: &str, value: &str| codes(&content(vec![kinds(), filter(op, value)]), "f");
    assert_eq!(run("is-a", "a"), ["a", "b", "c", "d"]);
    assert_eq!(run("descendent-of", "a"), ["b", "c", "d"]);
    assert_eq!(run("is-a", "b"), ["b", "c"]);
    assert_eq!(run("is-not-a", "b"), ["a", "d", "x"]);
    let err = error(&content(vec![kinds(), filter("regex", ".*")]), "f");
    assert!(err.contains("`concept regex .*`"), "{err}");
}

#[test]
fn excludes_and_inactive_codes_are_left_out() {
    let c = content(vec![
        kinds(),
        vs(
            "ex",
            json!({"inactive": false, "include": [{"system": CS}],
                   "exclude": [{"system": CS, "concept": [{"code": "b"}]}]}),
        ),
    ]);
    assert_eq!(codes(&c, "ex"), ["a", "c", "d"]);
}

#[test]
fn included_value_sets_intersect() {
    let url = |id: &str| format!("http://example.org/ValueSet/{id}");
    let c = content(vec![
        kinds(),
        vs(
            "abc",
            json!({"include": [{"system": CS, "concept": [
            {"code": "a"}, {"code": "b"}, {"code": "c"}]}]}),
        ),
        vs(
            "bcd",
            json!({"include": [{"system": CS, "concept": [
            {"code": "b"}, {"code": "c"}, {"code": "d"}]}]}),
        ),
        vs(
            "both",
            json!({"include": [{"valueSet": [url("abc"), url("bcd")]}]}),
        ),
        vs(
            "narrowed",
            json!({"include": [{"system": CS, "filter": [
            {"property": "concept", "op": "is-a", "value": "b"}], "valueSet": [url("bcd")]}]}),
        ),
        vs(
            "union",
            json!({"include": [{"valueSet": [url("abc")]}, {"valueSet": [url("bcd")]}]}),
        ),
        vs("loop", json!({"include": [{"valueSet": [url("loop")]}]})),
    ]);
    assert_eq!(codes(&c, "both"), ["b", "c"]);
    assert_eq!(codes(&c, "narrowed"), ["b", "c"]);
    assert_eq!(codes(&c, "union"), ["a", "b", "c", "d"]);
    assert!(error(&c, "loop").contains("includes itself"));
}

#[test]
fn content_the_packages_do_not_hold_fails() {
    let sct = "http://snomed.info/sct";
    let c = content(vec![
        vs(
            "sct",
            json!({"include": [{"system": sct, "filter": [
            {"property": "concept", "op": "is-a", "value": "404684003"}]}]}),
        ),
        json!({"resourceType": "CodeSystem", "url": "http://example.org/frag",
               "content": "fragment", "concept": [{"code": "a"}]}),
        vs(
            "frag",
            json!({"include": [{"system": "http://example.org/frag"}]}),
        ),
        vs("none", json!({})),
    ]);
    let err = error(&c, "sct");
    assert!(
        err.contains("filters http://snomed.info/sct, which is not in the packages"),
        "{err}"
    );
    assert!(error(&c, "frag").contains("content = fragment"));
    assert_eq!(
        codes(&c, "none"),
        Vec::<String>::new(),
        "an empty compose has no members"
    );
    assert!(error(&c, "missing").contains("no ValueSet"));
}

#[test]
fn several_versions_need_one_named() {
    let v = |version: &str, code: &str| {
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/v",
               "version": version, "compose": {"include": [{"system": CS,
               "concept": [{"code": code}]}]}})
    };
    let c = content(vec![v("1", "a"), v("2", "b"), v("2", "duplicate")]);
    assert!(error(&c, "v").contains("versions 1, 2"));
    let members = c
        .value_set_members("http://example.org/ValueSet/v|2")
        .unwrap();
    assert_eq!(members[0].code, "b", "the first copy of a version is kept");
}

fn map_rows(cm: Value) -> Result<Vec<Vec<Option<String>>>> {
    let c = content(vec![cm]);
    Ok(c.concept_map_rows("http://example.org/ConceptMap/m")?
        .into_iter()
        .map(|r| {
            vec![
                Some(r.source_system),
                r.source_version,
                Some(r.source_code),
                r.target_system,
                r.target_version,
                r.target_code,
                r.relationship,
            ]
        })
        .collect())
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn an_r5_concept_map_gives_a_row_per_target_and_per_no_map() {
    let rows = map_rows(json!({
        "resourceType": "ConceptMap", "url": "http://example.org/ConceptMap/m",
        "group": [{"source": "http://src|1", "target": "http://tgt", "element": [
            {"code": "a", "target": [
                {"code": "A", "relationship": "equivalent"},
                {"code": "A", "relationship": "equivalent"},
                {"code": "B", "relationship": "source-is-broader-than-target"}]},
            {"code": "b", "noMap": true}
        ], "unmapped": {"mode": "use-source-code"}}]
    }))
    .unwrap();
    assert_eq!(
        rows,
        [
            vec![
                s("http://src"),
                s("1"),
                s("a"),
                s("http://tgt"),
                None,
                s("A"),
                s("equivalent")
            ],
            vec![
                s("http://src"),
                s("1"),
                s("a"),
                s("http://tgt"),
                None,
                s("B"),
                s("source-is-broader-than-target")
            ],
            vec![
                s("http://src"),
                s("1"),
                s("b"),
                s("http://tgt"),
                None,
                None,
                None
            ],
        ]
    );
}

#[test]
fn an_r4_concept_map_has_its_equivalence_as_a_relationship() {
    let rows = map_rows(json!({
        "resourceType": "ConceptMap", "url": "http://example.org/ConceptMap/m",
        "group": [{"source": "http://src", "sourceVersion": "4", "target": "http://tgt",
            "element": [
                {"code": "a", "target": [{"code": "A", "equivalence": "wider"}]},
                {"code": "b", "target": [{"code": "B", "equivalence": "specializes"}]},
                {"code": "c", "target": [{"equivalence": "unmatched"}]}
            ]}]
    }))
    .unwrap();
    let rel: Vec<_> = rows.iter().map(|r| (r[2].clone(), r[6].clone())).collect();
    assert_eq!(
        rel,
        [
            (s("a"), s("source-is-narrower-than-target")),
            (s("b"), s("source-is-broader-than-target")),
            (s("c"), None),
        ]
    );
    assert_eq!(rows[0][1], s("4"), "sourceVersion");
}

#[test]
fn a_concept_map_rows_cannot_hold_fails() {
    let with = |group: Value| {
        map_rows(json!({"resourceType": "ConceptMap",
            "url": "http://example.org/ConceptMap/m", "group": [group]}))
        .unwrap_err()
        .to_string()
    };
    assert!(with(json!({"element": []})).contains("no source"));
    assert!(
        with(
            json!({"source": "http://s", "element": [{"code": "a", "target": [
            {"code": "A", "relationship": "equivalent", "dependsOn": [{"attribute": "x"}]}]}]})
        )
        .contains("dependsOn")
    );
    assert!(
        with(
            json!({"source": "http://s", "element": [{"code": "a", "target": [
            {"valueSet": "http://vs", "relationship": "equivalent"}]}]})
        )
        .contains("maps to a value set")
    );
}

#[test]
fn packages_are_read_once_per_url_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let cs = kinds().to_string();
    let v = vs("all", json!({"include": [{"system": CS}]})).to_string();
    for name in ["one.tgz", "two.tgz"] {
        let bytes =
            crate::fhir::package::tgz(&[("CodeSystem-kinds.json", &cs), ("ValueSet-all.json", &v)]);
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }
    let specs: Vec<String> = ["one.tgz", "two.tgz"]
        .iter()
        .map(|n| dir.path().join(n).to_string_lossy().into_owned())
        .collect();
    let c = Content::load(&specs).unwrap();
    assert_eq!(codes(&c, "all"), ["a", "b", "c", "d", "x"]);
}
