//! `fhir-pipes index`: backfill spatial-index cells onto a snapshot.
//!
//! Reads `locations.ndjson`, and for every Location with a position that
//! lacks a cell at one of the requested (scheme, level) pairs, writes the
//! resource with the cell(s) added to the output NDJSON -- `meta.versionId`
//! and everything else untouched, so `fhir-pipes load` sends it as the same
//! version-checked PUT a `diff` edit would be. Re-running is idempotent:
//! once loaded and re-extracted, nothing is missing and nothing is written.

use std::io::Write;

use serde_json::Value;

use crate::cli::IndexArgs;
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::{NdjsonReader, Output, is_gz, with_suffix};
use crate::fhir::spatial::{cells_of, compute_cell, parse_scheme_level, upsert_cell};
use crate::report::Report;
use crate::snapshot::LOCATIONS_FILE;

pub fn run_index(args: &IndexArgs) -> Result<()> {
    let ndjson = args.snapshot.join(LOCATIONS_FILE);
    if !ndjson.is_file() {
        return Err(PipesError::Usage(format!(
            "snapshot file not found: {}",
            ndjson.display()
        )));
    }
    if args.out.is_dir() {
        return Err(PipesError::Usage(format!(
            "--out {}: is a directory",
            args.out.display()
        )));
    }
    let wanted: Vec<(String, u32)> = args
        .spatial_index
        .iter()
        .map(|a| parse_scheme_level(a))
        .collect::<Result<_>>()?;

    let mut report = Report::default();
    let tmp = with_suffix(&args.out, ".tmp");
    let mut out = Output::create(&tmp, is_gz(&args.out))?;
    let (mut seen, mut written) = (0usize, 0usize);

    for line in NdjsonReader::open(&ndjson)? {
        let line = line?;
        seen += 1;
        let mut value: Value = match serde_json::from_str(&line.text) {
            Ok(v) => v,
            Err(e) => {
                report.add(
                    "snapshot_line_unparsed",
                    &format!("line {}", line.number),
                    &e.to_string(),
                );
                continue;
            }
        };
        let Some(obj) = value.as_object_mut() else {
            report.add(
                "snapshot_line_unparsed",
                &format!("line {}", line.number),
                "not a JSON object",
            );
            continue;
        };
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<no id>")
            .to_string();
        let Some((lon, lat)) = crate::diff::rebuild::position_of(obj) else {
            report.add("no_position", &id, "no position; nothing to index");
            continue;
        };
        let existing = cells_of(obj);
        let mut changed = false;
        for (scheme, level) in &wanted {
            let present = existing
                .iter()
                .any(|c| &c.scheme == scheme && c.level == *level);
            if present && !args.refresh {
                continue;
            }
            let Some(cell) = compute_cell(scheme, *level, lon, lat) else {
                continue;
            };
            if upsert_cell(obj, scheme, *level, &cell) {
                changed = true;
            }
        }
        if changed {
            serde_json::to_writer(&mut out, &value)?;
            out.write_all(b"\n").map_err(|e| PipesError::io(&tmp, e))?;
            written += 1;
            report.add("indexed", &id, "spatial-index cell(s) added");
        } else {
            report.add("already_indexed", &id, "every requested cell present");
        }
    }
    out.finish().map_err(|e| PipesError::io(&tmp, e))?;
    std::fs::rename(&tmp, &args.out).map_err(|e| PipesError::io(&args.out, e))?;

    println!(
        "Wrote {written} of {seen} Locations to {} ({} already indexed, {} without a position)",
        args.out.display(),
        report.counts().get("already_indexed").copied().unwrap_or(0),
        report.counts().get("no_position").copied().unwrap_or(0)
    );
    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report.to_json())?;
        std::fs::write(path, json).map_err(|e| PipesError::io(path, e))?;
    }
    Ok(())
}
