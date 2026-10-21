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

/// The longest error message written to last-run.json and given to a hook as
/// ERROR, in characters; a longer one is cut there and ends with `…`.
const MAX_ERROR: usize = 2000;

pub struct Status {
    path: PathBuf,
    record: Value,
    started: Instant,
}

impl Status {
    /// Record that a run of `steps` has started.
    pub fn start(s: &Settings, steps: &BTreeSet<Step>) -> Result<Status> {
        let status = Status {
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
        };
        status.write()?;
        Ok(status)
    }

    /// Record how the run ended: `result` is its `refreshed_at` or its error,
    /// `step` the part it was in, `timings` the parts that finished. A
    /// failure to write is reported, not returned, so that it does not hide
    /// how the run ended.
    pub fn finish(mut self, result: &Result<String>, step: &str, timings: &[(&str, f64)]) {
        let r = &mut self.record;
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
        if let Err(e) = self.write() {
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
