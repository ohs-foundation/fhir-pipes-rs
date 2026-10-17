//! The change log of an export directory, `<dir>/.changes.json`: per resource
//! type, the ids of the resources that changed or were deleted since the
//! database step last loaded the view tables. `refresh` keeps it for
//! `[database] mode = "upsert"`, which rewrites only those resources' rows
//! (see warehouse/database.rs).
//!
//! An export adds to it before it replaces a type's file:
//!   - an incremental merge (search, bulk, HAPI database): the ids it read,
//!     and the ids the server listed as deleted
//!   - a file replaced whole (a full download, the full download after a
//!     count mismatch, a full bulk export or HAPI database read, a file
//!     source's split): the ids whose line differs between the old file and
//!     the new one or that only the new one has, and the ids only the old one
//!     has (deleted). Without an old file, or when most of the type changed,
//!     the type is marked `full` instead.
//!
//! Ids recorded for a resource that did not change (one fetched again, or a
//! file that was not renamed into place after all) cost a row rewritten, not
//! a wrong row, so the log is written before the file it describes. Nothing
//! is recorded in a directory without the file: refresh creates it at the
//! start of a run in upsert mode, and removes it in any other mode.
//!
//! `running` says a refresh started and has not finished, and `reload` why
//! the next load must reload every table in full; while it is set the ids are
//! not recorded, since they would not be read. `database` and `tables`
//! describe the last load (warehouse/database.rs).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fhir::ndjson::NdjsonReader;
use crate::snapshot::merge::id_and_updated;

pub const FILE: &str = ".changes.json";

/// Below this many ids a type's changes are always recorded by id.
const FULL_ABOVE: usize = 10_000;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Changes {
    /// A refresh started and has not finished.
    #[serde(default)]
    pub running: bool,
    /// The next load reloads every table in full, for this reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<String>,
    /// The database and schema of the last load, without credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Each table of the last load, with what its rows were built from.
    #[serde(default)]
    pub tables: BTreeMap<String, String>,
    /// What changed since the last load, per resource type.
    #[serde(default)]
    pub types: BTreeMap<String, TypeChanges>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeChanges {
    /// Too much changed to list, or the old file was missing: every table
    /// of the type is reloaded in full.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full: bool,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub changed: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deleted: BTreeSet<String>,
}

impl TypeChanges {
    /// Every id whose rows are to be rewritten.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub fn ids(&self) -> BTreeSet<&str> {
        self.changed
            .iter()
            .chain(&self.deleted)
            .map(String::as_str)
            .collect()
    }
}

/// Exports in one process record from several threads; each read, change
/// and write of the file is done under this lock.
static LOCK: Mutex<()> = Mutex::new(());

impl Changes {
    /// The log in `dir`: `Ok(None)` when there is none, `Err` with why when
    /// it cannot be read or parsed.
    pub fn read(dir: &Path) -> std::result::Result<Option<Changes>, String> {
        let path = dir.join(FILE);
        crate::fs::read_state(&path).map_err(|e| match e {
            crate::fs::StateError::Invalid(e) => format!("{}: not valid ({e})", path.display()),
            crate::fs::StateError::Unreadable(e) | crate::fs::StateError::Io(e) => {
                format!("{}: not readable ({e})", path.display())
            }
        })
    }

    /// Written beside the target and renamed into place.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        crate::fs::atomic_write(&dir.join(FILE), text)
    }
}

/// Whether ids are being recorded in `dir`.
fn recording(dir: &Path) -> bool {
    let _held = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    matches!(Changes::read(dir), Ok(Some(c)) if c.reload.is_none())
}

/// Apply `f` to `rtype`'s entry in the log in `dir`, if there is one and it
/// is recording. A log that cannot be read is replaced by one that asks for
/// a full reload.
fn update(dir: &Path, rtype: &str, f: impl FnOnce(&mut TypeChanges)) -> Result<()> {
    let _held = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    match Changes::read(dir) {
        Ok(None) => Ok(()),
        Ok(Some(c)) if c.reload.is_some() => Ok(()),
        Ok(Some(mut c)) => {
            let t = c.types.entry(rtype.to_string()).or_default();
            if !t.full {
                f(t);
            }
            if t.full {
                t.changed.clear();
                t.deleted.clear();
            }
            c.write(dir)
        }
        Err(why) => {
            eprintln!("{why}; the database is reloaded in full at the next load");
            Changes {
                running: true,
                reload: Some("the change log could not be read".into()),
                ..Changes::default()
            }
            .write(dir)
        }
    }
}

/// Record that the resources `changed` of `rtype` were read again and those
/// `deleted` removed, by a merge into its file.
pub fn record<'a>(
    dir: &Path,
    rtype: &str,
    changed: impl IntoIterator<Item = &'a str>,
    deleted: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    update(dir, rtype, |t| {
        t.changed.extend(changed.into_iter().map(str::to_string));
        t.deleted.extend(deleted.into_iter().map(str::to_string));
    })
}

/// Record that every table of `rtype` is to be reloaded.
pub fn record_full(dir: &Path, rtype: &str) -> Result<()> {
    update(dir, rtype, |t| t.full = true)
}

/// Record what differs between `old`, `rtype`'s file, and `new`, the file
/// about to replace it. Without `old`, the type is marked `full`.
pub fn record_replaced(dir: &Path, rtype: &str, old: &Path, new: &Path) -> Result<()> {
    // The files are read only when the ids would be kept.
    if !recording(dir) {
        return update(dir, rtype, |_| {});
    }
    if !old.exists() {
        return record_full(dir, rtype);
    }
    let d = diff(old, new)?;
    let n = d.changed.len() + d.deleted.len();
    if n > FULL_ABOVE && n * 2 > d.total {
        return record_full(dir, rtype);
    }
    record(
        dir,
        rtype,
        d.changed.iter().map(String::as_str),
        d.deleted.iter().map(String::as_str),
    )
}

/// What differs between two NDJSON files of one type.
#[derive(Debug, PartialEq)]
struct Diff {
    /// Ids whose line differs, or that only the new file has.
    changed: Vec<String>,
    /// Ids only the old file has.
    deleted: Vec<String>,
    /// The resources in the new file.
    total: usize,
}

/// Compare `old` and `new` by id, each line by a hash of its text: the same
/// resource written differently counts as changed, which only costs its rows
/// rewritten. Lines without an id are left out, as the merges leave them.
fn diff(old: &Path, new: &Path) -> Result<Diff> {
    let hash = |text: &str| {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        h.finish()
    };
    let mut before: HashMap<String, u64> = HashMap::new();
    for line in NdjsonReader::open(old)? {
        let line = line?;
        if let Some((id, _)) = id_and_updated(&line.text) {
            before.insert(id, hash(&line.text));
        }
    }
    let mut changed = Vec::new();
    let mut total = 0;
    for line in NdjsonReader::open(new)? {
        let line = line?;
        let Some((id, _)) = id_and_updated(&line.text) else {
            continue;
        };
        total += 1;
        if before.remove(&id) != Some(hash(&line.text)) {
            changed.push(id);
        }
    }
    let mut deleted: Vec<String> = before.into_keys().collect();
    deleted.sort();
    Ok(Diff {
        changed,
        deleted,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(dir: &Path) {
        Changes {
            running: true,
            ..Changes::default()
        }
        .write(dir)
        .unwrap();
    }

    fn log(dir: &Path) -> Changes {
        Changes::read(dir).unwrap().unwrap()
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nothing_is_recorded_without_a_log() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "Patient", ["a"], []).unwrap();
        record_full(dir.path(), "Patient").unwrap();
        assert!(!dir.path().join(FILE).exists());
    }

    #[test]
    fn merges_add_up_until_the_type_is_full() {
        let dir = tempfile::tempdir().unwrap();
        start(dir.path());
        record(dir.path(), "Patient", ["a", "b"], []).unwrap();
        record(dir.path(), "Patient", ["b"], ["c"]).unwrap();
        record(dir.path(), "Observation", [], ["o"]).unwrap();
        let c = log(dir.path());
        assert_eq!(c.types["Patient"].changed, set(&["a", "b"]));
        assert_eq!(c.types["Patient"].deleted, set(&["c"]));
        assert_eq!(c.types["Patient"].ids().len(), 3);
        assert_eq!(c.types["Observation"].deleted, set(&["o"]));
        record_full(dir.path(), "Patient").unwrap();
        record(dir.path(), "Patient", ["d"], []).unwrap();
        let c = log(dir.path());
        assert!(c.types["Patient"].full);
        assert!(c.types["Patient"].ids().is_empty());
        assert!(c.running);
    }

    #[test]
    fn a_reload_records_nothing_and_a_bad_log_asks_for_one() {
        let dir = tempfile::tempdir().unwrap();
        Changes {
            reload: Some("test".into()),
            ..Changes::default()
        }
        .write(dir.path())
        .unwrap();
        record(dir.path(), "Patient", ["a"], []).unwrap();
        assert!(log(dir.path()).types.is_empty());
        std::fs::write(dir.path().join(FILE), "{nope").unwrap();
        record(dir.path(), "Patient", ["a"], []).unwrap();
        let c = log(dir.path());
        assert!(c.reload.is_some() && c.running && c.types.is_empty());
    }

    #[test]
    fn a_replaced_file_records_what_differs() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("old.ndjson"), dir.path().join("new.ndjson"));
        std::fs::write(
            &old,
            "{\"id\":\"a\",\"v\":1}\n{\"id\":\"b\",\"v\":1}\n{\"id\":\"c\"}\nnot json\n",
        )
        .unwrap();
        std::fs::write(
            &new,
            "{\"id\":\"b\",\"v\":2}\n{\"id\":\"a\",\"v\":1}\n{\"id\":\"d\"}\n",
        )
        .unwrap();
        assert_eq!(
            diff(&old, &new).unwrap(),
            Diff {
                changed: vec!["b".into(), "d".into()],
                deleted: vec!["c".into()],
                total: 3,
            }
        );
        start(dir.path());
        record_replaced(dir.path(), "Patient", &old, &new).unwrap();
        record_replaced(dir.path(), "Task", &dir.path().join("none.ndjson"), &new).unwrap();
        let c = log(dir.path());
        assert_eq!(c.types["Patient"].changed, set(&["b", "d"]));
        assert_eq!(c.types["Patient"].deleted, set(&["c"]));
        assert!(c.types["Task"].full);
    }
}
