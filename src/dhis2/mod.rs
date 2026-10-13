//! `kiln dhis2`: read a DHIS2 instance into FHIR NDJSON for `kiln load`.
//!
//! - `locations`: org units become ICR Locations (facilities also get their
//!   Organization), each carrying the org unit UID as an identifier under a
//!   system named after the instance.
//! - `measures`: indicators become Measures.
//! - `reports`: indicator values from `/api/analytics` become one
//!   MeasureReport per indicator, org unit and period. The org unit is mapped
//!   to its Location through that identifier.
//!
//! Every id is deterministic (`INSTANCE-<uid>…`), so re-running replaces the
//! same resources. Nothing is written to DHIS2.

pub mod api;
pub mod locations;
pub mod measures;
pub mod reports;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::Dhis2Command;
use crate::error::{KilnError, Result};

pub const ICR_BASE: &str = "https://icr.healthcampaigns.org";
pub const PROJECT_TAG_SYSTEM: &str =
    "https://icr.healthcampaigns.org/CodeSystem/icr-project-tag-cs";
/// Instance names go into every id, so they are kept short.
pub const MAX_INSTANCE_LEN: usize = 16;

pub fn run(cmd: &Dhis2Command) -> Result<()> {
    match cmd {
        Dhis2Command::Locations(args) => locations::run_locations(args),
        Dhis2Command::Measures(args) => measures::run_measures(args),
        Dhis2Command::Reports(args) => reports::run_reports(args),
    }
}

/// True for a name made of ASCII letters, digits and hyphens, 1–`max` long.
pub fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The short name an instance is filed under, and everything derived from
/// it: identifier systems, ids, canonical URLs and the meta tag.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
}

impl Instance {
    pub fn new(name: &str) -> Result<Self> {
        if !valid_name(name, MAX_INSTANCE_LEN) {
            return Err(KilnError::Usage(format!(
                "--instance must be letters, digits and hyphens, at most {MAX_INSTANCE_LEN} characters, got {name:?}"
            )));
        }
        Ok(Self {
            name: name.to_string(),
        })
    }

    pub fn orgunit_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/orgunit", self.name)
    }

    pub fn orgunit_code_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/orgunit-code", self.name)
    }

    pub fn indicator_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/indicator", self.name)
    }

    pub fn indicator_code_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/indicator-code", self.name)
    }

    pub fn data_element_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/data-element", self.name)
    }

    pub fn data_element_code_system(&self) -> String {
        format!(
            "{ICR_BASE}/identifiers/dhis2/{}/data-element-code",
            self.name
        )
    }

    pub fn data_set_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/data-set", self.name)
    }

    pub fn data_set_code_system(&self) -> String {
        format!("{ICR_BASE}/identifiers/dhis2/{}/data-set-code", self.name)
    }

    /// Code system of a group: `indicator-group`, `data-element-group`, or
    /// `data-element-group-set/<uid>` for the groups of one group set.
    pub fn group_system(&self, kind: &str) -> String {
        format!("{ICR_BASE}/CodeSystem/dhis2/{}/{kind}", self.name)
    }

    /// Code system of a breakdown's options: `dimension/<uid>` (a category,
    /// org unit group set or data element group set).
    pub fn dimension_system(&self, dimension: Option<&str>) -> String {
        match dimension {
            Some(d) => format!("{ICR_BASE}/CodeSystem/dhis2/{}/dimension/{d}", self.name),
            None => format!("{ICR_BASE}/CodeSystem/dhis2/{}/dimension", self.name),
        }
    }

    pub fn measure_id(&self, indicator: &str) -> String {
        format!("{}-{indicator}", self.name)
    }

    pub fn measure_url(&self, indicator: &str) -> String {
        format!("{ICR_BASE}/Measure/{}", self.measure_id(indicator))
    }

    pub fn tag(&self) -> Value {
        json!({"system": PROJECT_TAG_SYSTEM, "code": self.name})
    }
}

pub fn check_out(out: &Path) -> Result<()> {
    if out.is_dir() {
        return Err(KilnError::Usage(format!(
            "--out {}: is a directory",
            out.display()
        )));
    }
    Ok(())
}

/// One resource per line, written to a temporary file and renamed into
/// place, so a failed run never leaves a half-written file behind.
pub fn write_ndjson<'a>(
    out: &Path,
    resources: impl IntoIterator<Item = &'a Value>,
) -> Result<usize> {
    let mut w = NdjsonWriter::create(out)?;
    for r in resources {
        w.write(r)?;
    }
    w.finish()
}

/// NDJSON written as it is produced: lines go to `<out>.ndjson.tmp`, which
/// `finish` renames into place. Dropped unfinished (a failed run), the
/// temporary file is removed.
pub struct NdjsonWriter {
    out: PathBuf,
    tmp: PathBuf,
    w: Option<BufWriter<File>>,
    n: usize,
}

impl NdjsonWriter {
    pub fn create(out: &Path) -> Result<Self> {
        let tmp = out.with_extension("ndjson.tmp");
        let file = File::create(&tmp).map_err(|e| KilnError::io(&tmp, e))?;
        Ok(Self {
            out: out.to_path_buf(),
            tmp,
            w: Some(BufWriter::new(file)),
            n: 0,
        })
    }

    pub fn write(&mut self, r: &Value) -> Result<()> {
        let w = self.w.as_mut().expect("written after finish");
        serde_json::to_writer(&mut *w, r)?;
        w.write_all(b"\n")
            .map_err(|e| KilnError::io(&self.tmp, e))?;
        self.n += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<usize> {
        let mut w = self.w.take().expect("finished once");
        w.flush().map_err(|e| KilnError::io(&self.tmp, e))?;
        drop(w);
        std::fs::rename(&self.tmp, &self.out).map_err(|e| KilnError::io(&self.out, e))?;
        Ok(self.n)
    }
}

impl Drop for NdjsonWriter {
    fn drop(&mut self) {
        if let Some(w) = self.w.take() {
            drop(w);
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_short_and_plain() {
        assert!(Instance::new("sl-demo").is_ok());
        assert!(Instance::new("").is_err());
        assert!(Instance::new("sl demo").is_err());
        assert!(Instance::new("sl_demo").is_err());
        assert!(Instance::new("a-very-long-instance-name").is_err());
    }

    #[test]
    fn systems_and_ids_carry_the_instance() {
        let i = Instance::new("sl-demo").unwrap();
        assert_eq!(
            i.orgunit_system(),
            "https://icr.healthcampaigns.org/identifiers/dhis2/sl-demo/orgunit"
        );
        assert_eq!(i.measure_id("Uvn6LCg7dVU"), "sl-demo-Uvn6LCg7dVU");
        assert_eq!(
            i.measure_url("Uvn6LCg7dVU"),
            "https://icr.healthcampaigns.org/Measure/sl-demo-Uvn6LCg7dVU"
        );
        assert_eq!(i.tag()["code"], "sl-demo");
    }
}
