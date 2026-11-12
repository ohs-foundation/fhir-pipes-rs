//! `import run --previous`: this run's output compared with an earlier
//! run's, resource by resource (`resourceType` and `id`).
//!
//! A previous run can hold millions of resources, so none is kept in
//! memory: per resource, a 128-bit key and a 64-bit content hash (see
//! `crate::fhir::content`), as `dhis2 reports --previous` keeps. The
//! previous files are read twice: once for the hashes, and once after the
//! comparison for the ids of the resources this run did not write.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;

use super::WRITTEN;
use super::manifest::IgnorePath;
use crate::error::{PipesError, usage};
use crate::fhir::content::{Change, Ignore, content_hash, key_hash};
use crate::fhir::ndjson::NdjsonReader;

/// The elements a FHIR server sets on a resource it stores, never compared:
/// a file that came back from a server carries them.
const SERVER_SET: [&str; 3] = ["meta.versionId", "meta.lastUpdated", "meta.source"];

/// The elements left out of the comparison, by resource type: those a
/// server sets, and the package's `[compare] ignore`.
pub struct Ignores {
    all: Ignore,
    by_type: HashMap<String, Ignore>,
}

impl Ignores {
    pub fn new(paths: &[IgnorePath]) -> Self {
        let all = Ignore::of(&SERVER_SET);
        let mut by_type: HashMap<String, Ignore> = HashMap::new();
        for p in paths {
            by_type
                .entry(p.resource_type.clone())
                .or_insert_with(|| all.clone())
                .add(&p.path);
        }
        Self { all, by_type }
    }

    fn of(&self, resource_type: &str) -> &Ignore {
        self.by_type.get(resource_type).unwrap_or(&self.all)
    }

    fn hash(&self, resource_type: &str, v: &Value) -> u64 {
        content_hash(v, self.of(resource_type))
    }
}

/// The resource files of an earlier run's `--out`: the files its
/// `.fhir-pipes-import.json` lists, or with none, every `*.ndjson` and
/// `*.ndjson.gz` file in it.
fn previous_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let list = dir.join(WRITTEN);
    if list.is_file() {
        let text = std::fs::read_to_string(&list)
            .with_context(|| format!("reading {}", list.display()))?;
        let names: Vec<String> = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v["files"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(str::to_string))
                        .collect()
                })
            })
            .ok_or_else(|| {
                usage(anyhow!(
                    "--previous {}: {WRITTEN} does not list the files of a run",
                    dir.display()
                ))
            })?;
        let mut files = Vec::new();
        for name in names {
            if !name.ends_with(".ndjson") || name.contains(['/', '\\']) {
                continue;
            }
            let path = dir.join(&name);
            if !path.is_file() {
                return Err(usage(anyhow!(
                    "--previous {}: {name} is listed in {WRITTEN}, but is not there",
                    dir.display()
                )));
            }
            files.push(path);
        }
        files.sort();
        return Ok(files);
    }
    Ok(crate::fs::sorted_entries(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                !n.starts_with('.') && (n.ends_with(".ndjson") || n.ends_with(".ndjson.gz"))
            })
        })
        .collect())
}

/// Every resource in an NDJSON file, with its type and id. A line that is
/// not a JSON object with a `resourceType` and an `id` is a usage error.
fn each_resource(path: &Path, mut f: impl FnMut(&str, &str, &Value) -> Result<()>) -> Result<()> {
    for line in NdjsonReader::open(path)? {
        let line = line?;
        let at = || format!("{}:{}", path.display(), line.number);
        let v: Value = serde_json::from_str(&line.text)
            .map_err(|e| usage(anyhow!("{}: not JSON ({e})", at())))?;
        let Some(t) = v.get("resourceType").and_then(Value::as_str) else {
            return Err(usage(anyhow!("{}: a line with no resourceType", at())));
        };
        let Some(id) = v
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Err(usage(anyhow!(
                "{}: a {t} with no id; --previous compares resources by resourceType and id",
                at()
            )));
        };
        f(t, id, &v)?;
    }
    Ok(())
}

fn key(resource_type: &str, id: &str) -> u128 {
    key_hash(&format!("{resource_type}/{id}"))
}

/// The previous run's resources: per `resourceType/id`, a content hash and
/// whether this run wrote it again.
pub struct Previous {
    pub dir: PathBuf,
    files: Vec<PathBuf>,
    seen: HashMap<u128, (u64, bool)>,
    ignores: Ignores,
}

impl Previous {
    /// Reads the resource files of `dir`, an earlier run's `--out`.
    pub fn load(dir: &Path, ignores: Ignores) -> Result<Self> {
        let files = previous_files(dir)?;
        let mut seen = HashMap::new();
        for path in &files {
            each_resource(path, |t, id, v| {
                seen.insert(key(t, id), (ignores.hash(t, v), false));
                Ok(())
            })?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            files,
            seen,
            ignores,
        })
    }

    /// The resources read (one per `resourceType/id`).
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Compares a resource written this run with the previous one of its
    /// type and id, and marks it as written again.
    fn classify(&mut self, t: &str, id: &str, v: &Value) -> Change {
        match self.seen.get_mut(&key(t, id)) {
            None => Change::New,
            Some((hash, seen)) => {
                *seen = true;
                if *hash == self.ignores.hash(t, v) {
                    Change::Unchanged
                } else {
                    Change::Changed
                }
            }
        }
    }

    /// `ResourceType/id` of every previous resource this run did not write,
    /// each once, sorted. Reads the previous files again rather than keep
    /// their ids.
    fn removed(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut listed = HashSet::new();
        for path in &self.files {
            each_resource(path, |t, id, _| {
                let k = key(t, id);
                let again = self.seen.get(&k).is_some_and(|(_, seen)| *seen);
                if !again && listed.insert(k) {
                    out.push(format!("{t}/{id}"));
                }
                Ok(())
            })?;
        }
        out.sort();
        Ok(out)
    }
}

/// Resources by how they compare with the previous run's.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub new: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub removed: usize,
}

impl Counts {
    fn add(&mut self, change: Change) {
        match change {
            Change::New => self.new += 1,
            Change::Changed => self.changed += 1,
            Change::Unchanged => self.unchanged += 1,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} new, {} changed, {} unchanged, {} removed",
            self.new, self.changed, self.unchanged, self.removed
        )
    }
}

/// How a run's output compares with the previous run's, for the report.
#[derive(Debug, Serialize)]
pub struct Comparison {
    /// The `--previous` folder.
    pub previous: String,
    /// Every resource type.
    #[serde(flatten)]
    pub total: Counts,
    /// By resource type: the types of this run's output and those only the
    /// previous run had.
    pub types: BTreeMap<String, Counts>,
}

/// The comparison, with what goes to `--changed` and `--removed`.
pub struct Compared {
    pub comparison: Comparison,
    /// `ResourceType/id`, sorted.
    pub removed: Vec<String>,
    /// The resources written to the changed folder, by type.
    pub changed: BTreeMap<String, usize>,
}

/// Compares the output files of `types` in `out` with `previous`. With
/// `changed`, the new and changed resources are written there, one
/// `<ResourceType>.ndjson` per type in the order of the output, each line as
/// the output has it; a type with none gets no file. A resource written
/// twice is compared twice.
pub fn compare(
    previous: &mut Previous,
    out: &Path,
    types: &[&String],
    changed: Option<&Path>,
) -> Result<Compared> {
    if let Some(dir) = changed {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut by_type: BTreeMap<String, Counts> = BTreeMap::new();
    let mut written = BTreeMap::new();
    for t in types {
        crate::stop::check()?;
        let path = out.join(format!("{t}.ndjson"));
        let mut counts = Counts::default();
        let mut writer: Option<(PathBuf, BufWriter<File>)> = None;
        let mut n = 0usize;
        for line in NdjsonReader::open(&path)? {
            let line = line?;
            let v: Value = serde_json::from_str(&line.text)
                .with_context(|| format!("{}:{}: not JSON", path.display(), line.number))?;
            let rt = v.get("resourceType").and_then(Value::as_str).unwrap_or(t);
            let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
            let change = previous.classify(rt, id, &v);
            counts.add(change);
            if change == Change::Unchanged {
                continue;
            }
            if let Some(dir) = changed {
                if writer.is_none() {
                    let p = dir.join(format!("{t}.ndjson"));
                    let file = crate::fs::create_private(&p).map_err(|e| PipesError::io(&p, e))?;
                    writer = Some((p, BufWriter::with_capacity(1 << 20, file)));
                }
                let (p, w) = writer.as_mut().expect("just made");
                w.write_all(line.text.as_bytes())
                    .and_then(|()| w.write_all(b"\n"))
                    .map_err(|e| PipesError::io(&*p, e))?;
                n += 1;
            }
        }
        if let Some((p, mut w)) = writer {
            w.flush().map_err(|e| PipesError::io(&p, e))?;
            written.insert(t.to_string(), n);
        }
        by_type.insert(t.to_string(), counts);
    }
    let removed = previous.removed()?;
    for r in &removed {
        let t = r.split_once('/').map_or(r.as_str(), |(t, _)| t);
        by_type.entry(t.to_string()).or_default().removed += 1;
    }
    let mut total = Counts::default();
    for c in by_type.values() {
        total.new += c.new;
        total.changed += c.changed;
        total.unchanged += c.unchanged;
        total.removed += c.removed;
    }
    Ok(Compared {
        comparison: Comparison {
            previous: previous.dir.display().to_string(),
            total,
            types: by_type,
        },
        removed,
        changed: written,
    })
}

/// Writes `lines`, one per line, to `path` through a temporary file beside
/// it, readable by its owner only on Unix: the ids can be patient ids.
pub fn write_lines(path: &Path, lines: &[String]) -> Result<()> {
    let file = crate::fs::AtomicFile::create_private_at(
        path,
        crate::fhir::ndjson::with_suffix(path, ".tmp"),
    )?;
    let tmp = file.tmp().to_path_buf();
    let mut w = BufWriter::new(file);
    for l in lines {
        writeln!(w, "{l}").map_err(|e| PipesError::io(&tmp, e))?;
    }
    let file = w
        .into_inner()
        .map_err(|e| PipesError::io(&tmp, e.into_error()))?;
    file.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(list: &[(&str, &[&str])]) -> Vec<IgnorePath> {
        list.iter()
            .map(|(t, p)| IgnorePath {
                resource_type: t.to_string(),
                path: p.iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }

    #[test]
    fn server_metadata_is_never_compared_and_package_paths_only_for_their_type() {
        let ignores = Ignores::new(&paths(&[("MeasureReport", &["date"])]));
        let report = json!({"resourceType": "MeasureReport", "id": "a", "date": "2026-10-12",
                            "meta": {"tag": [{"code": "x"}]}});
        let mut served = report.clone();
        served["meta"]["versionId"] = json!("3");
        served["meta"]["lastUpdated"] = json!("2026-10-13T01:00:00Z");
        served["meta"]["source"] = json!("#abc");
        served["date"] = json!("2026-10-13");
        let h = |v: &Value| ignores.hash(v["resourceType"].as_str().unwrap(), v);
        assert_eq!(h(&report), h(&served));
        let mut tagged = report.clone();
        tagged["meta"]["tag"][0]["code"] = json!("y");
        assert_ne!(h(&report), h(&tagged));
        // `date` is compared on another type.
        let mut obs = report.clone();
        obs["resourceType"] = json!("Observation");
        let mut later = obs.clone();
        later["date"] = json!("2026-10-13");
        assert_ne!(h(&obs), h(&later));
        later["date"] = obs["date"].clone();
        later["meta"]["lastUpdated"] = json!("x");
        assert_eq!(h(&obs), h(&later));
    }

    #[test]
    fn the_previous_files_are_the_listed_ones_or_every_ndjson_file() {
        let dir = tempfile::tempdir().unwrap();
        let line = |t: &str, id: &str| format!("{}\n", json!({"resourceType": t, "id": id}));
        std::fs::write(dir.path().join("Patient.ndjson"), line("Patient", "p")).unwrap();
        std::fs::write(dir.path().join("Other.ndjson"), line("Basic", "b")).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let all = previous_files(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        std::fs::write(
            dir.path().join(WRITTEN),
            r#"{"package": "p", "files": ["Patient.ndjson"]}"#,
        )
        .unwrap();
        let mut prev = Previous::load(dir.path(), Ignores::new(&[])).unwrap();
        assert_eq!(prev.len(), 1);
        assert_eq!(
            prev.classify(
                "Patient",
                "p",
                &json!({"resourceType": "Patient", "id": "p"})
            ),
            Change::Unchanged
        );
        assert_eq!(prev.removed().unwrap(), Vec::<String>::new());
        std::fs::write(
            dir.path().join(WRITTEN),
            r#"{"package": "p", "files": ["Gone.ndjson"]}"#,
        )
        .unwrap();
        let e = Previous::load(dir.path(), Ignores::new(&[])).err().unwrap();
        assert!(e.to_string().contains("Gone.ndjson is listed"), "{e}");
    }
}
