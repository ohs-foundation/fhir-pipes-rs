//! `fhir-pipes dhis2`: read a DHIS2 instance into FHIR NDJSON for `fhir-pipes load`.
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
pub mod calendar;
pub mod expression;
pub mod locations;
pub mod measures;
pub mod raw;
pub mod reports;
pub mod rerun;

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};

use flate2::Compression;
use flate2::write::GzEncoder;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cli::Dhis2Command;
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::{is_gz, read_text, with_suffix};

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
            return Err(PipesError::Usage(format!(
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
        return Err(PipesError::Usage(format!(
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

/// NDJSON written as it is produced: lines go to `<out>.tmp`, which
/// `finish` renames into place. Dropped unfinished (a failed run), the
/// temporary file is removed, unless `keep_partial` asked to keep it for a
/// later run to continue.
///
/// When `out` ends in `.gz` the file is gzip, written as one gzip member
/// per `sync` (a batch of reports). The members concatenated are one valid
/// gzip file, and the length after each is a place an interrupted run can
/// be cut back to, which the middle of a member is not.
pub struct NdjsonWriter {
    out: PathBuf,
    tmp: PathBuf,
    file: Option<BufWriter<File>>,
    /// The gzip member being written, holding the file meanwhile.
    member: Option<GzEncoder<BufWriter<File>>>,
    gz: bool,
    n: usize,
    /// Length of the file up to the last line written (plain), or the last
    /// member finished (gzip).
    bytes: u64,
    keep: bool,
}

impl NdjsonWriter {
    pub fn tmp_path(out: &Path) -> PathBuf {
        with_suffix(out, ".tmp")
    }

    pub fn create(out: &Path) -> Result<Self> {
        let tmp = Self::tmp_path(out);
        let file = File::create(&tmp).map_err(|e| PipesError::io(&tmp, e))?;
        Ok(Self::new(out, tmp, file, 0))
    }

    /// Carries on with the temporary file of an interrupted run: cut back
    /// to `bytes`, the length last synced, dropping anything half-written
    /// after it, then appended to.
    pub fn resume(out: &Path, bytes: u64) -> Result<Self> {
        let tmp = Self::tmp_path(out);
        let mut file = OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(|e| PipesError::io(&tmp, e))?;
        let len = file.metadata().map_err(|e| PipesError::io(&tmp, e))?.len();
        if len < bytes {
            return Err(PipesError::Usage(format!(
                "{}: {len} bytes, shorter than the {bytes} recorded; run without --resume to start over",
                tmp.display()
            )));
        }
        file.set_len(bytes).map_err(|e| PipesError::io(&tmp, e))?;
        file.seek(SeekFrom::End(0))
            .map_err(|e| PipesError::io(&tmp, e))?;
        Ok(Self::new(out, tmp, file, bytes))
    }

    fn new(out: &Path, tmp: PathBuf, file: File, bytes: u64) -> Self {
        Self {
            out: out.to_path_buf(),
            tmp,
            file: Some(BufWriter::with_capacity(1 << 20, file)),
            member: None,
            gz: is_gz(out),
            n: 0,
            bytes,
            keep: false,
        }
    }

    /// Keep the temporary file if the run fails.
    pub fn keep_partial(mut self) -> Self {
        self.keep = true;
        self
    }

    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// The text written up to the last sync, decompressed.
    pub fn read_back(&self) -> Result<Box<dyn std::io::BufRead + Send>> {
        let file = File::open(&self.tmp).map_err(|e| PipesError::io(&self.tmp, e))?;
        Ok(read_text(file.take(self.bytes), self.gz))
    }

    pub fn write(&mut self, r: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(r)?;
        line.push(b'\n');
        let written = if self.gz {
            if self.member.is_none() {
                let file = self.file.take().expect("written after finish");
                self.member = Some(GzEncoder::new(file, Compression::default()));
            }
            self.member.as_mut().map(|m| m.write_all(&line))
        } else {
            self.bytes += line.len() as u64;
            self.file.as_mut().map(|f| f.write_all(&line))
        };
        written
            .expect("written after finish")
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        self.n += 1;
        Ok(())
    }

    /// Ends the gzip member being written, if any.
    fn end_member(&mut self) -> std::io::Result<()> {
        if let Some(m) = self.member.take() {
            let mut file = m.finish()?;
            self.bytes = file.stream_position()?;
            self.file = Some(file);
        }
        Ok(())
    }

    /// Flushes everything written to disk; returns the file's length.
    pub fn sync(&mut self) -> Result<u64> {
        self.end_member()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        let f = self.file.as_mut().expect("synced after finish");
        f.flush().map_err(|e| PipesError::io(&self.tmp, e))?;
        f.get_ref()
            .sync_data()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        Ok(self.bytes)
    }

    /// Renames the file into place; returns the lines this writer wrote.
    pub fn finish(mut self) -> Result<usize> {
        self.end_member()
            .map_err(|e| PipesError::io(&self.tmp, e))?;
        let mut f = self.file.take().expect("finished once");
        // An empty gzip file is not valid gzip; an empty member is.
        if self.gz && self.bytes == 0 {
            f = GzEncoder::new(f, Compression::default())
                .finish()
                .map_err(|e| PipesError::io(&self.tmp, e))?;
        }
        f.flush().map_err(|e| PipesError::io(&self.tmp, e))?;
        drop(f);
        std::fs::rename(&self.tmp, &self.out).map_err(|e| PipesError::io(&self.out, e))?;
        Ok(self.n)
    }
}

impl Drop for NdjsonWriter {
    fn drop(&mut self) {
        if self.file.is_some() || self.member.is_some() {
            drop(self.member.take());
            drop(self.file.take());
            if !self.keep {
                let _ = std::fs::remove_file(&self.tmp);
            }
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
