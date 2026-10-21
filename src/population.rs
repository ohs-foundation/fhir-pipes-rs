//! `fhir-pipes population`: population denominators for admin units from a raster.
//!
//! Reads the snapshot with the same pass-one index `transform` uses, so the
//! hierarchy (and its report) come for free; sums the raster under every
//! admin unit at `--level` with the pixel-centroid rule; rolls the sums up
//! the partOf chain so every admin ancestor gets a calculated total (or, with
//! `--type CODE`, sums every Location of that type — e.g. facility-catchment —
//! on its own, with no roll-up); and
//! writes one `ICRTargetPopulation` Group per unit as FHIR NDJSON for
//! `fhir-pipes load`. Group ids are deterministic (`pop-SOURCE-YEAR-LOCATIONID`),
//! so re-running replaces the same resources. Locations are never touched.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use geo::Geometry;

use crate::cli::PopulationArgs;
use crate::error::{PipesError, Result};
use crate::fhir::group::{
    MAX_ID_LEN, MAX_QUANTITY, TargetPopulation, group_id, is_valid_id, target_population_group,
};
use crate::fhir::ndjson::{LineAccess, Output, is_gz};
use crate::fhir::{Boundary, Location};
use crate::fs::AtomicFile;
use crate::geometry::parse_boundary;
use crate::index::hierarchy::Hierarchy;
use crate::index::{IndexRecord, build_index};
use crate::profile::Profile;
use crate::raster::Raster;
use crate::raster::fetch;
use crate::raster::geotiff::GeoTiff;
use crate::raster::zonal::{ZonalSum, raster_total, zonal_sum};
use crate::report::{Report, write_report};
use crate::snapshot::Snapshot;

/// Which Locations to measure: every admin unit at one level, or every Location of one
/// type code (catchments, operational areas). Only the level mode rolls up: a catchment
/// tiling is not the admin tree, and its sums must not overwrite the admin units' own.
enum Selector {
    Level(i8),
    Type(String),
}

/// One unit's figure: measured from the raster at `--level`, or rolled up
/// from measured descendants.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UnitTotal {
    pub sum: f64,
    pub pixels: u64,
    /// Rolled up from children rather than measured directly.
    pub calculated: bool,
    /// A measured descendant had no usable boundary, so this under-counts.
    pub incomplete: bool,
}

/// Admin-unit ancestors of record `i`, nearest first. Non-admin nodes on
/// the chain (a facility's supervisory area, say) are skipped, not stopped at.
fn admin_ancestors(hierarchy: &Hierarchy, i: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cur = hierarchy.get(i).and_then(|info| info.parent);
    while let Some(n) = cur {
        let idx = n.index();
        let info = hierarchy.get(idx);
        if info.and_then(|f| f.admin_level).is_some() {
            out.push(idx);
        }
        cur = info.and_then(|f| f.parent);
    }
    out
}

/// Fold one measured unit (or a unit that could not be measured) into every
/// admin ancestor's rolled-up total.
pub fn roll_up(
    totals: &mut BTreeMap<usize, UnitTotal>,
    hierarchy: &Hierarchy,
    i: usize,
    measured: Option<ZonalSum>,
) {
    for anc in admin_ancestors(hierarchy, i) {
        let t = totals.entry(anc).or_insert(UnitTotal {
            calculated: true,
            ..UnitTotal::default()
        });
        match measured {
            Some(z) => {
                t.sum += z.sum;
                t.pixels += z.pixels;
            }
            None => t.incomplete = true,
        }
    }
}

/// The unit's boundary summed against the raster; `None` (after reporting)
/// when there is nothing usable to measure. Parsing here re-reads the line
/// pass one already indexed, so parse noise goes to a scratch report and
/// only this command's own kinds reach `report`.
fn measure(
    lines: &mut LineAccess,
    rec: &IndexRecord,
    raster: &mut impl Raster,
    profile: &Profile,
    report: &mut Report,
) -> Result<Option<ZonalSum>> {
    let text = lines.read_at(rec.offset, rec.len)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let mut scratch = Report::default();
    let Some(loc) = Location::parse(&value, profile, &mut scratch) else {
        report.add(
            "no_boundary",
            &rec.id,
            "resource did not parse as a Location; no population measured",
        );
        return Ok(None);
    };
    let geom = match &loc.boundary {
        Some(Boundary::Inline(bytes)) => {
            let g = parse_boundary(bytes, &loc.id, &mut scratch);
            if g.is_none() {
                report.add(
                    "no_boundary",
                    &loc.id,
                    "boundary did not parse; no population measured",
                );
            }
            g
        }
        Some(Boundary::Url(url)) => {
            report.add(
                "no_boundary",
                &loc.id,
                &format!("boundary is an unresolved url ({url}); run extract to inline it"),
            );
            None
        }
        None => {
            report.add(
                "no_boundary",
                &loc.id,
                "admin unit has no boundary; no population measured",
            );
            None
        }
    };
    let Some(geom) = geom else {
        return Ok(None);
    };
    // GeoTiff::read_tile also classifies decode failures as Usage; check the
    // geometry's own type here rather than matching on zonal_sum's error, so
    // a corrupt raster is never mislabeled as `no_boundary`.
    if !matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_)) {
        report.add(
            "no_boundary",
            &loc.id,
            "boundary is not a Polygon or MultiPolygon; no population measured",
        );
        return Ok(None);
    }
    Ok(Some(zonal_sum(raster, &geom)?))
}

/// True when the Group id fhir-pipes would mint for this Location is a valid FHIR
/// id; otherwise reports `group_id_too_long` / `group_id_invalid` and returns
/// false. The prefix `pop-{source}-{year}-` eats into the 64-character limit.
fn valid_group_id(report: &mut Report, source: &str, year: u16, location_id: &str) -> bool {
    let id = group_id(source, year, location_id);
    if is_valid_id(&id) {
        return true;
    }
    let kind = if id.len() > MAX_ID_LEN {
        "group_id_too_long"
    } else {
        "group_id_invalid"
    };
    report.add(
        kind,
        location_id,
        &format!("{id:?} is not a valid FHIR id ([A-Za-z0-9.-], 1–64 chars); no Group written"),
    );
    false
}

fn valid_source_code(code: &str) -> bool {
    !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn run_population(args: &PopulationArgs, profile: Option<&str>) -> anyhow::Result<()> {
    let snapshot = Snapshot::new(&args.snapshot);
    let ndjson = snapshot.locations();
    if !ndjson.is_file() {
        return Err(
            PipesError::Usage(format!("snapshot file not found: {}", ndjson.display())).into(),
        );
    }
    crate::fs::check_out(&args.out)?;
    if !valid_source_code(&args.source) {
        return Err(PipesError::Usage(format!(
            "--source must be a code of letters, digits and hyphens, got {:?}",
            args.source
        ))
        .into());
    }
    if !(1900..=2100).contains(&args.year) {
        return Err(PipesError::Usage(format!("--year {} is outside 1900–2100", args.year)).into());
    }
    if let Some(d) = &args.estimate_date
        && !crate::calendar::is_date(d)
    {
        return Err(PipesError::Usage(format!(
            "--estimate-date must be a date, YYYY-MM-DD, got {d:?}"
        ))
        .into());
    }
    let estimate_date = args
        .estimate_date
        .clone()
        .unwrap_or_else(|| format!("{:04}-01-01", args.year));

    // Admin levels come from the profile's admin_unit_type; the Groups'
    // profile, extensions and systems from its [population] section.
    let profile = Profile::for_snapshot(profile, &args.snapshot)?;
    eprintln!("profile: {} ({})", profile.name, profile.source);
    let pop = profile.population()?.clone();
    let index = build_index(&ndjson, &profile, None, false)?;
    let mut report = index.report;
    if !pop.known_source_codes.is_empty() && !pop.known_source_codes.contains(&args.source) {
        report.add(
            "unknown_source_code",
            &args.source,
            &format!(
                "not a code in {}; written as given (extensible binding)",
                pop.denominator_source_system
            ),
        );
    }
    // What to measure: every admin unit at one level (then rolled up the admin chain), or
    // every Location of one type code — catchments, operational areas — summed on its own.
    let selector = match (args.level, &args.type_code) {
        (Some(l), None) => Selector::Level(
            i8::try_from(l)
                .map_err(|_| PipesError::Usage(format!("--level {l} is out of range")))?,
        ),
        (None, Some(t)) if !t.is_empty() => Selector::Type(t.clone()),
        _ => {
            return Err(
                PipesError::Usage("give exactly one of --level or --type".to_string()).into(),
            );
        }
    };
    // "level 2" / "type facility-catchment" in messages; "admin level 2" in the source text;
    // "level-2" / "facility-catchment" before "units".
    let (scope, scope_long, unit_label) = match &selector {
        Selector::Level(l) => (
            format!("level {l}"),
            format!("admin level {l}"),
            format!("level-{l}"),
        ),
        Selector::Type(t) => (format!("type {t}"), format!("type {t}"), t.clone()),
    };
    let targets: Vec<usize> = (0..index.records.len())
        .filter(|&i| match &selector {
            Selector::Level(level) => {
                index.hierarchy.get(i).and_then(|f| f.admin_level) == Some(*level)
            }
            // Any coding under the ICR location-type system, not just the
            // first coding: a Location may list a local concept first.
            Selector::Type(t) => index.records[i].location_types.iter().any(|c| c == t),
        })
        .collect();
    if targets.is_empty() {
        return Err(PipesError::Usage(match &selector {
            Selector::Level(l) => format!("no admin units at level {l} in {}", ndjson.display()),
            Selector::Type(t) => format!("no Locations of type {t} in {}", ndjson.display()),
        })
        .into());
    }

    let cache_dir = args.cache_dir.clone().unwrap_or_else(|| snapshot.rasters());
    let loaded = fetch::load(
        &args.raster,
        &cache_dir,
        args.retries,
        Duration::from_secs(args.timeout),
    )?;
    for e in &loaded.cache_errors {
        report.add("cache_error", &args.raster, e);
    }
    eprintln!(
        "Raster {} ({:.1} MB{})",
        loaded.label,
        loaded.bytes.len() as f64 / 1e6,
        if loaded.cached { ", from cache" } else { "" }
    );
    let mut raster = GeoTiff::open(loaded.bytes, &loaded.label)?;
    let grand = raster_total(&mut raster)?;

    let mut lines = LineAccess::open(&ndjson)?;
    let mut totals: BTreeMap<usize, UnitTotal> = BTreeMap::new();
    for &i in &targets {
        let measured = measure(
            &mut lines,
            &index.records[i],
            &mut raster,
            &profile,
            &mut report,
        )?;
        // Round once at the leaf so every ancestor's published count is
        // exactly the sum of its published descendants.
        let measured = measured.map(|z| ZonalSum {
            sum: z.sum.round(),
            pixels: z.pixels,
        });
        // A leaf whose Group cannot be written must not inflate its ancestors
        // unflagged: treat it like a missing boundary, so they roll up as
        // incomplete.
        let measured = if valid_group_id(&mut report, &args.source, args.year, &index.records[i].id)
        {
            measured
        } else {
            None
        };
        if let Some(z) = measured {
            totals.insert(
                i,
                UnitTotal {
                    sum: z.sum,
                    pixels: z.pixels,
                    calculated: false,
                    incomplete: false,
                },
            );
        }
        if matches!(selector, Selector::Level(_)) {
            roll_up(&mut totals, &index.hierarchy, i, measured);
        }
    }

    let measured_any = totals.values().any(|t| !t.calculated && t.pixels > 0);
    if !measured_any {
        return Err(PipesError::Usage(format!(
            "no Location at {scope} has a raster pixel centre inside it (raster total {:.0}); is {} the right raster for this snapshot?",
            grand.sum, loaded.label
        )).into());
    }

    let source_text = format!(
        "{}; pixel-centroid zonal sum over {scope_long}",
        loaded.label
    );
    let file = AtomicFile::create(&args.out)?;
    let tmp = file.tmp().to_path_buf();
    let mut out = Output::new(file, is_gz(&args.out));
    let (mut measured_n, mut rolled_n) = (0usize, 0usize);
    for (&i, t) in &totals {
        let rec = &index.records[i];
        if t.pixels == 0 {
            report.add(
                if t.calculated {
                    "rollup_empty"
                } else {
                    "no_pixels"
                },
                &rec.id,
                "no raster pixel with data has its centre inside; no Group written",
            );
            continue;
        }
        if t.incomplete {
            report.add(
                "rollup_incomplete",
                &rec.id,
                "a descendant at the measured level had no usable boundary or no writable Group; this total under-counts",
            );
        }
        // Leaves were validated before roll-up; this catches ancestors.
        if t.calculated && !valid_group_id(&mut report, &args.source, args.year, &rec.id) {
            continue;
        }
        // Leaf sums were rounded once before roll-up and ancestors are sums of
        // those integers, so `t.sum` is already integral.
        let count = t.sum;
        if !(count >= 0.0 && count <= MAX_QUANTITY as f64) {
            report.add(
                "quantity_out_of_range",
                &rec.id,
                &format!("{count} exceeds FHIR unsignedInt; no Group written"),
            );
            continue;
        }
        let group = target_population_group(
            &TargetPopulation {
                location_id: &rec.id,
                location_name: rec.name.as_deref(),
                count: count as u64,
                year: args.year,
                source_code: &args.source,
                source_text: &source_text,
                estimate_date: &estimate_date,
                calculated: t.calculated,
                planning_denominator: args.planning_denominator,
            },
            &pop,
        );
        serde_json::to_writer(&mut out, &group)?;
        out.write_all(b"\n").map_err(|e| PipesError::io(&tmp, e))?;
        if t.calculated {
            rolled_n += 1;
        } else {
            measured_n += 1;
        }
    }
    out.finish()
        .map_err(|e| PipesError::io(&tmp, e))?
        .commit()?;

    let assigned: f64 = totals
        .values()
        .filter(|t| !t.calculated)
        .fold(0.0_f64, |acc, t| acc + t.sum);
    let share = if grand.sum > 0.0 {
        100.0 * assigned / grand.sum
    } else {
        0.0
    };
    println!(
        "Wrote {} Groups to {} ({measured_n} measured at {scope}, {rolled_n} rolled up)",
        measured_n + rolled_n,
        args.out.display(),
    );
    println!(
        "Raster total {:.0}; assigned to {unit_label} units {:.0} ({share:.1}%)",
        grand.sum, assigned
    );
    if let Selector::Level(level) = selector {
        let other_level_admin_units = (0..index.records.len())
            .filter(|&i| {
                index
                    .hierarchy
                    .get(i)
                    .and_then(|f| f.admin_level)
                    .is_some_and(|l| l != level)
            })
            .count();
        if other_level_admin_units > 0 {
            println!(
                "{other_level_admin_units} admin units at other levels were not measured directly"
            );
        }
    }
    println!("{}", report.summary());
    if let Some(path) = &args.report {
        write_report(path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::hierarchy::resolve_hierarchy;
    use base64::Engine;
    use serde_json::{Value, json};

    /// Writes each resource to its own line of a temp `locations.ndjson`,
    /// re-reads it with the same `NdjsonReader` pass two uses to get every
    /// line's offset/len, and builds one `IndexRecord` per line plus the
    /// `LineAccess` `measure` reads through. The `NamedTempFile` must be
    /// returned and kept alive by the caller: `LineAccess` reopens the path
    /// itself, so the file must still be on disk.
    fn index_ndjson(lines: &[Value]) -> (tempfile::NamedTempFile, LineAccess, Vec<IndexRecord>) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for v in lines {
            writeln!(tmp, "{v}").unwrap();
        }
        let path = tmp.path().to_path_buf();
        let records: Vec<IndexRecord> = crate::fhir::ndjson::NdjsonReader::open(&path)
            .unwrap()
            .map(|l| l.unwrap())
            .zip(lines)
            .map(|(line, v)| IndexRecord {
                id: v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                offset: line.offset,
                len: line.len,
                ..IndexRecord::default()
            })
            .collect();
        let access = LineAccess::open(&path).unwrap();
        (tmp, access, records)
    }

    fn admin_loc(id: &str, boundary: Option<Value>) -> Value {
        let mut r = json!({
            "resourceType": "Location",
            "id": id,
            "type": [{"coding": [{"code": "admin-unit"}]}],
        });
        if let Some(ext) = boundary {
            r["extension"] = json!([ext]);
        }
        r
    }

    fn boundary_ext(data: Option<&str>, url: Option<&str>) -> Value {
        let mut att = json!({"contentType": "application/geo+json"});
        if let Some(d) = data {
            att["data"] = json!(d);
        }
        if let Some(u) = url {
            att["url"] = json!(u);
        }
        json!({
            "url": "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson",
            "valueAttachment": att
        })
    }

    fn rect_geojson(x0: f64, y0: f64, x1: f64, y1: f64) -> String {
        format!(
            r#"{{"type":"Polygon","coordinates":[[[{x0},{y0}],[{x1},{y0}],[{x1},{y1}],[{x0},{y1}],[{x0},{y0}]]]}}"#
        )
    }

    #[test]
    fn measure_reports_each_way_a_unit_can_be_unmeasurable() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);

        // A real boundary measures cleanly: no report entries at all.
        {
            let square = rect_geojson(3.0, 6.5, 3.5, 7.0);
            let loc = admin_loc("a", Some(boundary_ext(Some(&b64(&square)), None)));
            let (_tmp, mut lines, records) = index_ndjson(&[loc]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(
                got,
                Some(ZonalSum {
                    sum: 5074.0,
                    pixels: 24
                })
            );
            assert_eq!(report.counts().len(), 0);
        }

        // No boundary extension at all.
        {
            let loc = admin_loc("a", None);
            let (_tmp, mut lines, records) = index_ndjson(&[loc]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(got, None);
            assert_eq!(report.count("no_boundary"), 1);
            assert!(
                report.issues[0].detail.contains("no boundary"),
                "{}",
                report.issues[0].detail
            );
        }

        // valueAttachment.url instead of data: not yet inlined by extract.
        {
            let loc = admin_loc(
                "a",
                Some(boundary_ext(None, Some("https://files/x.geojson"))),
            );
            let (_tmp, mut lines, records) = index_ndjson(&[loc]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(got, None);
            assert_eq!(report.count("no_boundary"), 1);
            assert!(
                report.issues[0].detail.contains("unresolved url"),
                "{}",
                report.issues[0].detail
            );
        }

        // data is base64 of something that is not GeoJSON at all.
        {
            let loc = admin_loc("a", Some(boundary_ext(Some(&b64("not json")), None)));
            let (_tmp, mut lines, records) = index_ndjson(&[loc]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(got, None);
            assert_eq!(report.count("no_boundary"), 1);
            assert!(
                report.issues[0].detail.contains("did not parse"),
                "{}",
                report.issues[0].detail
            );
        }

        // A Point boundary parses as GeoJSON but has no area to measure.
        {
            let point = r#"{"type":"Point","coordinates":[3.2,6.8]}"#;
            let loc = admin_loc("a", Some(boundary_ext(Some(&b64(point)), None)));
            let (_tmp, mut lines, records) = index_ndjson(&[loc]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(got, None);
            assert_eq!(report.count("no_boundary"), 1);
            assert!(
                report.issues[0].detail.contains("not a Polygon"),
                "{}",
                report.issues[0].detail
            );
        }

        // The line is not a Location at all.
        {
            let org = json!({"resourceType": "Organization", "id": "o"});
            let (_tmp, mut lines, records) = index_ndjson(&[org]);
            let mut raster = crate::raster::MemRaster::fixture();
            let mut report = Report::default();
            let got = measure(
                &mut lines,
                &records[0],
                &mut raster,
                &crate::profile::icr_example(),
                &mut report,
            )
            .unwrap();
            assert_eq!(got, None);
            assert_eq!(report.count("no_boundary"), 1);
            assert!(
                report.issues[0]
                    .detail
                    .contains("did not parse as a Location"),
                "{}",
                report.issues[0].detail
            );
        }
    }

    fn admin(id: &str, part_of: Option<&str>) -> IndexRecord {
        IndexRecord {
            id: id.to_string(),
            part_of: part_of.map(str::to_string),
            type_code: Some("admin-unit".to_string()),
            is_admin: true,
            ..IndexRecord::default()
        }
    }

    #[test]
    fn roll_up_sums_into_every_admin_ancestor_and_flags_gaps() {
        // ng ← kano ← (lga-a, lga-b); ng ← lagos ← lga-c ; clinic (not admin) under lga-a
        let records = vec![
            admin("ng", None),
            admin("kano", Some("ng")),
            admin("lga-a", Some("kano")),
            admin("lga-b", Some("kano")),
            admin("lagos", Some("ng")),
            admin("lga-c", Some("lagos")),
            IndexRecord {
                id: "clinic".into(),
                part_of: Some("lga-a".into()),
                type_code: Some("facility".into()),
                ..IndexRecord::default()
            },
        ];
        let mut report = Report::default();
        let hierarchy = resolve_hierarchy(&records, &mut report);
        assert_eq!(hierarchy.get(2).unwrap().admin_level, Some(2));
        assert_eq!(hierarchy.get(6).unwrap().admin_level, None);

        let mut totals = BTreeMap::new();
        let z = |sum: f64, pixels: u64| Some(ZonalSum { sum, pixels });
        roll_up(&mut totals, &hierarchy, 2, z(10.0, 1));
        roll_up(&mut totals, &hierarchy, 3, z(20.0, 2));
        roll_up(&mut totals, &hierarchy, 5, None);

        let kano = totals[&1];
        assert_eq!(
            (kano.sum, kano.pixels, kano.calculated, kano.incomplete),
            (30.0, 3, true, false)
        );
        let lagos = totals[&4];
        assert_eq!(
            (lagos.sum, lagos.pixels, lagos.calculated, lagos.incomplete),
            (0.0, 0, true, true)
        );
        let ng = totals[&0];
        assert_eq!(
            (ng.sum, ng.pixels, ng.calculated, ng.incomplete),
            (30.0, 3, true, true)
        );
        assert!(
            !totals.contains_key(&2),
            "measured units are the caller's to insert"
        );
        assert!(!totals.contains_key(&6));
    }

    #[test]
    fn source_codes_are_restricted_to_id_safe_characters() {
        assert!(
            valid_source_code("worldpop")
                && valid_source_code("grid3")
                && valid_source_code("census-projection")
        );
        assert!(
            !valid_source_code("") && !valid_source_code("world pop") && !valid_source_code("a/b")
        );
    }
}
