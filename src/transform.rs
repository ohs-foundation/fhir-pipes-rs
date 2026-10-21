//! `fhir-pipes transform locations`: pass one, pass two, report.

use std::path::Path;

use crate::cli::TransformArgs;
use crate::error::{PipesError, Result};
use crate::index::build_index;
use crate::index::partition::parse_keys;
use crate::profile::Profile;
use crate::report::write_json;
use crate::snapshot::ORGANIZATIONS_FILE;
use crate::write::dataset::write_dataset;

pub const SNAPSHOT_FILE: &str = "locations.ndjson";

/// `--out` must either not exist yet or already be a directory.
pub fn check_out_dir(out: &Path) -> Result<()> {
    if out.exists() && !out.is_dir() {
        return Err(PipesError::Usage(format!(
            "--out {}: not a directory",
            out.display()
        )));
    }
    Ok(())
}

pub fn run_transform(args: &TransformArgs, profile: Option<&str>) -> anyhow::Result<()> {
    let profile = Profile::for_snapshot(profile, &args.snapshot)?;
    Ok(transform_with(args, &profile)?)
}

/// `run_transform` with the profile already loaded (`run` loads it once for
/// both of its steps).
pub fn transform_with(args: &TransformArgs, profile: &Profile) -> Result<()> {
    check_out_dir(&args.out)?;
    let ndjson = args.snapshot.join(SNAPSHOT_FILE);
    if !ndjson.is_file() {
        return Err(PipesError::Usage(format!(
            "snapshot file not found: {}",
            ndjson.display()
        )));
    }
    let keys = parse_keys(&args.partition_by)?;
    if args.row_group_size == 0 {
        return Err(PipesError::Usage(
            "--row-group-size must be at least 1".into(),
        ));
    }

    eprintln!("profile: {} ({})", profile.name, profile.source);
    let mut index = build_index(
        &ndjson,
        profile,
        args.country.as_deref(),
        args.keep_without_geometry,
    )?;
    if args.keep_without_geometry {
        let without = index
            .order
            .iter()
            .filter(|&&i| index.records[i as usize].geometry.is_none())
            .count();
        eprintln!(
            "indexed {} resources, {} with a place in the hierarchy ({without} of them without geometry)",
            index.read,
            index.order.len()
        );
    } else {
        eprintln!(
            "indexed {} resources, {} with geometry and hierarchy",
            index.read,
            index.order.len()
        );
    }

    // A stale report must never describe a dataset it does not match:
    // remove it before writing, rewrite it only on success.
    let report_path = args.out.join("_report.json");
    match std::fs::remove_file(&report_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PipesError::io(&report_path, e)),
    }

    let organizations = args.snapshot.join(ORGANIZATIONS_FILE);
    let organizations = organizations.is_file().then_some(organizations.as_path());
    let written = write_dataset(
        &ndjson,
        profile,
        &mut index,
        &args.out,
        &keys,
        args.row_group_size,
        organizations,
    )?;

    let without_geometry: usize = written.iter().map(|w| w.rows_without_geometry).sum();
    let mut report_json = index.report.to_json();
    if args.keep_without_geometry {
        // Present only with the flag, so the report says these rows were
        // written, not dropped, next to their no_geometry issues.
        report_json["written_without_geometry"] = without_geometry.into();
    }
    write_json(&report_path, &report_json)?;
    // diff, index and population read the snapshot with the same profile.
    profile.save_copy(&args.snapshot);
    let rows: usize = written.iter().map(|w| w.rows).sum();
    println!(
        "Wrote {rows} rows across {} partitions to {}",
        written.len(),
        args.out.display()
    );
    if args.keep_without_geometry {
        println!(
            "{without_geometry} of these rows have no geometry; --keep-without-geometry wrote them with the geometry columns null"
        );
    }
    for w in &written {
        let rel = w.path.strip_prefix(&args.out).unwrap_or(&w.path);
        eprintln!("  {} ({} rows)", rel.display(), w.rows);
    }
    println!("{}", index.report.summary());
    Ok(())
}
