//! Running `fhir-pipes dhis2 reports` again: comparing with a previous run's
//! output (`--previous`), and continuing an interrupted run (`--resume`).
//!
//! A full export can be close to a million reports, so neither keeps
//! documents in memory. `--previous` keeps a 128-bit key and a 64-bit content
//! hash per id; `--resume` keeps a small checkpoint file beside the output's
//! temporary file.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{PipesError, Result};
pub use crate::fhir::content::Change;
use crate::fhir::content::{self, Ignore};
use crate::fhir::ndjson::{NdjsonReader, Output, is_gz, with_suffix};
use crate::fs::{AtomicFile, atomic_write};
use crate::report::Report;

/// The elements of a report left out of its content hash: `date`, which is
/// when DHIS2 last built its analytics tables and so changes every night
/// whether or not the value did, and `meta.lastUpdated` / `meta.versionId`,
/// which a FHIR server sets.
static IGNORED: LazyLock<Ignore> =
    LazyLock::new(|| Ignore::of(&["date", "meta.lastUpdated", "meta.versionId"]));

/// A report's content, as a 64-bit hash, without the elements in `IGNORED`.
/// Object keys are taken in sorted order, so key order does not matter.
pub fn content_hash(r: &Value) -> u64 {
    content::content_hash(r, &IGNORED)
}

/// An id as a 128-bit key.
fn id_key(id: &str) -> u128 {
    content::key_hash(id)
}

/// The previous run's reports: per id, a content hash and whether this run
/// wrote the id again.
pub struct Previous {
    path: PathBuf,
    seen: HashMap<u128, (u64, bool)>,
}

/// Every MeasureReport in an NDJSON file (gzip when it ends in `.gz`).
fn each_report(path: &Path, mut f: impl FnMut(&str, &Value) -> Result<()>) -> Result<()> {
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let v: Value = serde_json::from_str(&line.text).map_err(|e| {
            PipesError::Usage(format!("{}: line {}: {e}", path.display(), line.number))
        })?;
        if v.get("resourceType").and_then(Value::as_str) != Some("MeasureReport") {
            continue;
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            f(id, &v)?;
        }
    }
    Ok(())
}

impl Previous {
    pub fn load(path: &Path) -> Result<Self> {
        let mut seen = HashMap::new();
        each_report(path, |id, v| {
            seen.insert(id_key(id), (content_hash(v), false));
            Ok(())
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            seen,
        })
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Compares a report written this run with the previous one of its id,
    /// and marks the id as written again.
    pub fn classify(&mut self, r: &Value) -> Change {
        let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
        match self.seen.get_mut(&id_key(id)) {
            None => Change::New,
            Some((hash, seen)) => {
                *seen = true;
                if *hash == content_hash(r) {
                    Change::Unchanged
                } else {
                    Change::Changed
                }
            }
        }
    }

    /// The ids of the previous run that this run did not write again, among
    /// the reports `in_scope` says this run would have written, in file order.
    /// The previous reports outside the scope go to `keep`, in file order.
    /// Reads the previous file a second time rather than keep its ids.
    pub fn removed(
        &self,
        in_scope: impl Fn(&Value) -> bool,
        mut keep: impl FnMut(&Value) -> Result<()>,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut listed = HashSet::new();
        each_report(&self.path, |id, v| {
            let key = id_key(id);
            let again = self.seen.get(&key).is_some_and(|(_, seen)| *seen);
            if again || !listed.insert(key) {
                return Ok(());
            }
            if in_scope(v) {
                out.push(id.to_string());
                Ok(())
            } else {
                keep(v)
            }
        })?;
        Ok(out)
    }
}

/// What a run asked for, as the reports it covers: a previous report is in
/// scope when this run would have written it had DHIS2 still had its value.
#[derive(Debug, Default)]
pub struct Scope {
    /// Measure canonical URLs.
    pub measures: HashSet<String>,
    /// `Location/<id>` references.
    pub subjects: HashSet<String>,
    /// `dhis2-period` extension values: period codes and range labels.
    pub periods: HashSet<String>,
}

impl Scope {
    pub fn contains(&self, r: &Value, period_extension: &str) -> bool {
        let s = |p: &str| r.pointer(p).and_then(Value::as_str).unwrap_or_default();
        let period = r
            .get("extension")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|e| e.get("url").and_then(Value::as_str) == Some(period_extension))
            .and_then(|e| e.get("valueString"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.measures.contains(s("/measure"))
            && self.subjects.contains(s("/subject/reference"))
            && self.periods.contains(period)
    }
}

/// One id per line (gzip when `out` ends in `.gz`), written to a temporary
/// file and renamed into place.
pub fn write_lines(out: &Path, lines: &[String]) -> Result<()> {
    let file = AtomicFile::create(out)?;
    let tmp = file.tmp().to_path_buf();
    let mut w = Output::new(file, is_gz(out));
    for l in lines {
        writeln!(w, "{l}").map_err(|e| PipesError::io(&tmp, e))?;
    }
    w.finish().map_err(|e| PipesError::io(&tmp, e))?.commit()
}

/// A file as the plan sees it: its path, length and modification time, so
/// a file edited since the interrupted run counts as a different plan.
pub fn file_stamp(path: &Path) -> Value {
    let meta = std::fs::metadata(path).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    json!({
        "path": path.display().to_string(),
        "bytes": meta.map(|m| m.len()),
        "modified": modified,
    })
}

/// Where an interrupted run stands, saved beside the output's temporary
/// file after every batch: the plan it was running, how many batches are in
/// the file and its length then, and the running totals that cannot be
/// recounted from the file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Everything that shapes the batches and their content.
    pub plan: Value,
    pub units_done: usize,
    pub out_bytes: u64,
    pub changed_bytes: Option<u64>,
    /// Values DHIS2 sent and analytics requests made.
    pub values: usize,
    pub requests: usize,
    /// Org units already reported as having no Location.
    pub unmapped: BTreeSet<String>,
    /// With `--previous`: the org units and periods DHIS2 listed in its
    /// responses, the scope removed ids are looked for in.
    pub scope_org_units: BTreeSet<String>,
    pub scope_periods: BTreeSet<String>,
    pub report: Report,
    /// With `--source data-values`: when the run started, the `date` of
    /// every report it writes.
    #[serde(default)]
    pub as_of: Option<String>,
}

impl Checkpoint {
    pub fn path(out: &Path) -> PathBuf {
        with_suffix(out, ".resume")
    }

    pub fn read(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                PipesError::Usage(format!(
                    "{}: not a checkpoint ({e}); run without --resume to start over",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(PipesError::io(path, e)),
        }
    }

    /// Written whole to a temporary file and renamed, so a crash leaves the
    /// previous checkpoint, never half of one.
    pub fn write(&self, path: &Path) -> Result<()> {
        atomic_write(path, serde_json::to_vec(self)?)
    }
}

/// The top-level keys on which two plans differ.
pub fn plan_differences(a: &Value, b: &Value) -> Vec<String> {
    let keys: BTreeSet<&String> = a
        .as_object()
        .into_iter()
        .chain(b.as_object())
        .flat_map(|o| o.keys())
        .collect();
    keys.into_iter()
        .filter(|k| a.get(k.as_str()) != b.get(k.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(value: f64, date: &str) -> Value {
        json!({"resourceType": "MeasureReport", "id": "a", "meta": {"tag": [{"code": "x"}]},
               "date": date, "group": [{"measureScore": {"value": value}}]})
    }

    #[test]
    fn the_hash_ignores_the_analytics_build_time_and_server_metadata() {
        let a = report(0.5, "2026-10-12");
        assert_eq!(content_hash(&a), content_hash(&report(0.5, "2026-10-13")));
        let mut served = a.clone();
        served["meta"]["lastUpdated"] = json!("2026-10-13T01:00:00Z");
        served["meta"]["versionId"] = json!("3");
        assert_eq!(content_hash(&a), content_hash(&served));
        assert_ne!(content_hash(&a), content_hash(&report(0.25, "2026-10-12")));
        let mut tagged = a.clone();
        tagged["meta"]["tag"][0]["code"] = json!("y");
        assert_ne!(content_hash(&a), content_hash(&tagged));
        // Key order does not matter.
        let reordered: Value = serde_json::from_str(
            r#"{"group":[{"measureScore":{"value":0.5}}],"date":"x","meta":{"tag":[{"code":"x"}]},"id":"a","resourceType":"MeasureReport"}"#,
        )
        .unwrap();
        assert_eq!(content_hash(&a), content_hash(&reordered));
    }

    #[test]
    fn plans_are_compared_key_by_key() {
        let a = json!({"levels": [[2, 14]], "chunk": 10});
        let b = json!({"levels": [[2, 15]], "chunk": 10, "campaign": "CarePlan/x"});
        assert_eq!(plan_differences(&a, &b), ["campaign", "levels"]);
        assert!(plan_differences(&a, &a).is_empty());
    }
}
