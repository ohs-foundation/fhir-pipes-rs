//! Pass two: walk the index's output order, seek each line, build rows, write
//! one file per partition into a staging directory, then swap it in atomically.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use geo::Intersects;

use crate::error::{PipesError, Result};
use crate::fhir::ndjson::LineAccess;
use crate::fhir::{Location, Organization};
use crate::geometry::{self, GeomKind, GeometryResult};
use crate::index::hierarchy::Hierarchy;
use crate::index::partition::{Claims, PartitionKey, segment};
use crate::index::{Index, IndexRecord};
use crate::profile::Profile;
use crate::report::Report;
use crate::snapshot::index::index_by_id;
use crate::write::parquet::PartitionWriter;
use crate::write::schema::{OutputRow, RowBatch, output_schema, output_schema_optional_geometry};

pub const DATASET_DIR: &str = "locations";
const STAGING_DIR: &str = ".locations.tmp";
const BACKUP_DIR: &str = ".locations.bak";
const MIN_PARTITION_ROWS: usize = 100;
const PARENT_CACHE_MAX: usize = 256;
const ROW_VANISHED_DETAIL: &str = "row indexed in pass one could not be rebuilt in pass two";

#[derive(Debug)]
pub struct WrittenPartition {
    pub path: PathBuf,
    /// `key=value/key=value` as written under the dataset directory.
    pub partition: String,
    pub rows: usize,
    /// Rows written with null geometry (only with --keep-without-geometry).
    pub rows_without_geometry: usize,
}

/// Re-read and parse one record's line. Diagnostics were reported in pass
/// one; this parses with a throwaway report so nothing is counted twice.
/// `fhir_json` is built only `with_json`: a parent polygon needs just the
/// geometry.
fn load_location(
    lines: &mut LineAccess,
    rec: &IndexRecord,
    profile: &Profile,
    with_json: bool,
) -> Result<Option<Location>> {
    let text = lines.read_at(rec.offset, rec.len)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let mut silent = Report::default();
    Ok(if with_json {
        Location::parse_owned(value, profile, &mut silent)
    } else {
        Location::parse(&value, profile, &mut silent)
    })
}

/// Bounded cache of parent polygons for the point-outside-parent check,
/// keyed by record index.
struct ParentCache {
    polygons: HashMap<u32, Option<geo::Geometry<f64>>>,
}

impl ParentCache {
    fn get(
        &mut self,
        idx: u32,
        records: &[IndexRecord],
        lines: &mut LineAccess,
        profile: &Profile,
    ) -> Result<Option<&geo::Geometry<f64>>> {
        if !self.polygons.contains_key(&idx) {
            // Eviction is wholesale rather than LRU: access is spatially
            // clustered (an admin ancestor is shared by all its children,
            // visited consecutively in Hilbert order), so by the time the
            // cache fills up the entries most likely to be reused soon are
            // already the most recently inserted ones -- a full clear
            // rarely throws away something about to be needed again. The
            // pathological case is a single partition whose rows cycle
            // through more than PARENT_CACHE_MAX distinct polygon parents,
            // which would thrash (repeatedly re-reading and re-parsing
            // parents); that trades some CPU for bounded memory.
            if self.polygons.len() >= PARENT_CACHE_MAX {
                self.polygons.clear();
            }
            let mut silent = Report::default();
            let geom = load_location(lines, &records[idx as usize], profile, false)?
                .and_then(|loc| geometry::build(&loc, &mut silent))
                .filter(|g| g.kind == GeomKind::Polygon)
                .map(|g| g.geometry);
            self.polygons.insert(idx, geom);
        }
        Ok(self.polygons.get(&idx).unwrap().as_ref())
    }
}

/// The nearest ancestor (walking parent links) that has a polygon geometry.
fn nearest_polygon_ancestor(
    hierarchy: &Hierarchy,
    records: &[IndexRecord],
    i: usize,
) -> Option<u32> {
    let mut cur = hierarchy.get(i)?.parent;
    while let Some(p) = cur {
        let pi = p.index();
        if records[pi].geometry.map(|g| g.kind) == Some(GeomKind::Polygon) {
            return Some(pi as u32);
        }
        cur = hierarchy.get(pi)?.parent;
    }
    None
}

pub fn write_dataset(
    ndjson: &Path,
    profile: &Profile,
    index: &mut Index,
    out_dir: &Path,
    keys: &[PartitionKey],
    row_group_size: usize,
    organizations: Option<&Path>,
) -> Result<Vec<WrittenPartition>> {
    std::fs::create_dir_all(out_dir).map_err(|e| PipesError::io(out_dir, e))?;
    let dataset = out_dir.join(DATASET_DIR);
    let backup = out_dir.join(BACKUP_DIR);
    let staging = out_dir.join(STAGING_DIR);
    if dataset.is_symlink() {
        return Err(PipesError::Usage(format!(
            "{} is a symlink; fhir-pipes will not replace it",
            dataset.display()
        )));
    }
    crate::fs::recover_swap(&dataset, &backup)?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| PipesError::io(&staging, e))?;
    }
    std::fs::create_dir(&staging).map_err(|e| PipesError::io(&staging, e))?;

    let written = match write_partitions(
        ndjson,
        profile,
        index,
        &staging,
        keys,
        row_group_size,
        organizations,
    ) {
        Ok(w) => w,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    crate::fs::swap_dir(&staging, &dataset, &backup)?;
    Ok(written
        .into_iter()
        .map(|w| WrittenPartition {
            path: dataset.join(&w.partition).join("part-0.parquet"),
            ..w
        })
        .collect())
}

fn write_partitions(
    ndjson: &Path,
    profile: &Profile,
    index: &mut Index,
    staging: &Path,
    keys: &[PartitionKey],
    row_group_size: usize,
    organizations: Option<&Path>,
) -> Result<Vec<WrittenPartition>> {
    let Index {
        records,
        hierarchy,
        order,
        report,
        keep_without_geometry,
        ..
    } = index;
    let schema = if *keep_without_geometry {
        output_schema_optional_geometry(profile)
    } else {
        output_schema(profile)
    };
    let groups = group_by_partition(records, order, keys);
    let mut source = RowSource {
        records,
        hierarchy,
        profile,
        lines: LineAccess::open(ndjson)?,
        // The Organization side of the facility pairing, when the snapshot
        // has it. Indexed by id; each row's Organization is read on demand.
        organizations: match organizations {
            Some(path) => Some(Organizations {
                index: index_by_id(path, report, "organization_line_unparsed")?,
                lines: LineAccess::open(path)?,
            }),
            None => None,
        },
        parents: ParentCache {
            polygons: HashMap::new(),
        },
    };
    // One claims table per (parent path, key): a segment only needs to be
    // unique among siblings in the same directory.
    let mut claims: HashMap<(Vec<String>, &'static str), Claims> = HashMap::new();
    let mut written = Vec::new();

    for (values, idxs) in &groups {
        let mut segments: Vec<String> = Vec::new();
        for (key, value) in keys.iter().zip(values) {
            let ctx = (segments.clone(), key.name());
            let seg = segment(key.name(), value, claims.entry(ctx).or_default(), report);
            segments.push(format!("{}={seg}", key.name()));
        }
        let partition = segments.join("/");
        if idxs.len() < MIN_PARTITION_ROWS {
            report.add(
                "small_partition",
                &partition,
                &format!(
                    "{} rows is below MIN_PARTITION_ROWS={MIN_PARTITION_ROWS}",
                    idxs.len()
                ),
            );
        }
        let path = segments
            .iter()
            .fold(staging.to_path_buf(), |p, s| p.join(s))
            .join("part-0.parquet");
        let mut writer =
            PartitionWriter::create_with_schema(&path, row_group_size, schema.clone())?;
        let mut batch = RowBatch::for_profile(profile, *keep_without_geometry);
        let mut geometry_types: Vec<String> = Vec::new();
        let mut rows_without_geometry = 0usize;

        for &i in idxs {
            let i = i as usize;
            let Some((loc, geom)) = source.location(i, report)? else {
                continue;
            };
            let org = source.organization(i, report)?;
            source.check(i, &loc, org.as_ref(), geom.as_ref(), report)?;
            match &geom {
                Some(geom) => {
                    let type_name = crate::geometry::kind_name(&geom.geometry);
                    if !geometry_types.iter().any(|t| t == type_name) {
                        geometry_types.push(type_name.to_string());
                    }
                }
                None => rows_without_geometry += 1,
            }
            batch.push(&source.row(i, loc, org, geom));
            if batch.rows >= row_group_size {
                writer.write(&mut batch)?;
            }
        }
        writer.write(&mut batch)?;
        let stats = writer.finish(&geometry_types)?;
        eprintln!(
            "  {partition}: {} row group{}",
            stats.row_groups,
            if stats.row_groups == 1 { "" } else { "s" }
        );
        written.push(WrittenPartition {
            path,
            partition,
            rows: stats.rows,
            rows_without_geometry,
        });
    }

    Ok(written)
}

/// The records grouped by their partition values, preserving the index's
/// (country, geom_type, Hilbert) order within each group so every
/// partition stays spatially clustered whatever keys were chosen.
fn group_by_partition(
    records: &[IndexRecord],
    order: &[u32],
    keys: &[PartitionKey],
) -> Vec<(Vec<String>, Vec<u32>)> {
    let mut groups: Vec<(Vec<String>, Vec<u32>)> = Vec::new();
    let mut group_index: HashMap<Vec<String>, usize> = HashMap::new();
    for &i in order {
        let rec = &records[i as usize];
        let values: Vec<String> = keys.iter().map(|k| k.value(rec).to_string()).collect();
        match group_index.get(&values) {
            Some(&gi) => groups[gi].1.push(i),
            None => {
                group_index.insert(values.clone(), groups.len());
                groups.push((values, vec![i]));
            }
        }
    }
    groups
}

/// What pass two builds rows from: the index, the snapshot's lines, and its
/// Organizations when it has them.
struct RowSource<'a> {
    records: &'a [IndexRecord],
    hierarchy: &'a Hierarchy,
    profile: &'a Profile,
    lines: LineAccess,
    organizations: Option<Organizations>,
    parents: ParentCache,
}

/// The snapshot's Organizations: where each one's line is, by id, and the
/// lines.
struct Organizations {
    index: HashMap<String, (u64, usize)>,
    lines: LineAccess,
}

impl RowSource<'_> {
    /// Record `i`'s Location and geometry, read again, or `None` after
    /// reporting a row that pass one indexed but that cannot be rebuilt.
    fn location(
        &mut self,
        i: usize,
        report: &mut Report,
    ) -> Result<Option<(Location, Option<GeometryResult>)>> {
        let rec = &self.records[i];
        let Some(loc) = load_location(&mut self.lines, rec, self.profile, true)? else {
            report.add("row_vanished", &rec.id, ROW_VANISHED_DETAIL);
            return Ok(None);
        };
        // Pass one kept a row without geometry only when asked to
        // (--keep-without-geometry); a row it gave geometry must still
        // have it.
        let geom = match rec.geometry {
            None => None,
            Some(_) => {
                let mut silent = Report::default();
                let Some(geom) = geometry::build(&loc, &mut silent) else {
                    report.add("row_vanished", &rec.id, ROW_VANISHED_DETAIL);
                    return Ok(None);
                };
                Some(geom)
            }
        };
        if self.hierarchy.get(i).is_none() {
            report.add("row_vanished", &rec.id, ROW_VANISHED_DETAIL);
            return Ok(None);
        }
        Ok(Some((loc, geom)))
    }

    /// The Organization record `i`'s Location names as its
    /// managingOrganization, when the snapshot has Organizations; one it
    /// does not have is reported.
    fn organization(&mut self, i: usize, report: &mut Report) -> Result<Option<Organization>> {
        let rec = &self.records[i];
        Ok(
            match (&rec.managing_organization, self.organizations.as_mut()) {
                (Some(org_id), Some(orgs)) => match orgs.index.get(org_id) {
                    Some(&(offset, len)) => {
                        let text = orgs.lines.read_at(offset, len)?;
                        let value: serde_json::Value = serde_json::from_str(&text)?;
                        Organization::parse(&value, self.profile, &mut Report::default())
                    }
                    None => {
                        report.add(
                            "organization_missing",
                            &rec.id,
                            &format!(
                                "managingOrganization {org_id} is not in organizations.ndjson"
                            ),
                        );
                        None
                    }
                },
                _ => None,
            },
        )
    }

    /// Reports a Location named otherwise than its Organization, and a
    /// point that falls outside its nearest admin ancestor's polygon.
    fn check(
        &mut self,
        i: usize,
        loc: &Location,
        org: Option<&Organization>,
        geom: Option<&GeometryResult>,
        report: &mut Report,
    ) -> Result<()> {
        let rec = &self.records[i];
        if let Some(o) = org
            && o.name.is_some()
            && o.name != loc.name
        {
            report.add(
                "organization_name_mismatch",
                &rec.id,
                &format!(
                    "Location is named {:?} but Organization {} is named {:?}",
                    loc.name.as_deref().unwrap_or(""),
                    o.id,
                    o.name.as_deref().unwrap_or("")
                ),
            );
        }
        if let (Some(geom), Some(parent_idx)) = (
            geom,
            nearest_polygon_ancestor(self.hierarchy, self.records, i),
        ) && let Some(parent) =
            self.parents
                .get(parent_idx, self.records, &mut self.lines, self.profile)?
        {
            let pt = geo::Point::new(geom.lon, geom.lat);
            if !parent.intersects(&pt) {
                report.add(
                    "point_outside_parent",
                    &rec.id,
                    &format!(
                        "({}, {}) falls outside its nearest admin ancestor",
                        geom.lon, geom.lat
                    ),
                );
            }
        }
        Ok(())
    }

    /// Record `i`'s output row. `location` has checked that the record has
    /// its place in the hierarchy.
    fn row(
        &self,
        i: usize,
        loc: Location,
        org: Option<Organization>,
        geom: Option<GeometryResult>,
    ) -> OutputRow {
        let (records, hierarchy) = (self.records, self.hierarchy);
        let rec = &records[i];
        let info = hierarchy.get(i).expect("checked by location");
        // Spatial cells are derived from the position; a row without
        // geometry leaves them null even if the resource carries some.
        let (quadkey, quadkey_level) = geom
            .as_ref()
            .and_then(|_| crate::fhir::spatial::deepest_quadkey(&loc.spatial_cells))
            .map(|c| (Some(c.cell.clone()), i32::try_from(c.level).ok()))
            .unwrap_or((None, None));
        let (organization_identifier, organization_columns, organization_json) = match org {
            Some(o) => (
                o.identifier
                    .into_iter()
                    .map(|i| (i.system, i.value))
                    .collect(),
                o.columns,
                Some(o.fhir_json),
            ),
            None => (Vec::new(), Vec::new(), None),
        };
        OutputRow {
            id: loc.id,
            version_id: loc.version_id.clone(),
            last_updated: loc.last_updated.clone(),
            name: loc.name,
            alias: loc.alias,
            status: loc.status,
            description: loc.description,
            type_code: loc.type_code,
            physical_type: loc.physical_type,
            part_of: loc.part_of,
            managing_organization: loc.managing_organization,
            identifier: loc
                .identifier
                .into_iter()
                .map(|ident| (ident.system, ident.value))
                .collect(),
            position: loc.position,
            location_columns: loc.columns,
            quadkey,
            quadkey_level,
            organization_identifier,
            organization_columns,
            depth: i32::from(info.depth),
            admin_level: info.admin_level.map(i32::from),
            tier: rec.tier.clone(),
            path: hierarchy.path(records, i),
            ancestor_ids: hierarchy.ancestor_ids(records, i),
            admin_names: hierarchy.admin_names(records, i),
            admin_codes: hierarchy.admin_codes(records, i),
            overlays_admin_unit_ids: loc.overlays_admin_unit_ids,
            organization_json,
            country: rec.country.clone(),
            geom_type: geom.as_ref().map(|g| g.kind.as_str().to_string()),
            lon: geom.as_ref().map(|g| g.lon),
            lat: geom.as_ref().map(|g| g.lat),
            wkb: geom.as_ref().map(|g| g.wkb()).unwrap_or_default(),
            bbox: geom.as_ref().map(|g| g.bbox),
            fhir_json: loc.fhir_json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::index::partition::parse_keys;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson")
    }

    /// Recursively snapshot every file under `dir` as `(path, bytes)`, for
    /// byte-identity comparisons across a failed rerun.
    fn snapshot_files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        if !dir.is_dir() {
            return out;
        }
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(snapshot_files(&path));
            } else {
                out.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
        out
    }

    #[test]
    fn writes_one_file_per_partition_and_swaps_atomically() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        let written = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        let rel: Vec<String> = written
            .iter()
            .map(|w| {
                w.path
                    .strip_prefix(out.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            rel.contains(&"locations/country=NG/geom_type=polygon/part-0.parquet".to_string()),
            "{rel:?}"
        );
        assert!(
            rel.contains(&"locations/country=NG/geom_type=point/part-0.parquet".to_string()),
            "{rel:?}"
        );
        assert!(
            rel.contains(&"locations/country=unknown/geom_type=point/part-0.parquet".to_string()),
            "{rel:?}"
        );
        assert_eq!(written.iter().map(|w| w.rows).sum::<usize>(), 8);
        assert_eq!(
            index.report.count("point_outside_parent"),
            1,
            "stray is outside nassarawa"
        );
        assert_eq!(index.report.count("small_partition"), 3);
        assert!(!out.path().join(".locations.tmp").exists());
        assert!(!out.path().join(".locations.bak").exists());

        // Second run replaces the dataset and leaves no backup behind.
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        assert!(!out.path().join(".locations.bak").exists());
        assert!(
            out.path()
                .join("locations/country=NG/geom_type=polygon/part-0.parquet")
                .exists()
        );
    }

    #[test]
    fn keep_without_geometry_writes_ghost_and_remote_with_null_geometry() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, true).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        let written = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        assert_eq!(written.iter().map(|w| w.rows).sum::<usize>(), 10);
        let null = written
            .iter()
            .find(|w| w.partition == "country=NG/geom_type=null")
            .unwrap();
        assert_eq!((null.rows, null.rows_without_geometry), (2, 2));
        assert_eq!(
            written
                .iter()
                .map(|w| w.rows_without_geometry)
                .sum::<usize>(),
            2
        );
        // The same point_outside_parent as without the flag: rows without
        // geometry are not checked against their parent.
        assert_eq!(index.report.count("point_outside_parent"), 1);
        assert_eq!(index.report.count("row_vanished"), 0);
    }

    #[test]
    fn rows_without_geometry_come_last_in_a_shared_partition() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, true).unwrap();
        let keys = parse_keys("tier").unwrap();
        let written = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            10,
            None,
        )
        .unwrap();
        let tier2 = written.iter().find(|w| w.partition == "tier=2").unwrap();
        assert_eq!((tier2.rows, tier2.rows_without_geometry), (3, 1));
        let file = std::fs::File::open(&tier2.path).unwrap();
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let mut ids = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let col = batch.column_by_name("id").unwrap();
            let col = col
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .unwrap();
            ids.extend(col.iter().map(|v| v.unwrap().to_string()));
        }
        assert_eq!(ids.last().map(String::as_str), Some("ghost"), "{ids:?}");
    }

    #[test]
    fn partition_by_tier_groups_non_consecutive_rows() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("tier").unwrap();
        let written = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        let mut parts: Vec<&str> = written.iter().map(|w| w.partition.as_str()).collect();
        parts.sort();
        assert_eq!(parts, vec!["tier=0", "tier=1", "tier=2", "tier=site"]);
        assert_eq!(
            written
                .iter()
                .find(|w| w.partition == "tier=2")
                .unwrap()
                .rows,
            2,
            "nassarawa and dup"
        );
    }

    #[test]
    fn empty_index_writes_an_empty_dataset_dir() {
        let out = tempfile::tempdir().unwrap();
        let snapshot = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            snapshot.path(),
            b"{\"resourceType\":\"Location\",\"id\":\"x\"}\n",
        )
        .unwrap();
        let mut index =
            build_index(snapshot.path(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        let written = write_dataset(
            snapshot.path(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        assert!(written.is_empty());
        assert!(out.path().join("locations").is_dir());
    }

    #[test]
    fn a_leftover_backup_from_a_crashed_swap_is_recovered() {
        let out = tempfile::tempdir().unwrap();
        // Simulate: live dataset missing, backup present (crash between the two renames).
        let backup = out.path().join(".locations.bak");
        std::fs::create_dir_all(backup.join("country=NG")).unwrap();
        std::fs::write(backup.join("country=NG/marker"), b"old").unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        assert!(!backup.exists());
        assert!(
            !out.path().join("locations/country=NG/marker").exists(),
            "the recovered old dataset was then replaced by the new one"
        );
        assert!(
            out.path()
                .join("locations/country=NG/geom_type=polygon/part-0.parquet")
                .exists()
        );
    }

    #[test]
    fn mid_write_failure_leaves_the_live_dataset_untouched() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();
        let before = snapshot_files(&out.path().join("locations"));
        assert!(!before.is_empty());

        // A record whose recorded offset is now nonsense (as if the ndjson
        // had shrunk out from under it): the seek succeeds but the read
        // fails, so the whole write must abort partway through.
        let mut broken =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let last = *broken.order.last().unwrap();
        broken.records[last as usize].offset = 1 << 40;
        let err = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut broken,
            out.path(),
            &keys,
            3,
            None,
        );
        assert!(err.is_err(), "{err:?}");

        assert!(!out.path().join(".locations.tmp").exists());
        assert!(!out.path().join(".locations.bak").exists());
        let after = snapshot_files(&out.path().join("locations"));
        assert_eq!(
            before, after,
            "live dataset must be byte-identical after a failed rerun"
        );
    }

    #[test]
    fn a_rerun_with_different_keys_leaves_only_its_own_files() {
        let out = tempfile::tempdir().unwrap();
        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();

        let mut index2 =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let tier_keys = parse_keys("tier").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index2,
            out.path(),
            &tier_keys,
            3,
            None,
        )
        .unwrap();

        let dataset = out.path().join("locations");
        let mut saw_any = false;
        for entry in std::fs::read_dir(&dataset).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            saw_any = true;
            assert!(
                name.starts_with("tier="),
                "leftover from the previous partitioning scheme: {name}"
            );
        }
        assert!(saw_any);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dataset_dir_is_refused() {
        use std::os::unix::fs::symlink;
        let out = tempfile::tempdir().unwrap();
        let real = out.path().join("real-locations");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, out.path().join("locations")).unwrap();

        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        let err = write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn stale_staging_is_discarded() {
        let out = tempfile::tempdir().unwrap();
        let staging = out.path().join(".locations.tmp");
        std::fs::create_dir_all(staging.join("country=STALE")).unwrap();
        std::fs::write(staging.join("country=STALE/junk"), b"stale").unwrap();

        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();

        assert!(!out.path().join("locations/country=STALE").exists());
        assert!(!staging.exists());
    }

    #[test]
    fn backup_alongside_a_live_dataset_is_removed() {
        let out = tempfile::tempdir().unwrap();
        let dataset = out.path().join("locations");
        std::fs::create_dir_all(dataset.join("country=OLD")).unwrap();
        std::fs::write(dataset.join("country=OLD/marker"), b"live").unwrap();
        let backup = out.path().join(".locations.bak");
        std::fs::create_dir_all(backup.join("country=OLDER")).unwrap();
        std::fs::write(backup.join("country=OLDER/marker"), b"backup").unwrap();

        let mut index =
            build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        let keys = parse_keys("country,geom_type").unwrap();
        write_dataset(
            &fixture(),
            &crate::profile::icr_example(),
            &mut index,
            out.path(),
            &keys,
            3,
            None,
        )
        .unwrap();

        assert!(!backup.exists());
        assert!(!dataset.join("country=OLD").exists());
        assert!(
            dataset
                .join("country=NG/geom_type=polygon/part-0.parquet")
                .exists()
        );
    }
}
