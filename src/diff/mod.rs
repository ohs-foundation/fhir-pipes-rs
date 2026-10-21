//! `fhir-pipes diff`: edited GeoJSON or GeoParquet -> FHIR NDJSON of the Locations
//! that changed. Offline. The snapshot is indexed by id and byte offset;
//! the input is streamed; each named resource is re-read, rebuilt from the
//! writable columns, and emitted only when it differs (object key order
//! aside; see `compare::differs`).

pub mod compare;
pub mod geojson;
pub mod input;
pub mod organization;
pub mod parquet;
pub mod rebuild;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

use crate::cli::DiffArgs;
use crate::diff::compare::differs;
use crate::diff::geojson::{feature_to_row, read_feature_collection, read_feature_lines};
use crate::diff::input::{ColumnValue, InputRow};
use crate::diff::organization::{new_organization, rebuild_organization};
use crate::diff::parquet::read_geoparquet;
use crate::diff::rebuild::rebuild;
use crate::error::{PipesError, Result};
use crate::fhir::Location;
use crate::fhir::location::strip_reference;
use crate::fhir::ndjson::{LineAccess, Output, is_gz};
use crate::fs::AtomicFile;
use crate::profile::Profile;
use crate::report::{Report, write_report};
use crate::snapshot::ORGANIZATIONS_FILE;
use crate::snapshot::index::index_by_id;
use crate::transform::SNAPSHOT_FILE;

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
    out: Output<AtomicFile>,
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
        let profile = if self.profile.is_base() {
            "no profile was given".to_string()
        } else {
            format!("the profile is {}", self.profile.source)
        };
        Err(PipesError::Usage(format!(
            "the edited file has column(s) {} that are neither built-in columns nor in the \
             profile ({profile}); pass the profile the dataset was made with (--profile), or \
             remove the column(s)",
            unknown.join(", "),
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
                        if differs(&org_new, &org_base) {
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
                            crate::fhir::location::reference("Organization", &org_id),
                        );
                    }
                }
                None => {}
            }
        }

        if is_create || differs(&rebuilt, &base) {
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

/// Flush, fsync and rename the finished changes file into place.
fn finish(out: Output<AtomicFile>, tmp: &Path) -> Result<()> {
    out.finish().map_err(|e| PipesError::io(tmp, e))?.commit()
}

pub fn run_diff(args: &DiffArgs, profile: Option<&str>) -> anyhow::Result<()> {
    let ndjson = args.snapshot.join(SNAPSHOT_FILE);
    if !ndjson.is_file() {
        return Err(
            PipesError::Usage(format!("snapshot file not found: {}", ndjson.display())).into(),
        );
    }
    let format = InputFormat::from_path(&args.input)?;
    if !args.input.is_file() {
        return Err(PipesError::Usage(format!("--in {}: not a file", args.input.display())).into());
    }
    crate::fs::check_out(&args.out)?;

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

    let file =
        AtomicFile::create(&args.out).map_err(|e| PipesError::Usage(format!("--out {e}")))?;
    let tmp = file.tmp().to_path_buf();
    let out = Output::new(file, is_gz(&args.out));
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
    read.and_then(|()| finish(out, &tmp))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::diff::rebuild::tests::{icr, row, text};

    fn location(id: &str, name: &str) -> Value {
        json!({"resourceType": "Location", "id": id, "name": name, "status": "active"})
    }

    /// A row for Location `id` with `cols`.
    fn row_for(id: Option<&str>, cols: &[(&str, ColumnValue)]) -> InputRow {
        InputRow {
            id: id.map(str::to_string),
            ..row(cols)
        }
    }

    /// What diff writes and counts for `rows` against a snapshot of
    /// `locations` (and of `organizations`, when given).
    struct Run {
        written: Vec<Value>,
        stats: DiffStats,
        report: Report,
        result: Result<()>,
    }

    fn run(
        locations: &[Value],
        organizations: Option<&[Value]>,
        profile: Profile,
        rows: Vec<InputRow>,
    ) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, resources: &[Value]| {
            let path = dir.path().join(name);
            let text: String = resources.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(&path, text).unwrap();
            path
        };
        let mut report = Report::default();
        let ndjson = write("locations.ndjson", locations);
        let organizations = organizations.map(|orgs| {
            let path = write("organizations.ndjson", orgs);
            let index = index_by_id(&path, &mut report, "organization_line_unparsed").unwrap();
            (index, LineAccess::open(&path).unwrap())
        });
        let out_path = dir.path().join("changes.ndjson");
        let file = AtomicFile::create(&out_path).unwrap();
        let tmp = file.tmp().to_path_buf();
        let mut diff = Diff {
            index: index_by_id(&ndjson, &mut report, "snapshot_line_unparsed").unwrap(),
            access: LineAccess::open(&ndjson).unwrap(),
            organizations,
            out: Output::new(file, false),
            out_path: &tmp,
            seen: HashSet::new(),
            stats: DiffStats::default(),
            profile,
        };
        let result = rows
            .into_iter()
            .try_for_each(|r| diff.process(r, &mut report));
        let Diff { out, stats, .. } = diff;
        finish(out, &tmp).unwrap();
        let written = std::fs::read_to_string(&out_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        Run {
            written,
            stats,
            report,
            result,
        }
    }

    #[test]
    fn only_changed_and_new_locations_are_written() {
        let r = run(
            &[location("a", "A"), location("b", "B")],
            None,
            Profile::base(),
            vec![
                row_for(Some("a"), &[("name", text("A"))]),
                row_for(Some("b"), &[("name", text("B2"))]),
                row_for(Some("c"), &[("name", text("C"))]),
                row_for(Some("a"), &[("name", text("A3"))]),
            ],
        );
        r.result.unwrap();
        assert_eq!(
            r.stats,
            DiffStats {
                changed: 1,
                unchanged: 1,
                created: 1,
                locations: 2,
                organizations: 0,
            }
        );
        let written: Vec<(&str, &str)> = r
            .written
            .iter()
            .map(|v| (v["id"].as_str().unwrap(), v["name"].as_str().unwrap()))
            .collect();
        // The second row for a is skipped, not applied.
        assert_eq!(written, [("b", "B2"), ("c", "C")]);
        assert_eq!(r.report.count("duplicate_id"), 1);
        assert_eq!(r.report.count("new_location"), 1);
    }

    #[test]
    fn a_row_without_an_id_is_a_create_with_a_new_id() {
        let r = run(
            &[location("a", "A")],
            None,
            Profile::base(),
            vec![row_for(None, &[("name", text("New"))])],
        );
        r.result.unwrap();
        assert_eq!(r.stats.created, 1);
        assert_eq!(r.report.count("new_location_generated_id"), 1);
        let id = r.written[0]["id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{id}");
        assert_eq!(r.written[0]["resourceType"], "Location");
    }

    #[test]
    fn a_column_not_built_in_or_in_the_profile_stops_the_diff() {
        let mut unread = row_for(Some("a"), &[]);
        unread.unread.push("colour".into());
        for (profile, rows, says) in [
            (
                Profile::base(),
                vec![row_for(Some("a"), &[("colour", text("red"))])],
                "no profile was given",
            ),
            (
                icr(),
                vec![unread],
                "the profile is examples/profiles/icr.toml",
            ),
        ] {
            let r = run(&[location("a", "A")], None, profile, rows);
            let err = r.result.unwrap_err().to_string();
            assert!(err.contains("column(s) colour "), "{err}");
            assert!(err.contains(says), "{err}");
            assert!(r.written.is_empty());
        }
        // A column the profile declares is not refused.
        let r = run(
            &[location("a", "A")],
            None,
            icr(),
            vec![row_for(
                Some("a"),
                &[("settlement_type", ColumnValue::Null)],
            )],
        );
        r.result.unwrap();
    }

    #[test]
    fn a_new_facility_gets_a_new_organization() {
        let r = run(
            &[location("a", "A")],
            Some(&[]),
            icr(),
            vec![
                row_for(
                    Some("f"),
                    &[("name", text("F")), ("type", text("facility"))],
                ),
                row_for(
                    Some("s"),
                    &[("name", text("S")), ("type", text("settlement"))],
                ),
            ],
        );
        r.result.unwrap();
        assert_eq!(r.stats.created, 2);
        assert_eq!(r.stats.organizations, 1);
        assert_eq!(r.stats.locations, 2);
        let org = &r.written[0];
        assert_eq!(org["resourceType"], "Organization");
        assert_eq!(org["id"], "org-f");
        let f = &r.written[1];
        assert_eq!(f["managingOrganization"]["reference"], "Organization/org-f");
        assert!(r.written[2].get("managingOrganization").is_none());
    }

    #[test]
    fn a_managing_organization_missing_from_the_snapshot_is_reported() {
        let mut a = location("a", "A");
        a["managingOrganization"] = json!({"reference": "Organization/gone"});
        let r = run(
            &[a],
            Some(&[]),
            icr(),
            vec![row_for(Some("a"), &[("name", text("A"))])],
        );
        r.result.unwrap();
        assert_eq!(r.report.count("organization_missing"), 1);
        assert_eq!(r.stats.unchanged, 1);
        assert!(r.written.is_empty());
    }

    #[test]
    fn the_input_format_comes_from_the_extension() {
        for (name, format) in [
            ("a.geojson", InputFormat::FeatureCollection),
            ("a.JSON", InputFormat::FeatureCollection),
            ("a.geojsonl", InputFormat::FeatureLines),
            ("a.geojsons", InputFormat::FeatureLines),
            ("a.parquet", InputFormat::GeoParquet),
        ] {
            assert_eq!(InputFormat::from_path(Path::new(name)).unwrap(), format);
        }
        for name in ["a.csv", "a"] {
            assert!(matches!(
                InputFormat::from_path(Path::new(name)),
                Err(PipesError::Usage(_))
            ));
        }
    }
}
