//! Comparing a run's output with a test case's expected files: the same
//! files, and in each the same resources in the same order, compared as JSON
//! values.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// The differing resources reported per file.
const RESOURCES_SHOWN: usize = 10;
/// The differing paths reported per resource.
const PATHS_SHOWN: usize = 8;
/// The longest value shown in a difference.
const VALUE_CHARS: usize = 80;

/// The `*.ndjson` file names in `dir`, which may not exist.
pub fn ndjson_files(dir: &Path) -> Result<BTreeSet<String>> {
    if !dir.is_dir() {
        return Ok(BTreeSet::new());
    }
    Ok(crate::fs::sorted_entries(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .into_iter()
        .filter(|p| p.is_file())
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .filter(|n| n.ends_with(".ndjson") && !n.starts_with('.'))
        .collect())
}

fn read(path: &Path) -> Result<Vec<Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l)
                .with_context(|| format!("{}:{}: not JSON", path.display(), i + 1))
                .map_err(crate::error::usage)
        })
        .collect()
}

/// The differences between the files in `actual` and those in `expected`,
/// as lines to print. None when they are the same.
pub fn compare(actual: &Path, expected: &Path) -> Result<Vec<String>> {
    let (got, want) = (ndjson_files(actual)?, ndjson_files(expected)?);
    let mut out = Vec::new();
    for missing in want.difference(&got) {
        out.push(format!("{missing}: expected, but the run wrote none"));
    }
    for extra in got.difference(&want) {
        let n = read(&actual.join(extra))?.len();
        out.push(format!(
            "{extra}: written ({n} resources), but not expected"
        ));
    }
    for name in got.intersection(&want) {
        let a = read(&actual.join(name))?;
        let e = read(&expected.join(name))?;
        let mut shown = 0;
        let mut differing = 0;
        for i in 0..a.len().max(e.len()) {
            let (got, want) = (a.get(i), e.get(i));
            if got == want {
                continue;
            }
            differing += 1;
            if shown == RESOURCES_SHOWN {
                continue;
            }
            shown += 1;
            let which = want.or(got).map(|r| label(r, i)).unwrap_or_default();
            match (got, want) {
                (Some(g), None) => {
                    out.push(format!("{name} line {}: unexpected {}", i + 1, label(g, i)))
                }
                (None, Some(_)) => out.push(format!(
                    "{name} line {}: expected {which}, but the file ends",
                    i + 1
                )),
                (Some(g), Some(w)) => {
                    out.push(format!("{name} line {} ({which}):", i + 1));
                    let mut paths = Vec::new();
                    diff("", w, g, &mut paths);
                    let more = paths.len().saturating_sub(PATHS_SHOWN);
                    out.extend(
                        paths
                            .into_iter()
                            .take(PATHS_SHOWN)
                            .map(|p| format!("  {p}")),
                    );
                    if more > 0 {
                        out.push(format!("  and {more} more"));
                    }
                }
                (None, None) => {}
            }
        }
        if differing > shown {
            out.push(format!(
                "{name}: {} more resources differ",
                differing - shown
            ));
        }
    }
    Ok(out)
}

/// `Task/x` for a resource with an id, else `line N`.
fn label(r: &Value, i: usize) -> String {
    match (r["resourceType"].as_str(), r["id"].as_str()) {
        (Some(t), Some(id)) => format!("{t}/{id}"),
        _ => format!("line {}", i + 1),
    }
}

fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > VALUE_CHARS {
        let cut: String = s.chars().take(VALUE_CHARS).collect();
        format!("{cut}...")
    } else {
        s
    }
}

/// The paths where `got` differs from `want`, with both values.
fn diff(path: &str, want: &Value, got: &Value, out: &mut Vec<String>) {
    let at = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            for (k, wv) in w {
                match g.get(k) {
                    Some(gv) => diff(&at(k), wv, gv, out),
                    None => out.push(format!("{}: expected {}, missing", at(k), show(wv))),
                }
            }
            for (k, gv) in g {
                if !w.contains_key(k) {
                    out.push(format!("{}: not expected, got {}", at(k), show(gv)));
                }
            }
        }
        (Value::Array(w), Value::Array(g)) => {
            for i in 0..w.len().max(g.len()) {
                let p = format!("{path}[{i}]");
                match (w.get(i), g.get(i)) {
                    (Some(wv), Some(gv)) => diff(&p, wv, gv, out),
                    (Some(wv), None) => out.push(format!("{p}: expected {}, missing", show(wv))),
                    (None, Some(gv)) => out.push(format!("{p}: not expected, got {}", show(gv))),
                    (None, None) => {}
                }
            }
        }
        _ if want == got => {}
        _ => out.push(format!(
            "{}: expected {}, got {}",
            if path.is_empty() { "(resource)" } else { path },
            show(want),
            show(got)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn differences_are_named_by_path() {
        let mut out = Vec::new();
        diff(
            "",
            &json!({"id": "a", "status": "completed", "output": [{"valueInteger": 1}], "x": 1}),
            &json!({"id": "a", "status": "failed", "output": [{"valueInteger": 1}, {}], "y": 2}),
            &mut out,
        );
        assert_eq!(
            out,
            [
                "status: expected \"completed\", got \"failed\"",
                "output[1]: not expected, got {}",
                "x: expected 1, missing",
                "y: not expected, got 2",
            ]
        );
    }

    #[test]
    fn files_are_compared_by_name_and_line() {
        let (a, e) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let task = |s: &str| json!({"resourceType": "Task", "id": "t1", "status": s}).to_string();
        std::fs::write(a.path().join("Task.ndjson"), task("failed") + "\n").unwrap();
        std::fs::write(a.path().join("Basic.ndjson"), "{}\n").unwrap();
        std::fs::write(
            e.path().join("Task.ndjson"),
            task("completed") + "\n" + &task("x"),
        )
        .unwrap();
        std::fs::write(e.path().join("Location.ndjson"), "").unwrap();
        let out = compare(a.path(), e.path()).unwrap();
        assert_eq!(
            out,
            [
                "Location.ndjson: expected, but the run wrote none",
                "Basic.ndjson: written (1 resources), but not expected",
                "Task.ndjson line 1 (Task/t1):",
                "  status: expected \"completed\", got \"failed\"",
                "Task.ndjson line 2: expected Task/t1, but the file ends",
            ]
        );
        std::fs::write(
            a.path().join("Task.ndjson"),
            task("completed") + "\n" + &task("x"),
        )
        .unwrap();
        std::fs::remove_file(a.path().join("Basic.ndjson")).unwrap();
        std::fs::write(a.path().join("Location.ndjson"), "\n").unwrap();
        assert!(compare(a.path(), e.path()).unwrap().is_empty());
    }
}
