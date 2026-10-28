//! Binary-level tests for `fhir-pipes libraries`.

use std::fs;

use assert_cmd::Command;

#[test]
fn build_writes_one_library_per_sql_file() {
    let src = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    for name in ["One", "Two"] {
        fs::write(src.path().join(format!("{name}.sql")), "SELECT 1 AS x\n").unwrap();
        fs::write(
            src.path().join(format!("{name}.toml")),
            format!(
                "url = \"https://example.org/Library/{name}\"\nname = \"{name}\"\n\
                 status = \"draft\"\nkind = \"SQLView\"\n"
            ),
        )
        .unwrap();
    }
    Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["libraries", "build"])
        .arg(src.path())
        .arg("--out")
        .arg(out.path())
        .assert()
        .success();
    for name in ["One", "Two"] {
        let text = fs::read_to_string(out.path().join(format!("Library-{name}.json"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["name"], name);
    }
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 2);
}
