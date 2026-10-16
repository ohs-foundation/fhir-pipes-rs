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
use crate::fs::AtomicFile;
use crate::profile::{Dhis2Profile, Profile};

/// Instance names go into every id, so they are kept short.
pub const MAX_INSTANCE_LEN: usize = 16;

/// Every system, profile and extension URL comes from the profile's [dhis2]
/// section (and the Location roles from [location] and [organization]).
pub fn run(cmd: &Dhis2Command, profile: Option<&str>) -> Result<()> {
    let profile = Profile::load(profile)?;
    eprintln!("profile: {} ({})", profile.name, profile.source);
    profile.dhis2()?;
    match cmd {
        Dhis2Command::Locations(args) => locations::run_locations(args, &profile),
        Dhis2Command::Measures(args) => measures::run_measures(args, &profile),
        Dhis2Command::Reports(args) => reports::run_reports(args, &profile),
    }
}

/// True for a name made of ASCII letters, digits and hyphens, 1–`max` long.
pub fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The short name an instance is filed under, and everything derived from
/// it: identifier systems, ids, canonical URLs and the meta tag, all under
/// the profile's [dhis2] base.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub urls: Dhis2Profile,
    /// [location] admin_unit_type: the type code of an org unit above facility level.
    pub admin_unit_type: Option<String>,
    /// [organization] facility_type: the type code of a facility.
    pub facility_type: Option<String>,
    /// [organization] id_prefix of a facility's Organization (`org-`).
    pub org_id_prefix: String,
    /// The boundary extension a written Location carries.
    pub boundary_extension: String,
}

impl Instance {
    pub fn new(name: &str, profile: &Profile) -> Result<Self> {
        if !valid_name(name, MAX_INSTANCE_LEN) {
            return Err(PipesError::Usage(format!(
                "--instance must be letters, digits and hyphens, at most {MAX_INSTANCE_LEN} characters, got {name:?}"
            )));
        }
        Ok(Self {
            name: name.to_string(),
            urls: profile.dhis2()?.clone(),
            admin_unit_type: profile.location.admin_unit_type.clone(),
            facility_type: profile.organization.facility_type.clone(),
            org_id_prefix: profile.organization.id_prefix.clone(),
            boundary_extension: profile.boundary_extension().to_string(),
        })
    }

    fn base(&self) -> &str {
        self.urls.base.trim_end_matches('/')
    }

    pub fn orgunit_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/orgunit", self.base(), self.name)
    }

    pub fn orgunit_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/orgunit-code",
            self.base(),
            self.name
        )
    }

    pub fn indicator_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/indicator", self.base(), self.name)
    }

    pub fn indicator_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/indicator-code",
            self.base(),
            self.name
        )
    }

    pub fn data_element_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-element",
            self.base(),
            self.name
        )
    }

    pub fn data_element_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-element-code",
            self.base(),
            self.name
        )
    }

    pub fn data_set_system(&self) -> String {
        format!("{}/identifiers/dhis2/{}/data-set", self.base(), self.name)
    }

    pub fn data_set_code_system(&self) -> String {
        format!(
            "{}/identifiers/dhis2/{}/data-set-code",
            self.base(),
            self.name
        )
    }

    /// Code system of a group: `indicator-group`, `data-element-group`, or
    /// `data-element-group-set/<uid>` for the groups of one group set.
    pub fn group_system(&self, kind: &str) -> String {
        format!("{}/CodeSystem/dhis2/{}/{kind}", self.base(), self.name)
    }

    /// Code system of a breakdown's options: `dimension/<uid>` (a category,
    /// org unit group set or data element group set).
    pub fn dimension_system(&self, dimension: Option<&str>) -> String {
        match dimension {
            Some(d) => format!(
                "{}/CodeSystem/dhis2/{}/dimension/{d}",
                self.base(),
                self.name
            ),
            None => format!("{}/CodeSystem/dhis2/{}/dimension", self.base(), self.name),
        }
    }

    pub fn measure_id(&self, indicator: &str) -> String {
        format!("{}-{indicator}", self.name)
    }

    pub fn measure_url(&self, indicator: &str) -> String {
        format!("{}/Measure/{}", self.base(), self.measure_id(indicator))
    }

    pub fn tag(&self) -> Value {
        json!({"system": self.urls.project_tag_system, "code": self.name})
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
    tmp: PathBuf,
    file: Option<BufWriter<AtomicFile>>,
    /// The gzip member being written, holding the file meanwhile.
    member: Option<GzEncoder<BufWriter<AtomicFile>>>,
    gz: bool,
    n: usize,
    /// Length of the file up to the last line written (plain), or the last
    /// member finished (gzip).
    bytes: u64,
}

impl NdjsonWriter {
    pub fn tmp_path(out: &Path) -> PathBuf {
        with_suffix(out, ".tmp")
    }

    pub fn create(out: &Path) -> Result<Self> {
        let file = AtomicFile::create_at(out, Self::tmp_path(out))?;
        Ok(Self::new(out, file, 0))
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
        Ok(Self::new(out, AtomicFile::with_file(out, tmp, file), bytes))
    }

    fn new(out: &Path, file: AtomicFile, bytes: u64) -> Self {
        Self {
            tmp: file.tmp().to_path_buf(),
            file: Some(BufWriter::with_capacity(1 << 20, file)),
            member: None,
            gz: is_gz(out),
            n: 0,
            bytes,
        }
    }

    /// Keep the temporary file if the run fails.
    /// Set on the `AtomicFile` itself, so the file survives however the
    /// writer is dropped, a failed `finish` included.
    pub fn keep_partial(mut self) -> Self {
        if let Some(m) = self.member.as_mut() {
            m.get_mut().get_mut().keep_on_drop(true);
        }
        if let Some(f) = self.file.as_mut() {
            f.get_mut().keep_on_drop(true);
        }
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
        self.write_line(&line)
    }

    /// One line already serialised, ending in a newline.
    pub fn write_line(&mut self, line: &[u8]) -> Result<()> {
        let written = if self.gz {
            if self.member.is_none() {
                let file = self.file.take().expect("written after finish");
                self.member = Some(GzEncoder::new(file, Compression::default()));
            }
            self.member.as_mut().map(|m| m.write_all(line))
        } else {
            self.bytes += line.len() as u64;
            self.file.as_mut().map(|f| f.write_all(line))
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
            .file()
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
        f.into_inner()
            .map_err(|e| PipesError::io(&self.tmp, e.into_error()))?
            .commit()?;
        Ok(self.n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_partial_keeps_the_file_when_finish_fails() {
        for name in ["out.ndjson", "out.ndjson.gz"] {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join(name);
            // A directory with a file in it: the rename onto it fails.
            std::fs::create_dir(&out).unwrap();
            std::fs::write(out.join("x"), "x").unwrap();
            let mut w = NdjsonWriter::create(&out).unwrap().keep_partial();
            w.write(&json!({"id": "a"})).unwrap();
            let bytes = w.sync().unwrap();
            w.write(&json!({"id": "b"})).unwrap();
            assert!(w.finish().is_err(), "{name}");
            let tmp = NdjsonWriter::tmp_path(&out);
            assert!(tmp.is_file(), "{name}: the partial output is kept");
            let resumed = NdjsonWriter::resume(&out, bytes).unwrap();
            let mut text = String::new();
            resumed
                .read_back()
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(text, "{\"id\":\"a\"}\n", "{name}");
        }
    }

    #[test]
    fn without_keep_partial_a_failed_finish_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.ndjson");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("x"), "x").unwrap();
        let mut w = NdjsonWriter::create(&out).unwrap();
        w.write(&json!({"id": "a"})).unwrap();
        assert!(w.finish().is_err());
        assert!(!NdjsonWriter::tmp_path(&out).exists());
    }

    #[test]
    fn instance_names_are_short_and_plain() {
        assert!(Instance::new("sl-demo", &crate::profile::icr_example()).is_ok());
        assert!(Instance::new("", &crate::profile::icr_example()).is_err());
        assert!(Instance::new("sl-demo", &crate::profile::Profile::base()).is_err());
        assert!(Instance::new("sl demo", &crate::profile::icr_example()).is_err());
        assert!(Instance::new("sl_demo", &crate::profile::icr_example()).is_err());
        assert!(
            Instance::new("a-very-long-instance-name", &crate::profile::icr_example()).is_err()
        );
    }

    #[test]
    fn systems_and_ids_carry_the_instance() {
        let i = Instance::new("sl-demo", &crate::profile::icr_example()).unwrap();
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
