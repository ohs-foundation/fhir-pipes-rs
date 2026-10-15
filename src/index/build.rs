//! Pass one: stream the snapshot, keep an IndexRecord per Location, resolve
//! the hierarchy, choose country and tier, and produce the output order
//! (retained records sorted by partition then Hilbert key).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::fhir::ndjson::NdjsonReader;
use crate::fhir::{Boundary, Location};
use crate::geometry::{self, GeometrySummary};
use crate::index::IndexRecord;
use crate::index::hierarchy::{Hierarchy, resolve_hierarchy};
use crate::index::hilbert::hilbert_key;
use crate::report::Report;

pub const UNKNOWN_COUNTRY: &str = "unknown";

/// A fixed WGS84 extent for the Hilbert key, rather than the dataset's own
/// bbox: keys stay deterministic across snapshots and are not skewed by a
/// single outlier point far from the rest of the data. At ORDER 16 this
/// gives roughly 5.5 km cells at the equator -- ample for clustering rows
/// within a partition.
pub const HILBERT_EXTENT: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];

pub struct Index {
    /// Every parsed resource, in file order. The hierarchy is indexed by position here.
    pub records: Vec<IndexRecord>,
    pub hierarchy: Hierarchy,
    /// Indices into `records` of the rows that will be written, in output order.
    pub order: Vec<u32>,
    pub report: Report,
    /// Resources read from the file, including ones that failed to parse.
    pub read: usize,
    /// Whether `order` may hold rows without geometry; the writer then uses
    /// the schema whose geometry columns are nullable.
    pub keep_without_geometry: bool,
}

/// Issue kinds pass one reports when a Location ends up with no geometry.
/// With `keep_without_geometry`, these issues on a row that is written
/// anyway get [`WRITTEN_WITHOUT_GEOMETRY`] appended to their detail.
const NO_GEOMETRY_KINDS: [&str; 5] = [
    "no_geometry",
    "geometry_empty",
    "geometry_unexpected_type",
    "boundary_unparseable",
    "boundary_unresolved_url",
];
pub const WRITTEN_WITHOUT_GEOMETRY: &str = "written without geometry (--keep-without-geometry)";

/// `keep_without_geometry`: retain a Location that has a resolved place in
/// the hierarchy but no point and no usable boundary, instead of dropping
/// it. Such rows sort after every row with geometry, by country then id.
pub fn build_index(
    ndjson: &Path,
    profile: &crate::profile::Profile,
    country_override: Option<&str>,
    keep_without_geometry: bool,
) -> crate::error::Result<Index> {
    let mut report = Report::default();
    let code_column = profile.location.admin_code_column.as_deref();
    let mut records: Vec<IndexRecord> = Vec::new();
    let mut read = 0usize;

    for line in NdjsonReader::open(ndjson)? {
        // Only I/O and encoding errors abort the run here; a line that
        // fails to parse as JSON, or as a Location, is reported below and
        // skipped so one bad line does not sink the whole snapshot.
        let line = line?;
        read += 1;
        let value: serde_json::Value = match serde_json::from_str(&line.text) {
            Ok(v) => v,
            Err(err) => {
                report.add(
                    "malformed_field",
                    &format!("line {}", line.number),
                    &err.to_string(),
                );
                continue;
            }
        };
        let Some(loc) = Location::parse(&value, profile, &mut report) else {
            continue;
        };
        if let Some(Boundary::Url(url)) = &loc.boundary {
            report.add(
                "boundary_unresolved_url",
                &loc.id,
                &format!(
                    "boundary is a url reference ({url}); run fhir-pipes extract to inline it"
                ),
            );
        }
        let geometry = geometry::summarize(&loc, &mut report);
        records.push(IndexRecord {
            id: loc.id,
            part_of: loc.part_of,
            name: loc.name,
            admin_code: loc.admin_code,
            is_admin: loc.type_code.is_some()
                && loc.type_code.as_deref() == profile.location.admin_unit_type.as_deref(),
            type_code: loc.type_code,
            location_types: loc.location_types,
            managing_organization: loc.managing_organization,
            offset: line.offset,
            len: line.len,
            geometry,
            ..Default::default()
        });
    }

    // Resolved over every parsed record, including ones with no geometry:
    // a geometry-less district is still somebody's parent.
    let hierarchy = resolve_hierarchy(&records, &mut report);

    let retained: Vec<(u32, Option<GeometrySummary>)> = (0..records.len())
        .filter_map(|i| {
            let r = &records[i];
            match (hierarchy.get(i), r.geometry) {
                (Some(_), Some(geom)) => Some((i as u32, Some(geom))),
                (Some(_), None) if keep_without_geometry => Some((i as u32, None)),
                _ => None,
            }
        })
        .collect();

    for &(i, geom) in &retained {
        let i = i as usize;
        let country = match country_override {
            Some(c) => c.to_string(),
            None => match hierarchy.country(&records, i) {
                Some(c) => c,
                // Without an admin code in the profile there is no country
                // to find, and nothing to report about every row.
                None if code_column.is_none() => UNKNOWN_COUNTRY.to_string(),
                None => {
                    report.add(
                        "no_country",
                        &records[i].id,
                        &format!(
                            "no admin unit ancestor carries an admin code ({}); filed under 'unknown'",
                            code_column.unwrap_or_default()
                        ),
                    );
                    UNKNOWN_COUNTRY.to_string()
                }
            },
        };
        let tier = match hierarchy.get(i).and_then(|info| info.admin_level) {
            Some(level) => level.to_string(),
            None => "site".to_string(),
        };
        // A row without geometry has no place on the curve; it keeps 0 and
        // sorts by id instead.
        let hilbert = geom.map_or(0, |g| {
            let b = g.bbox;
            hilbert_key([(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0], HILBERT_EXTENT)
        });
        let rec = &mut records[i];
        rec.country = country;
        rec.tier = tier;
        rec.hilbert = hilbert;
    }

    if keep_without_geometry {
        let kept: HashSet<&str> = retained
            .iter()
            .filter(|(_, g)| g.is_none())
            .map(|&(i, _)| records[i as usize].id.as_str())
            .collect();
        for issue in &mut report.issues {
            if NO_GEOMETRY_KINDS.contains(&issue.kind.as_str())
                && kept.contains(issue.location_id.as_str())
            {
                issue.detail.push_str("; ");
                issue.detail.push_str(WRITTEN_WITHOUT_GEOMETRY);
            }
        }
    }

    // Duplicate admin code detection is scoped to retained rows only: a dropped
    // record (no geometry without --keep-without-geometry, or trimmed by
    // the hierarchy resolver) never reaches the output, so it should never
    // manufacture a collision with one that does.
    let mut by_pcode: HashMap<&str, Vec<u32>> = HashMap::new();
    for &(i, _) in &retained {
        if let Some(p) = &records[i as usize].admin_code {
            by_pcode.entry(p.as_str()).or_default().push(i);
        }
    }
    let mut dup_pcodes: Vec<&str> = by_pcode
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(&k, _)| k)
        .collect();
    dup_pcodes.sort();
    for pcode in dup_pcodes {
        let ids = &by_pcode[pcode];
        let mut names: Vec<&str> = ids
            .iter()
            .map(|&i| records[i as usize].id.as_str())
            .collect();
        names.sort();
        // Named after the column: duplicate_pcode for ICR.
        let column = code_column.unwrap_or("admin_code");
        report.add(
            &format!("duplicate_{column}"),
            &names.join(", "),
            &format!("{column} {pcode} claimed by {} Locations", names.len()),
        );
    }

    // Rows with geometry: (country, geometry kind, Hilbert key), ties in
    // file order (the sort is stable). Rows without geometry come after all
    // of them, by country then id, so they also come last in any partition.
    let mut order: Vec<u32> = retained.iter().map(|&(i, _)| i).collect();
    order.sort_by_key(|&i| {
        let r = &records[i as usize];
        match r.geometry {
            Some(g) => (false, r.country.as_str(), g.kind.as_str(), r.hilbert, ""),
            None => (true, r.country.as_str(), "", 0, r.id.as_str()),
        }
    });

    Ok(Index {
        records,
        hierarchy,
        order,
        report,
        read,
        keep_without_geometry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/snapshot/locations.ndjson")
    }

    fn idx(index: &Index, id: &str) -> usize {
        index
            .records
            .iter()
            .position(|r| r.id == id)
            .unwrap_or_else(|| panic!("no record {id}"))
    }

    #[test]
    fn indexes_the_fixture_snapshot() {
        let index = build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        assert_eq!(index.read, 12);
        assert_eq!(
            index.records.len(),
            12,
            "every parsed resource stays addressable"
        );
        let retained: Vec<&str> = index
            .order
            .iter()
            .map(|&i| index.records[i as usize].id.as_str())
            .collect();
        // dropped: remote (url boundary, no position), ghost (no geometry), cyc-a, cyc-b (cycle)
        assert_eq!(retained.len(), 8, "{retained:?}");
        assert!(!retained.contains(&"ghost"));
        assert!(!retained.contains(&"remote"));
        assert!(!retained.contains(&"cyc-a"));
        // Exact order: sorted by the three-key (country, geometry kind,
        // Hilbert key) over the fixed WGS84 extent. Seven of the eight
        // retained rows are country "NG" (sorting before "unknown"), and
        // within a country group points ("point" < "polygon") come before
        // polygons; "orphan" is the sole "unknown"-country row and so
        // sorts last regardless of its own (point) kind.
        assert_eq!(
            retained,
            vec![
                "clinic",
                "stray",
                "ng",
                "kano",
                "dup",
                "gama",
                "nassarawa",
                "orphan"
            ]
        );
        let clinic = idx(&index, "clinic");
        assert_eq!(index.records[clinic].country, "NG");
        assert_eq!(index.records[clinic].tier, "site");
        assert_eq!(
            index.hierarchy.admin_names(&index.records, clinic)[1].as_deref(),
            Some("Kano")
        );
        let orphan = idx(&index, "orphan");
        assert_eq!(index.records[orphan].country, "unknown");
        assert_eq!(index.report.count("no_country"), 1);
        assert_eq!(index.report.count("boundary_unresolved_url"), 1);
        assert_eq!(index.report.count("duplicate_pcode"), 1);
        assert_eq!(index.report.count("cycle"), 2);
        assert_eq!(index.report.count("orphan"), 1);
        assert_eq!(index.report.count("no_geometry"), 2, "ghost and remote");
        // Sorted by partition then Hilbert: (country, geom kind) is non-decreasing along `order`.
        let keys: Vec<(String, &str)> = index
            .order
            .iter()
            .map(|&i| {
                let r = &index.records[i as usize];
                (r.country.clone(), r.geometry.unwrap().kind.as_str())
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert!(
            index
                .order
                .iter()
                .all(|&i| index.records[i as usize].len > 0)
        );
    }

    #[test]
    fn country_override_wins() {
        let index = build_index(
            &fixture(),
            &crate::profile::icr_example(),
            Some("XX"),
            false,
        )
        .unwrap();
        assert!(
            index
                .order
                .iter()
                .all(|&i| index.records[i as usize].country == "XX")
        );
        assert_eq!(index.report.count("no_country"), 0);
    }

    #[test]
    fn empty_snapshot_yields_empty_order() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b"{\"resourceType\":\"Location\",\"id\":\"x\"}\n").unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, false).unwrap();
        assert_eq!(index.records.len(), 1);
        assert!(index.order.is_empty());
        assert_eq!(index.report.count("no_geometry"), 1);
    }

    #[test]
    fn dropped_rows_do_not_create_duplicate_pcodes() {
        // Two Locations share a pcode, but only one of them is retained
        // (the other has no usable geometry): no duplicate should be
        // reported, since the dropped row never reaches the output.
        let ndjson = concat!(
            "{\"resourceType\":\"Location\",\"id\":\"a\",\"identifier\":[{\"system\":\"https://icr.healthcampaigns.org/identifiers/pcode\",\"value\":\"NG1\"}],\"position\":{\"longitude\":1,\"latitude\":2}}\n",
            "{\"resourceType\":\"Location\",\"id\":\"b\",\"identifier\":[{\"system\":\"https://icr.healthcampaigns.org/identifiers/pcode\",\"value\":\"NG1\"}]}\n",
        );
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), ndjson).unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, false).unwrap();
        assert_eq!(index.report.count("duplicate_pcode"), 0);
        assert_eq!(index.order.len(), 1);
    }

    #[test]
    fn keep_without_geometry_retains_ghost_and_remote_last() {
        let index = build_index(&fixture(), &crate::profile::icr_example(), None, true).unwrap();
        assert!(index.keep_without_geometry);
        let retained: Vec<&str> = index
            .order
            .iter()
            .map(|&i| index.records[i as usize].id.as_str())
            .collect();
        // The eight rows with geometry in their usual order, then the two
        // without, by country then id. Cycle members stay dropped.
        assert_eq!(
            retained,
            vec![
                "clinic",
                "stray",
                "ng",
                "kano",
                "dup",
                "gama",
                "nassarawa",
                "orphan",
                "ghost",
                "remote"
            ]
        );
        let ghost = idx(&index, "ghost");
        assert!(index.records[ghost].geometry.is_none());
        assert_eq!(index.records[ghost].country, "NG");
        assert_eq!(index.records[ghost].tier, "2");
        let remote = idx(&index, "remote");
        assert_eq!(index.records[remote].tier, "site");
        // Still reported, and the detail says the row was written.
        assert_eq!(index.report.count("no_geometry"), 2);
        for id in ["ghost", "remote"] {
            let issue = index
                .report
                .issues
                .iter()
                .find(|i| i.kind == "no_geometry" && i.location_id == id)
                .unwrap();
            assert!(
                issue.detail.ends_with(WRITTEN_WITHOUT_GEOMETRY),
                "{}",
                issue.detail
            );
        }
        // Issues for other rows are left as they were.
        assert!(
            index
                .report
                .issues
                .iter()
                .filter(|i| i.location_id != "ghost" && i.location_id != "remote")
                .all(|i| !i.detail.contains(WRITTEN_WITHOUT_GEOMETRY))
        );
    }

    #[test]
    fn without_the_flag_no_issue_mentions_written_rows() {
        let index = build_index(&fixture(), &crate::profile::icr_example(), None, false).unwrap();
        assert!(!index.keep_without_geometry);
        assert!(
            index
                .report
                .issues
                .iter()
                .all(|i| !i.detail.contains(WRITTEN_WITHOUT_GEOMETRY))
        );
    }

    #[test]
    fn keep_without_geometry_still_drops_cycles_and_duplicate_ids() {
        let ndjson = concat!(
            "{\"resourceType\":\"Location\",\"id\":\"a\",\"partOf\":{\"reference\":\"Location/b\"}}\n",
            "{\"resourceType\":\"Location\",\"id\":\"b\",\"partOf\":{\"reference\":\"Location/a\"}}\n",
            "{\"resourceType\":\"Location\",\"id\":\"c\"}\n",
            "{\"resourceType\":\"Location\",\"id\":\"c\"}\n",
            "{\"resourceType\":\"Location\",\"id\":\"d\",\"extension\":[{\"url\":\"https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson\",\"valueAttachment\":{\"contentType\":\"application/geo+json\",\"data\":\"bm90IGpzb24=\"}}]}\n",
            "not json\n",
        );
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), ndjson).unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, true).unwrap();
        let retained: Vec<&str> = index
            .order
            .iter()
            .map(|&i| index.records[i as usize].id.as_str())
            .collect();
        // a and b are a cycle, the second c is a duplicate id, the last
        // line does not parse. The first c (nothing at all) and d (a
        // boundary that does not parse) are written without geometry.
        assert_eq!(retained, vec!["c", "d"]);
        assert_eq!(index.report.count("cycle"), 2);
        assert_eq!(index.report.count("duplicate_id"), 1);
        assert_eq!(index.report.count("boundary_unparseable"), 1);
        let d = index
            .report
            .issues
            .iter()
            .find(|i| i.kind == "boundary_unparseable")
            .unwrap();
        assert!(d.detail.ends_with(WRITTEN_WITHOUT_GEOMETRY), "{}", d.detail);
        let cycle = index
            .report
            .issues
            .iter()
            .find(|i| i.kind == "no_geometry" && i.location_id == "a")
            .unwrap();
        assert!(
            !cycle.detail.contains(WRITTEN_WITHOUT_GEOMETRY),
            "a dropped row must not be described as written"
        );
    }

    #[test]
    fn rows_without_geometry_count_towards_duplicate_pcodes_when_kept() {
        let ndjson = concat!(
            "{\"resourceType\":\"Location\",\"id\":\"a\",\"identifier\":[{\"system\":\"https://icr.healthcampaigns.org/identifiers/pcode\",\"value\":\"NG1\"}],\"position\":{\"longitude\":1,\"latitude\":2}}\n",
            "{\"resourceType\":\"Location\",\"id\":\"b\",\"identifier\":[{\"system\":\"https://icr.healthcampaigns.org/identifiers/pcode\",\"value\":\"NG1\"}]}\n",
        );
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), ndjson).unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, true).unwrap();
        assert_eq!(index.order.len(), 2);
        assert_eq!(index.report.count("duplicate_pcode"), 1);
    }

    #[test]
    fn rows_without_geometry_sort_by_country_then_id() {
        let ndjson = concat!(
            "{\"resourceType\":\"Location\",\"id\":\"z\"}\n",
            "{\"resourceType\":\"Location\",\"id\":\"p\",\"position\":{\"longitude\":1,\"latitude\":2}}\n",
            "{\"resourceType\":\"Location\",\"id\":\"m\"}\n",
            "{\"resourceType\":\"Location\",\"id\":\"a\"}\n",
        );
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), ndjson).unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, true).unwrap();
        let retained: Vec<&str> = index
            .order
            .iter()
            .map(|&i| index.records[i as usize].id.as_str())
            .collect();
        assert_eq!(retained, vec!["p", "a", "m", "z"]);
    }

    #[test]
    fn a_malformed_line_is_reported_and_the_run_continues() {
        let ndjson = concat!(
            "{\"resourceType\":\"Location\",\"id\":\"a\",\"position\":{\"longitude\":1,\"latitude\":2}}\n",
            "garbage\n",
            "{\"resourceType\":\"Location\",\"id\":\"b\",\"position\":{\"longitude\":3,\"latitude\":4}}\n",
        );
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), ndjson).unwrap();
        let index = build_index(f.path(), &crate::profile::icr_example(), None, false).unwrap();
        assert_eq!(index.read, 3);
        assert_eq!(index.records.len(), 2);
        assert_eq!(index.report.count("malformed_field"), 1);
        assert_eq!(
            index
                .report
                .issues
                .iter()
                .find(|i| i.kind == "malformed_field")
                .unwrap()
                .location_id,
            "line 2"
        );
    }
}
