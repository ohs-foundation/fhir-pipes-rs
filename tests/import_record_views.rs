//! Binary-level tests for record views: ViewDefinitions in an import
//! package whose `resource` is one of the package's record types, so they
//! read the record files directly (`[views] records = "allow"`). The
//! packages are written here. The tests that run a library step need the
//! duckdb CLI and are skipped without it, as in tests/import_package.rs.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::prelude::*;
use serde_json::{Value, json};

fn have_duckdb(skipped: &str) -> bool {
    let found = Command::new("duckdb")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        let _ = writeln!(std::io::stderr(), "skipped without duckdb: {skipped}");
    }
    found
}

fn import(args: &[&str]) -> Output {
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .arg("import")
        .args(args)
        .output()
        .unwrap()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{} has no {from:?}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

/// A logical model with a snapshot. Each element is `(path below the root,
/// min, max, type)`; a type starting with `#` is a `contentReference`.
fn model(name: &str, elements: &[(&str, u64, &str, &str)]) -> String {
    let mut snapshot = vec![json!({"id": name, "path": name, "min": 0, "max": "*"})];
    for (path, min, max, ty) in elements {
        let path = format!("{name}.{path}");
        let mut e = json!({"id": path, "path": path, "min": min, "max": max});
        match ty.strip_prefix('#') {
            Some(r) => e["contentReference"] = json!(format!("#{name}.{r}")),
            None => e["type"] = json!([{"code": ty}]),
        }
        snapshot.push(e);
    }
    let url = format!("https://example.org/StructureDefinition/{name}");
    json!({"resourceType": "StructureDefinition", "id": name, "url": url, "name": name,
        "status": "draft", "kind": "logical", "abstract": false, "type": url,
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Element",
        "derivation": "specialization", "snapshot": {"element": snapshot}})
    .to_string()
}

const MANIFEST: &str = r#"name = "shops"

[records]
null = "absent"
unknown = "ignore"

[views]
records = "allow"

[[step]]
map = "StageVisit"
from = "Visit"
stage = true

[[step]]
library = "ShopTotal"

[[step]]
map = "ShopTotalToOrganization"
from = { library = "ShopTotal" }

[[test]]
name = "one row per shop"
library = "ShopTotal"
unique = ["shopId"]
"#;

/// A package of three record types: shops and sales read by record views,
/// and visits staged as Basic resources for a view over a resource type.
/// The ShopTotal Library joins the three views, and a map writes an
/// Organization per row.
fn shops() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    write(&pkg.join("import.toml"), MANIFEST);
    write(
        &pkg.join("models/Shop.json"),
        &model(
            "Shop",
            &[
                ("id", 1, "1", "string"),
                ("name", 0, "1", "string"),
                ("region", 0, "1", "BackboneElement"),
                ("region.code", 0, "1", "string"),
            ],
        ),
    );
    write(
        &pkg.join("models/Sale.json"),
        &model(
            "Sale",
            &[("shop", 1, "1", "string"), ("amount", 1, "1", "integer")],
        ),
    );
    write(
        &pkg.join("models/Visit.json"),
        &model("Visit", &[("shop", 1, "1", "string")]),
    );
    write(
        &pkg.join("models/ShopTotal.json"),
        &model(
            "ShopTotal",
            &[
                ("shopId", 1, "1", "string"),
                ("name", 0, "1", "string"),
                ("line", 1, "1", "integer"),
                ("total", 1, "1", "integer"),
                ("visits", 1, "1", "integer"),
                ("region", 0, "1", "string"),
            ],
        ),
    );
    write(
        &pkg.join("maps/StageVisit.map"),
        r#"/// url = 'https://example.org/StructureMap/StageVisit'
/// name = 'StageVisit'
uses "https://example.org/StructureDefinition/Visit" alias Visit as source
uses "http://hl7.org/fhir/StructureDefinition/Basic" alias Basic as target

group Visit(source src : Visit, target b : Basic) {
  src.shop as s -> b.code as c, c.text = s "shop";
}
"#,
    );
    write(
        &pkg.join("maps/ShopTotalToOrganization.map"),
        r#"/// url = 'https://example.org/StructureMap/ShopTotalToOrganization'
/// name = 'ShopTotalToOrganization'
uses "https://example.org/StructureDefinition/ShopTotal" alias ShopTotal as source
uses "http://hl7.org/fhir/StructureDefinition/Organization" alias Organization as target

group Shop(source src : ShopTotal, target org : Organization) {
  src.shopId as i -> org.id = i "id";
  src.name as n -> org.name = n "name";
  src.line as v -> org.alias = evaluate(v, 'line ' + $this.toString()) "line";
  src.total as v -> org.alias = evaluate(v, 'total ' + $this.toString()) "total";
  src.visits as v -> org.alias = evaluate(v, 'visits ' + $this.toString()) "visits";
  src.region as v -> org.alias = evaluate(v, 'region ' + $this) "region";
}
"#,
    );
    write(
        &pkg.join("views/ShopRecord.json"),
        r#"{"resourceType": "ViewDefinition", "url": "https://example.org/ViewDefinition/ShopRecord",
 "name": "ShopRecord", "status": "draft", "resource": "Shop",
 "select": [{"column": [
   {"name": "line", "path": "%recordLine", "type": "integer"},
   {"name": "id", "path": "id", "type": "string"},
   {"name": "name", "path": "name", "type": "string"},
   {"name": "region", "path": "region", "collection": true}]}]}
"#,
    );
    write(
        &pkg.join("views/SaleRecord.json"),
        r#"{"resourceType": "ViewDefinition", "url": "https://example.org/ViewDefinition/SaleRecord",
 "name": "SaleRecord", "status": "draft", "resource": "Sale",
 "select": [{"column": [
   {"name": "shop", "path": "shop", "type": "string"},
   {"name": "amount", "path": "amount", "type": "integer"}]}]}
"#,
    );
    write(
        &pkg.join("views/StgVisit.json"),
        r#"{"resourceType": "ViewDefinition", "url": "https://example.org/ViewDefinition/StgVisit",
 "name": "StgVisit", "status": "draft", "resource": "Basic",
 "select": [{"column": [{"name": "shop", "path": "code.text", "type": "string"}]}]}
"#,
    );
    write(
        &pkg.join("libraries/ShopTotal.sql"),
        r#"SELECT s.id AS "shopId", s.name AS "name", s.line AS "line",
       CAST(sum(a.amount) AS INTEGER) AS "total",
       CAST((SELECT count(*) FROM visit AS v WHERE v.shop = s.id) AS INTEGER) AS "visits",
       CAST(s.region AS VARCHAR) AS "region"
FROM shop AS s
JOIN sale AS a ON a.shop = s.id
GROUP BY s.id, s.name, s.line, CAST(s.region AS VARCHAR)
ORDER BY s.line
"#,
    );
    write(
        &pkg.join("libraries/ShopTotal.toml"),
        r#"url = "https://example.org/Library/ShopTotal"
name = "ShopTotal"
status = "draft"
kind = "SQLQuery"

[[depends_on]]
label = "shop"
resource = "https://example.org/ViewDefinition/ShopRecord"

[[depends_on]]
label = "sale"
resource = "https://example.org/ViewDefinition/SaleRecord"

[[depends_on]]
label = "visit"
resource = "https://example.org/ViewDefinition/StgVisit"
"#,
    );
    (tmp, pkg)
}

/// The records: a blank line in the shops, so a record's line is not its
/// position; nulls and members no model describes.
fn records(dir: &Path) {
    write(
        &dir.join("Shop.ndjson"),
        concat!(
            r#"{"id":"s-2","name":"Second","region":{"code":"N","extra":1},"audit":{"by":"u"}}"#,
            "\n\n",
            r#"{"id":"s-1","name":null,"region":null}"#,
            "\n",
            r#"{"id":"s-3","name":"Third","rowVersion":2}"#,
            "\n"
        ),
    );
    write(
        &dir.join("Sale.ndjson"),
        concat!(
            r#"{"shop":"s-1","amount":5}"#,
            "\n",
            r#"{"shop":"s-2","amount":7}"#,
            "\n",
            r#"{"shop":"s-2","amount":3,"till":null}"#,
            "\n",
            r#"{"shop":"s-3","amount":1}"#,
            "\n"
        ),
    );
    write(
        &dir.join("Visit.ndjson"),
        "{\"shop\":\"s-2\"}\n{\"shop\":\"s-2\"}\n{\"shop\":\"s-1\"}\n",
    );
}

fn run(pkg: &Path, input: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "run",
        "--package",
        s(pkg),
        "--in",
        s(input),
        "--out",
        s(out),
    ];
    args.extend_from_slice(extra);
    import(&args)
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_library_joins_record_views_and_a_staged_view_and_a_map_reads_the_result() {
    if !have_duckdb("a_library_joins_record_views_and_a_staged_view") {
        return;
    }
    let (tmp, pkg) = shops();
    let input = tmp.path().join("in");
    records(&input);
    let out = tmp.path().join("out");
    let report = tmp.path().join("report.json");
    let o = run(&pkg, &input, &out, &["--report", s(&report)]);
    let (stdout, err) = text(&o);
    assert!(o.status.success(), "{stdout}\n{err}");
    // In the order of the shops' file, each with its line; the nulls read as
    // absent (s-1 has no name or region) and the members no model describes
    // left out of what the views see (s-2's region has no `extra`).
    assert_eq!(
        lines(&out.join("Organization.ndjson")),
        [
            json!({"resourceType": "Organization", "id": "s-2", "name": "Second",
                "alias": ["line 1", "total 10", "visits 2", "region [{\"code\":\"N\"}]"]}),
            json!({"resourceType": "Organization", "id": "s-1",
                "alias": ["line 3", "total 5", "visits 1", "region []"]}),
            json!({"resourceType": "Organization", "id": "s-3", "name": "Third",
                "alias": ["line 4", "total 1", "visits 0", "region []"]}),
        ]
    );
    let report: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    let tables = &report["steps"][1]["tables"];
    let table = |name: &str| {
        tables
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["table"] == name)
            .unwrap_or_else(|| panic!("no table {name} in {tables}"))
            .clone()
    };
    assert_eq!(table("shop_record")["rows"], 3);
    assert_eq!(table("shop_record")["from"], "view ShopRecord");
    assert_eq!(
        table("shop_record")["file"],
        input.join("Shop.ndjson").display().to_string()
    );
    assert_eq!(table("sale_record")["rows"], 4);
    // A view over a resource type reads the staged resources, as before.
    assert_eq!(table("stg_visit")["rows"], 3);
    assert!(table("stg_visit").get("file").is_none(), "{tables}");
}

#[test]
fn record_views_write_the_same_bytes_at_any_thread_count() {
    if !have_duckdb("record_views_write_the_same_bytes_at_any_thread_count") {
        return;
    }
    let (tmp, pkg) = shops();
    let input = tmp.path().join("in");
    records(&input);
    // Enough shops and sales that the views run on several threads.
    let mut shops = fs::File::create(input.join("Shop.ndjson")).unwrap();
    let mut sales = fs::File::create(input.join("Sale.ndjson")).unwrap();
    for i in (0..6000).rev() {
        writeln!(
            shops,
            r#"{{"id":"s-{i}","name":"Shop {i}","region":{{"code":"R{}"}}}}"#,
            i % 7
        )
        .unwrap();
        for n in 0..(i % 3 + 1) {
            writeln!(sales, r#"{{"shop":"s-{i}","amount":{n}}}"#).unwrap();
        }
    }
    drop((shops, sales));
    let mut outputs = Vec::new();
    for threads in ["1", "8"] {
        let out = tmp.path().join(format!("out-{threads}"));
        let o = run(&pkg, &input, &out, &["--threads", threads]);
        assert!(o.status.success(), "{:?}", text(&o));
        outputs.push(fs::read(out.join("Organization.ndjson")).unwrap());
    }
    assert_eq!(outputs[0], outputs[1]);
    let first = lines(&tmp.path().join("out-1/Organization.ndjson"));
    assert_eq!(first.len(), 6000);
    // The shops' file is in descending order, and so is the output.
    assert_eq!(first[0]["id"], "s-5999");
    assert_eq!(first[0]["alias"][0], "line 1");
    assert_eq!(first[5999]["id"], "s-0");
}

#[test]
fn a_missing_record_file_gives_an_empty_table() {
    if !have_duckdb("a_missing_record_file_gives_an_empty_table") {
        return;
    }
    let (tmp, pkg) = shops();
    let input = tmp.path().join("in");
    records(&input);
    fs::remove_file(input.join("Sale.ndjson")).unwrap();
    let out = tmp.path().join("out");
    let report = tmp.path().join("report.json");
    let o = run(&pkg, &input, &out, &["--report", s(&report)]);
    let (stdout, err) = text(&o);
    assert!(o.status.success(), "{stdout}\n{err}");
    assert!(
        stdout.contains("no Sale.ndjson in the input folder"),
        "{stdout}"
    );
    assert!(!out.join("Organization.ndjson").exists());
    let report: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    let sale = report["steps"][1]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"] == "sale_record")
        .unwrap()
        .clone();
    assert_eq!(sale["rows"], 0);
    assert_eq!(sale["file"], Value::Null);
}

#[test]
fn records_rules_apply_to_what_a_record_view_reads() {
    if !have_duckdb("records_rules_apply_to_what_a_record_view_reads") {
        return;
    }
    let (tmp, pkg) = shops();
    let input = tmp.path().join("in");
    records(&input);
    let shops = input.join("Shop.ndjson").display().to_string();
    let sales = input.join("Sale.ndjson").display().to_string();
    // Without `[records]`, the view refuses what the model does not
    // describe, as a map does, naming the file and line.
    edit(
        &pkg.join("import.toml"),
        "[records]\nnull = \"absent\"\nunknown = \"ignore\"\n",
        "",
    );
    let o = run(&pkg, &input, &tmp.path().join("out1"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!(
            "{sales}:3: unknown element `till`; `fhir-pipes import check"
        )),
        "{err}"
    );
    // Unknown members ignored, nulls not: the null name of line 3 is refused.
    edit(
        &pkg.join("import.toml"),
        "[views]",
        "[records]\nunknown = \"ignore\"\n\n[views]",
    );
    let o = run(&pkg, &input, &tmp.path().join("out2"), &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(err.contains(&format!("{shops}:3: `name` is null")), "{err}");
}

#[test]
fn a_record_view_needs_the_manifest_to_allow_it() {
    let (_tmp, pkg) = shops();
    edit(
        &pkg.join("import.toml"),
        "[views]\nrecords = \"allow\"\n",
        "",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains(
            "view SaleRecord reads record type Sale: a view over a record type reads the \
             record file directly, which goes beyond the SQL on FHIR specification. Set \
             `records = \"allow\"` under [views] in import.toml to allow it"
        ),
        "{err}"
    );
    edit(
        &pkg.join("import.toml"),
        "[records]",
        "[views]\nrecords = \"allwo\"\n\n[records]",
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("views.records is \"allwo\"; give \"error\" or \"allow\""),
        "{err}"
    );
}

#[test]
fn import_check_lists_the_record_views() {
    let (tmp, pkg) = shops();
    let input = tmp.path().join("in");
    records(&input);
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains(
            "record views, which read record files directly, beyond the SQL on FHIR \
             specification: SaleRecord (Sale), ShopRecord (Shop)\n"
        ),
        "{out}"
    );
    assert!(out.contains("10 records in 3 files: 0 problems"), "{out}");
}

#[test]
fn a_misspelt_path_in_a_record_view_fails_import_check() {
    let (_tmp, pkg) = shops();
    let view = pkg.join("views/ShopRecord.json");
    edit(&view, r#""path": "name""#, r#""path": "nmae""#);
    edit(
        &view,
        r#"{"name": "region", "path": "region", "collection": true}"#,
        r#"{"name": "region", "path": "region.cdoe"},
   {"name": "first", "path": "region.where(code = 'N').first().code"},
   {"name": "any", "path": "region.code.exists()"},
   {"name": "computed", "path": "iif(nmae.exists(), 1, 2)"}"#,
    );
    edit(
        &view,
        r#""select": ["#,
        r#""where": [{"path": "regoin.exists()"}], "select": ["#,
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    let url = "https://example.org/StructureDefinition/Shop";
    for want in [
        format!(
            "error: view ShopRecord (Shop, {url}): column `name`, path `nmae`: unknown element \
             `nmae` — did you mean `name`?"
        ),
        format!(
            "error: view ShopRecord (Shop, {url}): column `region`, path `region.cdoe`: \
             unknown element `cdoe` — did you mean `code`?"
        ),
        format!(
            "error: view ShopRecord (Shop, {url}): path `regoin.exists()`: unknown element \
             `regoin`"
        ),
    ] {
        assert!(out.contains(&want), "{want}\n{out}");
    }
    // Paths that are right are not reported, nor one it cannot follow (a
    // function's argument).
    assert_eq!(out.matches("error: view").count(), 3, "{out}");
    assert!(out.contains("3 errors"), "{out}");
}

#[test]
fn get_resource_key_is_refused_in_a_record_view() {
    let (_tmp, pkg) = shops();
    edit(
        &pkg.join("views/SaleRecord.json"),
        r#""path": "shop""#,
        r#""path": "getResourceKey()""#,
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(
            "view SaleRecord reads record type Sale, and its path `getResourceKey()` calls \
             getResourceKey(): a record is not a resource and has no key of its own"
        ),
        "{err}"
    );
    // getReferenceKey() reads a Reference value, and is allowed.
    edit(
        &pkg.join("views/SaleRecord.json"),
        r#""path": "getResourceKey()""#,
        r#""path": "shop.getReferenceKey()""#,
    );
    let o = import(&["check", "--package", s(&pkg)]);
    assert!(o.status.success(), "{:?}", text(&o));
}

#[test]
fn a_record_line_is_only_for_record_views() {
    let (_tmp, pkg) = shops();
    edit(
        &pkg.join("views/StgVisit.json"),
        r#""path": "code.text""#,
        r#""path": "%recordLine""#,
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(err.contains("StgVisit"), "{err}");
    assert!(err.contains("recordLine"), "{err}");
}

#[test]
fn a_model_named_like_a_fhir_type_is_refused() {
    let (_tmp, pkg) = shops();
    write(
        &pkg.join("models/Location.json"),
        &model("Location", &[("code", 1, "1", "string")]),
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains(
            "https://example.org/StructureDefinition/Location is record type Location, which \
             is also a FHIR resource type"
        ),
        "{err}"
    );
}

#[test]
fn a_record_view_flattens_a_tree_of_any_depth() {
    if !have_duckdb("a_record_view_flattens_a_tree_of_any_depth") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    write(
        &pkg.join("import.toml"),
        r#"name = "trees"

[views]
records = "allow"

[[step]]
library = "NodeList"

[[test]]
name = "every node, each parent before its children"
sql = """
SELECT * FROM (SELECT string_agg(code, ' ' ORDER BY n) AS codes FROM node_list)
WHERE codes IS DISTINCT FROM 'a a1 a11 a111 a12 a2 b b1'
"""
"#,
    );
    write(
        &pkg.join("models/Tree.json"),
        &model(
            "Tree",
            &[
                ("name", 1, "1", "string"),
                ("node", 0, "*", "BackboneElement"),
                ("node.code", 1, "1", "string"),
                ("node.children", 0, "*", "#node"),
            ],
        ),
    );
    fs::create_dir_all(pkg.join("maps")).unwrap();
    write(
        &pkg.join("views/TreeNode.json"),
        r#"{"resourceType": "ViewDefinition", "url": "https://example.org/ViewDefinition/TreeNode",
 "name": "TreeNode", "status": "draft", "resource": "Tree",
 "select": [{"column": [{"name": "line", "path": "%recordLine", "type": "integer"}]},
   {"forEach": "node", "unionAll": [
     {"column": [{"name": "code", "path": "code", "type": "string"},
                 {"name": "i", "path": "%rowIndex", "type": "integer"}]},
     {"repeat": ["children"], "column": [{"name": "code", "path": "code", "type": "string"},
                 {"name": "i", "path": "%rowIndex", "type": "integer"}]}]}]}
"#,
    );
    write(
        &pkg.join("libraries/NodeList.sql"),
        "SELECT code, row_number() OVER () AS n FROM node\n",
    );
    write(
        &pkg.join("libraries/NodeList.toml"),
        r#"url = "https://example.org/Library/NodeList"
name = "NodeList"
status = "draft"
kind = "SQLQuery"

[[depends_on]]
label = "node"
resource = "https://example.org/ViewDefinition/TreeNode"
"#,
    );
    let input = tmp.path().join("in");
    let leaf = |c: &str| json!({"code": c, "children": []});
    let trees = [
        json!({"name": "A", "node": [
            {"code": "a", "children": [
                {"code": "a1", "children": [
                    {"code": "a11", "children": [leaf("a111")]}, leaf("a12")]},
                leaf("a2")]}]}),
        json!({"name": "B", "node": [{"code": "b", "children": [{"code": "b1"}]}]}),
    ];
    write(
        &input.join("Tree.ndjson"),
        &(trees.map(|t| t.to_string()).join("\n") + "\n"),
    );
    let o = import(&["check", "--package", s(&pkg), "--in", s(&input)]);
    assert!(o.status.success(), "{:?}", text(&o));
    let o = run(&pkg, &input, &tmp.path().join("out"), &[]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(out.contains("tree_node 8 rows"), "{out}");

    // A misspelt column under the repeat is found by import check: every
    // node it reaches has the elements of a node.
    edit(
        &pkg.join("views/TreeNode.json"),
        r#""path": "code", "type": "string"},
                 {"name": "i", "path": "%rowIndex", "type": "integer"}]}]}]}"#,
        r#""path": "cdoe", "type": "string"},
                 {"name": "i", "path": "%rowIndex", "type": "integer"}]}]}]}"#,
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.contains(
            "view TreeNode (Tree, https://example.org/StructureDefinition/Tree): column `code`, \
             path `cdoe`: unknown element `cdoe`"
        ),
        "{out}"
    );
}

#[test]
fn import_check_warns_of_elements_the_view_engine_can_read_for_another() {
    let (_tmp, pkg) = shops();
    write(
        &pkg.join("models/Shop.json"),
        &model(
            "Shop",
            &[
                ("id", 1, "1", "string"),
                ("name", 0, "1", "string"),
                ("nameLocal", 0, "1", "string"),
                ("region", 0, "1", "BackboneElement"),
                ("region.code", 0, "1", "string"),
            ],
        ),
    );
    let o = import(&["check", "--package", s(&pkg)]);
    let (out, err) = text(&o);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        out.contains(
            "warning: view ShopRecord (Shop): Shop has elements `name` and `nameLocal`: in a \
             record with no `name`, the view engine reads `nameLocal` for a path that reads \
             `name`"
        ),
        "{out}"
    );
}
