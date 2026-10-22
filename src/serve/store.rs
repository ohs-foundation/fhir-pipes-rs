//! Run records: `<data>/.serve/runs/<id>/run.json`, beside each attempt's
//! result file and the run's log. A record is written with an fsync before
//! and after its rename at every change of state, since a caller is told a
//! run was accepted only once its record exists.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::Mode;

/// A run's id: its UTC start time and four random hex digits, so ids sort
/// by time.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(now: DateTime<Utc>) -> RunId {
        let random = uuid::Uuid::new_v4().simple().to_string();
        RunId(format!("{}-{}", now.format("%Y%m%dT%H%M%SZ"), &random[..4]))
    }

    /// `s` if it has the pattern of an id, and nothing else: an id is joined
    /// to a path only after this.
    pub fn parse(s: &str) -> Option<RunId> {
        let b = s.as_bytes();
        let ok = b.len() == 21
            && b[..8].iter().all(u8::is_ascii_digit)
            && b[8] == b'T'
            && b[9..15].iter().all(u8::is_ascii_digit)
            && b[15] == b'Z'
            && b[16] == b'-'
            && b[17..]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c));
        ok.then(|| RunId(s.to_string()))
    }

    /// The same time with new random digits.
    fn redraw(&self) -> RunId {
        let random = uuid::Uuid::new_v4().simple().to_string();
        RunId(format!("{}-{}", &self.0[..16], &random[..4]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Accepted,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub fn finished(self) -> bool {
        !matches!(
            self,
            RunStatus::Accepted | RunStatus::Running | RunStatus::Waiting
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Api,
    Schedule,
}

/// What a run was asked to do: `refresh` with these options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub only: Vec<String>,
    #[serde(default)]
    pub skip: Vec<String>,
    #[serde(default)]
    pub push: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    /// The attempt's result file, or what `serve` made of its exit.
    pub result: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed,
}

/// A notify destination's state for one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub state: DeliveryState,
    pub tries: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub trigger: Trigger,
    pub request: RunRequest,
    pub status: RunStatus,
    pub accepted_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub attempts: Vec<Attempt>,
    /// Why the last failed attempt was or was not retried.
    pub retry: Option<String>,
    /// Destination name → its delivery.
    pub notify: BTreeMap<String, Delivery>,
}

impl RunRecord {
    pub fn new(id: RunId, trigger: Trigger, request: RunRequest, accepted_at: String) -> RunRecord {
        RunRecord {
            id,
            trigger,
            request,
            status: RunStatus::Accepted,
            accepted_at,
            started_at: None,
            finished_at: None,
            attempts: Vec::new(),
            retry: None,
            notify: BTreeMap::new(),
        }
    }

    /// Whether a destination still has this run to deliver.
    pub fn pending(&self) -> bool {
        self.notify
            .values()
            .any(|d| d.state == DeliveryState::Pending)
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The runs under `root` (`<data>/.serve/runs`), created if missing.
    pub fn open(root: PathBuf) -> Result<Store> {
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Store { root })
    }

    pub fn run_dir(&self, id: &RunId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// Create the run's directory, exclusively (a new id is drawn if one
    /// exists), and write its record.
    pub fn create(&self, record: &mut RunRecord) -> Result<()> {
        for _ in 0..16 {
            let dir = self.run_dir(&record.id);
            match std::fs::create_dir(&dir) {
                Ok(()) => return self.save(record),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    record.id = record.id.redraw();
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        }
        bail!("no free run id for {}", record.id.as_str())
    }

    pub fn save(&self, record: &RunRecord) -> Result<()> {
        let path = self.run_dir(&record.id).join("run.json");
        crate::fs::atomic_write(&path, serde_json::to_string_pretty(record)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn load(&self, id: &RunId) -> Result<RunRecord> {
        let path = self.run_dir(id).join("run.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The ids of every run, newest first.
    fn ids(&self) -> Result<Vec<RunId>> {
        let mut ids: Vec<RunId> = std::fs::read_dir(&self.root)
            .with_context(|| format!("reading {}", self.root.display()))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| RunId::parse(&n))
            .collect();
        ids.sort();
        ids.reverse();
        Ok(ids)
    }

    /// Every readable record, newest first; one that cannot be read is
    /// reported and left out.
    fn all(&self) -> Result<Vec<RunRecord>> {
        Ok(self
            .ids()?
            .iter()
            .filter_map(|id| match self.load(id) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
                    None
                }
            })
            .collect())
    }

    /// Up to `limit` runs, newest first.
    pub fn list(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let mut out = Vec::new();
        for id in self.ids()? {
            if out.len() >= limit {
                break;
            }
            if let Ok(r) = self.load(&id) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// The runs not finished: accepted, running or waiting.
    pub fn unfinished(&self) -> Result<Vec<RunRecord>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|r| !r.status.finished())
            .collect())
    }

    /// The finished runs with a destination still pending, oldest first.
    pub fn pending(&self) -> Result<Vec<RunRecord>> {
        let mut runs: Vec<RunRecord> = self
            .all()?
            .into_iter()
            .filter(|r| r.status.finished() && r.pending())
            .collect();
        runs.reverse();
        Ok(runs)
    }

    /// Remove finished runs beyond the newest `keep`, except those with a
    /// destination still pending.
    pub fn prune(&self, keep: usize) -> Result<()> {
        let finished = self.all()?.into_iter().filter(|r| r.status.finished());
        for r in finished.skip(keep).filter(|r| !r.pending()) {
            let dir = self.run_dir(&r.id);
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_792_461_600 + secs, 0).unwrap()
    }

    fn record(id: RunId) -> RunRecord {
        RunRecord::new(id, Trigger::Api, RunRequest::default(), "t".into())
    }

    #[test]
    fn ids_sort_by_time_and_parse_only_the_pattern() {
        let a = RunId::new(at(0));
        let b = RunId::new(at(1));
        assert!(a < b, "{a:?} {b:?}");
        assert_eq!(RunId::parse(a.as_str()), Some(a.clone()));
        assert_eq!(a.as_str().len(), "20261021T020000Z-7f3a".len());
        for bad in [
            "../x",
            "20261021T020000Z-7f3a/..",
            "20261021T020000Z-7F3A",
            "20261021T020000Z-7f3",
            "",
            "20261021T020000Z-7f3a\n",
        ] {
            assert_eq!(RunId::parse(bad), None, "{bad:?}");
        }
        assert!(RunId::parse("20261021T020000Z-7f3a").is_some());
    }

    #[test]
    fn create_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs")).unwrap();
        let id = RunId::parse("20261021T020000Z-7f3a").unwrap();
        let mut first = record(id.clone());
        store.create(&mut first).unwrap();
        let mut second = record(id.clone());
        store.create(&mut second).unwrap();
        assert_ne!(first.id, second.id);
        assert!(second.id.as_str().starts_with("20261021T020000Z-"));
        assert_eq!(store.load(&first.id).unwrap(), first);
        assert_eq!(store.load(&second.id).unwrap(), second);
    }

    #[test]
    fn list_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs")).unwrap();
        for i in 0..3 {
            store.create(&mut record(RunId::new(at(i)))).unwrap();
        }
        // Not a run: skipped.
        std::fs::create_dir_all(dir.path().join("runs/notes")).unwrap();
        let ids: Vec<RunId> = store.list(10).unwrap().into_iter().map(|r| r.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.reverse();
        assert_eq!(ids, sorted);
        assert_eq!(ids.len(), 3);
        assert_eq!(store.list(2).unwrap().len(), 2);
    }

    #[test]
    fn prune_keeps_unfinished_and_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs")).unwrap();
        let mut ids = Vec::new();
        for (i, (status, pending)) in [
            (RunStatus::Succeeded, false),
            (RunStatus::Failed, true),
            (RunStatus::Running, false),
            (RunStatus::Succeeded, false),
            (RunStatus::Succeeded, false),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = record(RunId::new(at(i as i64)));
            r.status = status;
            if pending {
                r.notify.insert(
                    "openfn".into(),
                    Delivery {
                        state: DeliveryState::Pending,
                        tries: 0,
                        last_error: None,
                    },
                );
            }
            store.create(&mut r).unwrap();
            ids.push(r.id);
        }
        store.prune(1).unwrap();
        let left: Vec<RunId> = store.list(10).unwrap().into_iter().map(|r| r.id).collect();
        // The newest finished, the pending one and the running one.
        assert_eq!(left, vec![ids[4].clone(), ids[2].clone(), ids[1].clone()]);
        assert_eq!(store.unfinished().unwrap().len(), 1);
    }

    #[test]
    fn a_save_survives_as_a_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs")).unwrap();
        let mut r = record(RunId::new(at(0)));
        store.create(&mut r).unwrap();
        r.status = RunStatus::Failed;
        r.retry = Some("not retried: failed at hooks".into());
        store.save(&r).unwrap();
        assert_eq!(store.load(&r.id).unwrap(), r);
        let leftovers: Vec<_> = std::fs::read_dir(store.run_dir(&r.id))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(leftovers, ["run.json"]);
    }
}
