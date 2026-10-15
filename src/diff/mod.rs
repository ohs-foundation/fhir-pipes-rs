//! `fhir-pipes diff`: edited GeoJSON or GeoParquet -> FHIR NDJSON of the Locations
//! that changed. Offline. The snapshot is indexed by id and byte offset;
//! the input is streamed; each named resource is re-read, rebuilt from the
//! writable columns, and emitted only when its canonical JSON differs.

pub mod compare;
pub mod geojson;
pub mod input;
pub mod parquet;
pub mod rebuild;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cli::DiffArgs;
use crate::diff::compare::canonical;
use crate::diff::geojson::{feature_to_row, read_feature_collection, read_feature_lines};
use crate::diff::input::{ColumnValue, InputRow};
use crate::diff::parquet::read_geoparquet;
use crate::diff::rebuild::{new_organization, rebuild, rebuild_organization};
use crate::error::{PipesError, Result};
use crate::fhir::Location;
use crate::fhir::location::strip_reference;
use crate::fhir::ndjson::{LineAccess, Output, is_gz};
use crate::profile::Profile;
use crate::report::Report;
use crate::snapshot::ORGANIZATIONS_FILE;
use crate::snapshot::index::index_by_id;
use crate::transform::{SNAPSHOT_FILE, write_report};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    FeatureCollection,
    FeatureLines,
    GeoParquet,
}

impl InputFormat {
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "geojson" | "json" => Ok(Self::FeatureCollection),
            "geojsonl" | "geojsons" => Ok(Self::FeatureLines),
            "parquet" => Ok(Self::GeoParquet),
            _ => Err(PipesError::Usage(format!(
                "--in {}: unknown extension; expected .geojson, .json, .geojsonl, .geojsons or .parquet",
                path.display()
            ))),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiffStats {
    pub changed: usize,
    pub unchanged: usize,
    pub created: usize,
    /// Resources written, by type.
    pub locations: usize,
    pub organizations: usize,
}

/// Resource id → (byte offset, length) of its line in a snapshot file.
type LineIndex = HashMap<String, (u64, usize)>;

struct Diff<'a> {
    index: LineIndex,
    access: LineAccess,
    /// The Organization side of the facility pairing, when the snapshot has it.
    organizations: Option<(LineIndex, LineAccess)>,
    out: Output,
    out_path: &'a Path,
    seen: HashSet<String>,
    stats: DiffStats,
    profile: Profile,
}

impl Diff<'_> {
    /// Refuse a row whose columns are neither built in nor in the profile:
    /// without the profile that declares them, their edits would be dropped.
    /// Columns whose value could not be read count too.
    fn check_columns(&self, row: &InputRow) -> Result<()> {
        let mut unknown: Vec<&str> = row
            .columns
            .keys()
            .chain(&row.unread)
            .map(String::as_str)
            .filter(|name| {
                !crate::diff::input::is_builtin_writable(name)
                    && !self.profile.column_names().any(|c| c == *name)
            })
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort();
        unknown.dedup();
        Err(PipesError::Usage(format!(
            "the edited file has column(s) {} that are neither built-in columns nor in the \
             profile ({}); pass the profile the dataset was made with (--profile), or remove \
             the column(s)",
            unknown.join(", "),
            self.profile.source
        )))
    }

    fn process(&mut self, row: InputRow, report: &mut Report) -> Result<()> {
        self.check_columns(&row)?;
        let (id, generated) = match row.id.clone() {
            Some(id) => (id, false),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        if !self.seen.insert(id.clone()) {
            report.add(
                "duplicate_id",
                &id,
                &format!("input row {} repeats an earlier row; skipped", row.line),
            );
            return Ok(());
        }
        let existing = self.index.get(&id).copied();
        let (base, snapshot) = match existing {
            Some((offset, len)) => {
                let text = self.access.read_at(offset, len)?;
                let base: Value = serde_json::from_str(&text)?;
                let loc = Location::parse(&base, &self.profile, &mut Report::default());
                (base, loc)
            }
            None => {
                if generated {
                    report.add(
                        "new_location_generated_id",
                        &id,
                        &format!("input row {} has no id; assigned {id}", row.line),
                    );
                } else {
                    report.add(
                        "new_location",
                        &id,
                        &format!(
                            "input row {}: id not in the snapshot; emitted as a create",
                            row.line
                        ),
                    );
                }
                (json!({"resourceType": "Location", "id": id}), None)
            }
        };
        let mut rebuilt = rebuild(&base, snapshot.as_ref(), &row, &self.profile, report);
        let is_create = existing.is_none();
        let mut emitted = 0usize;

        // The Organization half, when the snapshot has Organizations.
        if let Some((org_index, org_access)) = self.organizations.as_mut() {
            let org_ref = rebuilt
                .get("managingOrganization")
                .and_then(|m| m.get("reference"))
                .and_then(Value::as_str)
                .and_then(strip_reference);
            match org_ref {
                Some(org_id) => match org_index.get(&org_id) {
                    Some(&(offset, len)) => {
                        let text = org_access.read_at(offset, len)?;
                        let org_base: Value = serde_json::from_str(&text)?;
                        let org_new = rebuild_organization(
                            &org_base,
                            &row,
                            snapshot.as_ref(),
                            &self.profile,
                            report,
                        );
                        if canonical(&org_new) != canonical(&org_base) {
                            writeln!(self.out, "{org_new}")
                                .map_err(|e| PipesError::io(self.out_path, e))?;
                            self.stats.organizations += 1;
                            emitted += 1;
                        }
                    }
                    None => report.add(
                        "organization_missing",
                        &id,
                        &format!(
                            "managingOrganization {org_id} is not in organizations.ndjson; Organization not updated"
                        ),
                    ),
                },
                None if is_create
                    && self.profile.organization.facility_type.as_ref().is_some_and(|t| {
                        row.columns.get("type") == Some(&ColumnValue::Text(t.clone()))
                    }) =>
                {
                    let org_id = format!("{}{id}", self.profile.organization.id_prefix);
                    let org_new = new_organization(&org_id, &row, &self.profile, report);
                    writeln!(self.out, "{org_new}").map_err(|e| PipesError::io(self.out_path, e))?;
                    self.stats.organizations += 1;
                    emitted += 1;
                    if let Some(obj) = rebuilt.as_object_mut() {
                        obj.insert(
                            "managingOrganization".into(),
                            json!({"reference": format!("Organization/{org_id}")}),
                        );
                    }
                }
                None => {}
            }
        }

        if is_create || canonical(&rebuilt) != canonical(&base) {
            writeln!(self.out, "{rebuilt}").map_err(|e| PipesError::io(self.out_path, e))?;
            self.stats.locations += 1;
            emitted += 1;
        }
        if is_create {
            self.stats.created += 1;
        } else if emitted > 0 {
            self.stats.changed += 1;
        } else {
            self.stats.unchanged += 1;
        }
        Ok(())
    }
}

fn read_input(
    format: InputFormat,
    input: &Path,
    diff: &mut Diff,
    report: &mut Report,
) -> Result<()> {
    match format {
        InputFormat::FeatureCollection => read_feature_collection(input, |f, i| {
            let row = feature_to_row(f, i, report);
            diff.process(row, report)
        }),
        InputFormat::FeatureLines => read_feature_lines(input, |f, i| {
            let row = feature_to_row(f, i, report);
            diff.process(row, report)
        }),
        InputFormat::GeoParquet => {
            read_geoparquet(input, report, |row, report| diff.process(row, report))
        }
    }
}

fn tmp_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Flush, fsync and rename the finished changes file into place.
fn finish(out: Output, tmp: &Path, dest: &Path) -> Result<()> {
    let file = out.finish().map_err(|e| PipesError::io(tmp, e))?;
    file.sync_all().map_err(|e| PipesError::io(tmp, e))?;
    std::fs::rename(tmp, dest).map_err(|e| PipesError::io(dest, e))
}

pub fn run_diff(args: &DiffArgs, profile: Option<&str>) -> Result<()> {
    let ndjson = args.snapshot.join(SNAPSHOT_FILE);
    if !ndjson.is_file() {
        return Err(PipesError::Usage(format!(
            "snapshot file not found: {}",
            ndjson.display()
        )));
    }
    let format = InputFormat::from_path(&args.input)?;
    if !args.input.is_file() {
        return Err(PipesError::Usage(format!(
            "--in {}: not a file",
            args.input.display()
        )));
    }
    if args.out.is_dir() {
        return Err(PipesError::Usage(format!(
            "--out {}: is a directory",
            args.out.display()
        )));
    }

    let profile = Profile::for_snapshot(profile, &args.snapshot)?;
    eprintln!("profile: {} ({})", profile.name, profile.source);
    let mut report = Report::default();
    let index = index_by_id(&ndjson, &mut report, "snapshot_line_unparsed")?;
    eprintln!("indexed {} snapshot resources", index.len());
    let org_path = args.snapshot.join(ORGANIZATIONS_FILE);
    let organizations = if org_path.is_file() {
        let org_index = index_by_id(&org_path, &mut report, "organization_line_unparsed")?;
        eprintln!("indexed {} organizations", org_index.len());
        Some((org_index, LineAccess::open(&org_path)?))
    } else {
        eprintln!(
            "no organizations.ndjson in the snapshot: organisation columns are ignored and no Organizations are written"
        );
        None
    };

    let tmp = tmp_path(&args.out);
    let out = Output::create(&tmp, is_gz(&args.out))
        .map_err(|e| PipesError::Usage(format!("--out {}: {e}", tmp.display())))?;
    let mut diff = Diff {
        index,
        access: LineAccess::open(&ndjson)?,
        organizations,
        out,
        out_path: &tmp,
        seen: HashSet::new(),
        stats: DiffStats::default(),
        profile,
    };
    let read = read_input(format, &args.input, &mut diff, &mut report);
    let Diff { out, stats, .. } = diff;
    if let Err(e) = read.and_then(|()| finish(out, &tmp, &args.out)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    println!(
        "{} changed, {} unchanged, {} new -> {}",
        stats.changed,
        stats.unchanged,
        stats.created,
        args.out.display()
    );
    println!(
        "Location: {}, Organization: {}",
        stats.locations, stats.organizations
    );
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    println!("{}", report.summary());
    Ok(())
}
