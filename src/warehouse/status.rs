//! `last-run.json` in the warehouse directory: how the last refresh of the
//! warehouse went, or that one is running. A run writes it only while it
//! holds the warehouse's lock, first as `running` and then as `succeeded` or
//! `failed`, each time to a temporary file renamed into place, so a reader
//! never sees half a file. A run that is killed leaves `running`, with its
//! process id.
//!
//! Unlike `raw/<server>/last-refresh.json`, which only a run that succeeds
//! writes, it records failures, so a scheduler or monitor can read how the
//! last run ended without its exit status.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::config::Settings;
use super::{Step, manifest};

pub const FILE: &str = "last-run.json";

/// Where a supervisor (`fhir-pipes serve`) wants the run's final record as
/// well, written whether or not the run got the lock.
pub const RESULT_FILE_ENV: &str = "FHIR_PIPES_RESULT_FILE";

/// The longest error message written to last-run.json and given to a hook as
/// ERROR, in characters; a longer one is cut there and ends with `…`.
const MAX_ERROR: usize = 2000;

/// Whether the database step's commit happened: `false`, `true`, or
/// `"unknown"` when the commit was sent and its answer was lost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
// Only the database step, built with the postgres feature, commits.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub enum Committed {
    #[default]
    No,
    Yes,
    Unknown,
}

impl Committed {
    fn json(self) -> Value {
        match self {
            Committed::No => json!(false),
            Committed::Yes => json!(true),
            Committed::Unknown => json!("unknown"),
        }
    }
}

pub struct Status {
    path: PathBuf,
    record: Value,
    started: Instant,
    /// Whether this run holds the lock and so writes last-run.json.
    owns_last_run: bool,
}

impl Status {
    /// Record that a run of `steps` has started.
    pub fn start(s: &Settings, steps: &BTreeSet<Step>) -> Result<Status> {
        let status = Status::new(s, steps, true);
        status.write()?;
        Ok(status)
    }

    /// The record of a run that did not get the lock: `finish` writes only
    /// the result file, since last-run.json belongs to the run that has it.
    pub fn unlocked(s: &Settings, steps: &BTreeSet<Step>) -> Status {
        Status::new(s, steps, false)
    }

    fn new(s: &Settings, steps: &BTreeSet<Step>, owns_last_run: bool) -> Status {
        Status {
            path: s.data.join(FILE),
            record: json!({
                "status": "running",
                "pid": std::process::id(),
                "started_at": manifest::now(),
                // Redacted as the manifest's is: a query string can carry a key.
                "server": s.server_used().map(crate::redact::url),
                "server_name": s.server_name,
                "full": s.full,
                "steps": steps.iter().map(|s| s.name()).collect::<Vec<_>>(),
            }),
            started: Instant::now(),
            owns_last_run,
        }
    }

    /// Record how the run ended: `result` is its `refreshed_at` or its error,
    /// `step` the part it was in, `timings` the parts that finished,
    /// `committed` and `published` what it changed. A failure to write is
    /// reported, not returned, so that it does not hide how the run ended.
    pub fn finish(
        mut self,
        result: &Result<String>,
        step: &str,
        timings: &[(&str, f64)],
        committed: Committed,
        published: bool,
    ) {
        let r = &mut self.record;
        r["committed"] = committed.json();
        r["published"] = json!(published);
        r["finished_at"] = json!(manifest::now());
        r["seconds"] = json!(self.started.elapsed().as_secs_f64());
        r["timings"] = timings
            .iter()
            .map(|(n, t)| (n.to_string(), json!(t)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        match result {
            Ok(refreshed_at) => {
                r["status"] = json!("succeeded");
                r["refreshed_at"] = json!(refreshed_at);
                r["exit_code"] = json!(0);
            }
            Err(e) => {
                r["status"] = json!("failed");
                r["failed_step"] = json!(step);
                r["error"] = json!(short(&crate::error::message(e)));
                r["exit_code"] = json!(crate::error::exit_code(e));
            }
        }
        // The result file first: a supervisor reads it once the run exits.
        if let Some(path) = std::env::var_os(RESULT_FILE_ENV).filter(|p| !p.is_empty()) {
            let path = PathBuf::from(path);
            let written = serde_json::to_string_pretty(&self.record)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(crate::fs::atomic_write(&path, text)?));
            if let Err(e) = written {
                eprintln!(
                    "fhir-pipes: warning: {}",
                    crate::error::message(&e.context(format!("writing {}", path.display())))
                );
            }
        }
        if self.owns_last_run
            && let Err(e) = self.write()
        {
            eprintln!(
                "fhir-pipes: warning: {}",
                crate::error::message(&e.context(format!("writing {FILE}")))
            );
        }
    }

    fn write(&self) -> Result<()> {
        let tmp = self.path.with_file_name(format!(".{FILE}.tmp"));
        fs::write(&tmp, serde_json::to_string_pretty(&self.record)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), self.path.display()))
    }
}

/// `message` cut to MAX_ERROR characters.
pub fn short(message: &str) -> String {
    // Secrets are hidden before the cut, which could leave one in part.
    let message = crate::redact::known_secrets(message);
    match message.char_indices().nth(MAX_ERROR) {
        Some((at, _)) => format!("{}…", &message[..at]),
        None => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_message_is_cut() {
        assert_eq!(short("down"), "down");
        let long = "é".repeat(MAX_ERROR + 5);
        let cut = short(&long);
        assert_eq!(cut.chars().count(), MAX_ERROR + 1);
        assert!(cut.ends_with('…'));
    }
}
