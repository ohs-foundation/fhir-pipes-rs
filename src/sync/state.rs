//! What earlier syncs from a directory sent, per sink server.
//!
//! `DIR/.sync-state.json` names, per sink URL, a directory under `DIR/.sync/`
//! and a short summary per type. The synced copy itself is one file per type
//! in that directory, `<TYPE>.tsv`: a line `<id>\t<meta.lastUpdated>` for
//! every resource the sink has from these syncs (the time empty when the
//! resource had none). It tells the next run what changed (an id that is new
//! or whose `meta.lastUpdated` differs) and what was deleted (an id that is no
//! longer in the download). It is kept apart from the JSON so that the summary
//! stays small however many resources there are.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PipesError, Result};
use crate::snapshot::same_server;

const STATE_FILE: &str = ".sync-state.json";
const SYNC_DIR: &str = ".sync";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    sinks: BTreeMap<String, SinkState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SinkState {
    /// The synced copies' directory, relative to DIR: `.sync/<n>`.
    dir: String,
    #[serde(default)]
    types: BTreeMap<String, TypeState>,
}

/// A summary for people reading the file; the `.tsv` is what runs use.
#[derive(Debug, Serialize, Deserialize)]
pub struct TypeState {
    /// Resources the sink has from these syncs.
    pub count: usize,
    /// The greatest meta.lastUpdated among them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    pub synced_at: String,
    pub fhir_pipes_version: String,
}

/// id -> meta.lastUpdated ("" when none) of each resource synced.
pub type Synced = HashMap<String, String>;

impl SyncState {
    /// The state in `dir`, or an empty one. An unreadable state file is an
    /// error: starting over would forget what to delete.
    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                PipesError::Usage(format!(
                    "{}: not a valid sync state ({e}); fix it, or remove it and the {SYNC_DIR} \
                     directory to sync everything again",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PipesError::io(&path, e)),
        }
    }

    /// Written beside the target and renamed into place.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let path = dir.join(STATE_FILE);
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(&tmp, text).map_err(|e| PipesError::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| PipesError::io(&path, e))
    }

    fn sink(&self, url: &str) -> Option<&SinkState> {
        self.sinks
            .iter()
            .find(|(u, _)| same_server(u, url))
            .map(|(_, s)| s)
    }

    /// The synced copy of `resource_type` at `url`, empty when there is none.
    pub fn synced(&self, root: &Path, url: &str, resource_type: &str) -> Result<Synced> {
        match self.sink(url) {
            Some(sink) => read_synced(&synced_path(root, &sink.dir, resource_type)),
            None => Ok(Synced::new()),
        }
    }

    /// Replace the synced copy of `resource_type` at `url`, then the summary.
    pub fn save(
        &mut self,
        root: &Path,
        url: &str,
        resource_type: &str,
        synced: &Synced,
        summary: TypeState,
    ) -> Result<()> {
        let key = url.trim_end_matches('/').to_string();
        if self.sink(url).is_none() {
            let n = (1..)
                .map(|n| format!("{SYNC_DIR}/{n}"))
                .find(|d| self.sinks.values().all(|s| &s.dir != d) && !root.join(d).exists())
                .expect("an unused name");
            self.sinks.insert(
                key.clone(),
                SinkState {
                    dir: n,
                    types: BTreeMap::new(),
                },
            );
        }
        let sink = self
            .sinks
            .iter_mut()
            .find(|(u, _)| same_server(u, &key))
            .map(|(_, s)| s)
            .expect("added above");
        write_synced(&synced_path(root, &sink.dir, resource_type), synced)?;
        sink.types.insert(resource_type.to_string(), summary);
        self.write(root)
    }
}

fn synced_path(root: &Path, dir: &str, resource_type: &str) -> PathBuf {
    root.join(dir).join(format!("{resource_type}.tsv"))
}

fn read_synced(path: &Path) -> Result<Synced> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Synced::new()),
        Err(e) => return Err(PipesError::io(path, e)),
    };
    let mut synced = Synced::new();
    for (n, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| PipesError::io(path, e))?;
        if line.is_empty() {
            continue;
        }
        let Some((id, updated)) = line.split_once('\t') else {
            return Err(PipesError::Usage(format!(
                "{}: line {}: not <id><tab><lastUpdated>",
                path.display(),
                n + 1
            )));
        };
        synced.insert(id.to_string(), updated.to_string());
    }
    Ok(synced)
}

/// Sorted by id, so the file is the same for the same copy.
fn write_synced(path: &Path, synced: &Synced) -> Result<()> {
    let dir = path.parent().expect("a file in a directory");
    std::fs::create_dir_all(dir).map_err(|e| PipesError::io(dir, e))?;
    let tmp = crate::fhir::ndjson::with_suffix(path, ".tmp");
    let mut rows: Vec<(&String, &String)> = synced.iter().collect();
    rows.sort_unstable();
    let file = std::fs::File::create(&tmp).map_err(|e| PipesError::io(&tmp, e))?;
    let mut w = std::io::BufWriter::new(file);
    for (id, updated) in rows {
        writeln!(w, "{id}\t{updated}").map_err(|e| PipesError::io(&tmp, e))?;
    }
    w.flush().map_err(|e| PipesError::io(&tmp, e))?;
    drop(w);
    std::fs::rename(&tmp, path).map_err(|e| PipesError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(count: usize) -> TypeState {
        TypeState {
            count,
            newest: None,
            synced_at: "2026-01-01T00:00:00Z".into(),
            fhir_pipes_version: "0".into(),
        }
    }

    #[test]
    fn each_sink_keeps_its_own_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut state = SyncState::read(root).unwrap();
        let a: Synced = [("p1".to_string(), "2026-01-01T00:00:00Z".to_string())].into();
        let b: Synced = [("p2".to_string(), String::new())].into();
        state
            .save(root, "http://a/fhir/", "Patient", &a, summary(1))
            .unwrap();
        state
            .save(root, "http://b/fhir", "Patient", &b, summary(1))
            .unwrap();

        let state = SyncState::read(root).unwrap();
        assert_eq!(state.synced(root, "http://a/fhir", "Patient").unwrap(), a);
        assert_eq!(state.synced(root, "http://b/fhir/", "Patient").unwrap(), b);
        assert!(
            state
                .synced(root, "http://c/fhir", "Patient")
                .unwrap()
                .is_empty()
        );
        assert!(root.join(".sync/1/Patient.tsv").is_file());
        assert!(root.join(".sync/2/Patient.tsv").is_file());
    }

    #[test]
    fn a_broken_state_file_is_an_error_not_a_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATE_FILE), "{").unwrap();
        assert!(matches!(
            SyncState::read(dir.path()),
            Err(PipesError::Usage(_))
        ));
    }
}
