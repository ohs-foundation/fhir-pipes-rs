//! `fhir-pipes dhis2 reports`: indicator values from DHIS2 analytics as
//! MeasureReports, one per indicator, org unit and period.
//!
//! DHIS2 computes every value itself: it adds up the numerator and the
//! denominator for the org unit and period, then divides, so values are
//! requested at each level and never summed here. `includeNumDen` returns
//! the numerator and denominator beside the value; `skipRounding` returns
//! the value unrounded. Period start and end dates come from the response's
//! own metadata, so every DHIS2 period type works without fhir-pipes knowing its
//! calendar rules. A `--range` of dates is asked for with `startDate` and
//! `endDate` instead, and gives one value for the whole range.
//!
//! Org units are mapped to Locations through the identifier that
//! `fhir-pipes dhis2 locations` writes. A row whose org unit has no Location is
//! reported and skipped.
//!
//! An export can run to millions of values, so the work is planned in
//! units (one level, one chunk of indicators, one batch of periods of the
//! same kind, sized from the level's org unit count to stay under
//! `--max-rows`). Workers fetch units in parallel; each unit's totals and
//! breakdowns come back together, and its reports are written as soon as
//! every unit before it has been, so memory holds a few units at a time and
//! the file is the same at any `--concurrency`. After each unit the file is
//! synced and a checkpoint saved, so `--resume` can continue an interrupted
//! run; `--previous` compares with an earlier run (see `rerun`).
//!
//! `--source data-values` computes the same tables from raw data values
//! instead (see `raw`), so everything after the fetch is shared.
//!
//! `plan` divides a run into units, `analytics` fetches each unit, `build`
//! turns its rows into MeasureReports and `writer` writes them in planned
//! order; `periods` reads period codes and date ranges.

mod analytics;
mod build;
mod periods;
mod plan;
mod writer;

pub use analytics::{Analytics, AnalyticsRow};
pub use periods::DateRange;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::Instance;
use super::api::{Dhis2, ServerInfo, check_analytics_age};
use super::measures::{fetch_indicators, fhir_key};
use super::rerun::{Checkpoint, Previous, Scope, file_stamp, plan_differences, write_lines};
use crate::cli::{Dhis2ReportsArgs, ReportSource};
use crate::error::{PipesError, Result};
use crate::fhir::group::is_valid_id;
use crate::fhir::ndjson::NdjsonWriter;
use crate::fs::check_out;
use crate::report::write_report;
use crate::snapshot::instant::format_utc;

use analytics::Values;
use build::load_mapping;
use periods::period_kind;
use plan::{Planned, plan_run};
use writer::{Tally, Writer, fetch_and_write};

/// DHIS2 UIDs are 11 characters.
const UID_LEN: usize = 11;

/// A `--campaign` reference, `ResourceType/id`.
fn check_campaign(reference: &str) -> Result<()> {
    let ok = match reference.split_once('/') {
        Some((kind, id)) => {
            kind.starts_with(|c: char| c.is_ascii_uppercase())
                && kind.chars().all(|c| c.is_ascii_alphanumeric())
                && is_valid_id(id)
        }
        None => false,
    };
    if ok {
        Ok(())
    } else {
        Err(PipesError::Usage(format!(
            "--campaign {reference:?}: expected a reference ResourceType/id, e.g. CarePlan/bgd-mr-sia-2026"
        )))
    }
}

/// Each value once, in the order first given.
fn unique<T: Clone + PartialEq>(list: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for x in list {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

pub fn run_reports(args: &Dhis2ReportsArgs, profile: &crate::profile::Profile) -> Result<()> {
    check_args(args)?;
    let from_values = args.source == ReportSource::DataValues;
    let instance = Instance::new(&args.conn.instance, profile)?;
    let ranges = unique(&args.ranges)
        .iter()
        .map(|r| DateRange::parse(r))
        .collect::<Result<Vec<_>>>()?;
    if let Some(c) = &args.campaign {
        check_campaign(c)?;
    }
    if !args.locations.is_file() {
        return Err(PipesError::Usage(format!(
            "--locations {}: no such file",
            args.locations.display()
        )));
    }
    let system = instance.orgunit_system();
    let mapping = load_mapping(&args.locations, &system)?;
    if mapping.is_empty() {
        return Err(PipesError::Usage(format!(
            "no Location in {} carries an identifier under {system}; run `fhir-pipes dhis2 locations --instance {}` and load its output first",
            args.locations.display(),
            instance.name
        )));
    }

    let api = Dhis2::connect(&args.conn)?;
    if !from_values {
        match &api.info.last_analytics {
            Some(t) => eprintln!("DHIS2 analytics tables last built {t}"),
            None => eprintln!("DHIS2 does not say when its analytics tables were last built"),
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        if let Some(warning) = check_analytics_age(&api.info, args.max_analytics_age, now)? {
            eprintln!("{warning}");
        }
    }
    let indicators = fetch_indicators(&api, &args.select)?;
    let ids: Vec<String> = indicators.keys().cloned().collect();
    check_report_ids(&instance, &ranges, &unique(&args.periods), &ids)?;

    let plan = plan_run(args, &api, &indicators, &ids, &system, ranges)?;
    let total = plan.units.len();
    let fingerprint = fingerprint(args, &instance, &api, &ids, &plan);
    let checkpoint = Checkpoint::path(&args.out);
    let saved = resume_point(args, &checkpoint, &fingerprint)?;

    let previous = match &args.previous {
        Some(p) => {
            let prev = Previous::load(p)?;
            eprintln!("{}: {} previous reports", p.display(), prev.len());
            Some(prev)
        }
        None => None,
    };
    let start = saved.as_ref().map_or(0, |s| s.units_done);
    let (out, changed, state) = open_outputs(args, saved, fingerprint, total)?;
    // From data values, a report's `date` is when the run started.
    let info = match &state.as_of {
        Some(t) if from_values => ServerInfo {
            last_analytics: Some(t.clone()),
            utc: true,
            ..api.info.clone()
        },
        _ => api.info.clone(),
    };
    let values = match &plan.raw {
        Some(src) => Values::DataValues {
            raw: src,
            indicators: &indicators,
        },
        None => Values::Analytics {
            root: args.root.as_deref(),
        },
    };
    let mut w = Writer {
        out: out.keep_partial(),
        changed: changed.map(NdjsonWriter::keep_partial),
        previous,
        instance: &instance,
        info: &info,
        indicators: &indicators,
        mapping: &mapping,
        campaign: args.campaign.as_deref(),
        source: args.source,
        state,
        checkpoint: checkpoint.clone(),
        tally: Tally::default(),
    };
    if start > 0 {
        w.recount()?;
    }
    w.save(start)?;
    fetch_and_write(&mut w, &api, values, args, &plan.units, start)?;

    let (removed, kept) = compare_with_previous(&mut w, args, &ids, &plan)?;
    let Writer {
        out,
        changed,
        state,
        tally,
        ..
    } = w;
    if let Some(c) = changed {
        c.finish()?;
    }
    if let (Some(path), Some(ids)) = (&args.removed, &removed) {
        write_lines(path, ids)?;
    }
    out.finish()?;
    std::fs::remove_file(&checkpoint).map_err(|e| PipesError::io(&checkpoint, e))?;
    print_summary(
        args,
        indicators.len(),
        &tally,
        &state,
        removed.as_deref(),
        kept,
    )
}

/// The checks of the flags alone, before anything is read or requested.
fn check_args(args: &Dhis2ReportsArgs) -> Result<()> {
    check_out(&args.out)?;
    // A dimension with items (`UID:a;b`) names its analytics column by the
    // UID alone, and goes into report ids and codes: only a bare UID.
    for b in &args.breakdowns {
        if b.len() != UID_LEN || !b.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(PipesError::Usage(format!(
                "--breakdown {b:?}: expected the UID of a category or group set (11 letters \
                 and digits); choosing some of its items is not supported"
            )));
        }
    }
    if args.chunk == 0 {
        return Err(PipesError::Usage("--chunk must be at least 1".into()));
    }
    if args.max_rows == 0 {
        return Err(PipesError::Usage("--max-rows must be at least 1".into()));
    }
    if args.concurrency == 0 {
        return Err(PipesError::Usage("--concurrency must be at least 1".into()));
    }
    if args.levels.contains(&0) {
        return Err(PipesError::Usage(
            "--level counts from 1, the top of the DHIS2 tree".into(),
        ));
    }
    for p in [&args.changed, &args.removed].into_iter().flatten() {
        check_out(p)?;
    }
    // Every file the run writes is a file of its own, and only --out may be
    // --previous (the last run's output, replaced at the end); `./a.ndjson`
    // and `a.ndjson` are one file.
    let files: Vec<(&str, &Path)> = [
        ("--out", Some(&args.out)),
        ("--changed", args.changed.as_ref()),
        ("--removed", args.removed.as_ref()),
        ("--report", args.report.as_ref()),
        ("--previous", args.previous.as_ref()),
    ]
    .into_iter()
    .filter_map(|(flag, p)| Some((flag, p?.as_path())))
    .collect();
    for (i, (flag, p)) in files.iter().enumerate() {
        for (other, q) in &files[..i] {
            let allowed = (*other, *flag) == ("--out", "--previous");
            if !allowed && same_file(p, q) {
                return Err(PipesError::Usage(format!(
                    "{flag} {}: is the same file as {other}; each needs a file of its own",
                    p.display()
                )));
            }
        }
    }
    let from_values = args.source == ReportSource::DataValues;
    if let Some(since) = &args.updated_since {
        if !from_values {
            return Err(PipesError::Usage(
                "--updated-since needs --source data-values (analytics has no last-updated filter)"
                    .into(),
            ));
        }
        let shaped = since.len() == 10 || (since.len() >= 16 && since.as_bytes()[10] == b'T');
        if !shaped || super::api::epoch_seconds(since).is_none() {
            return Err(PipesError::Usage(format!(
                "--updated-since {since:?}: expected YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS"
            )));
        }
    }
    if let Some(p) = &args.previous
        && !p.is_file()
    {
        return Err(PipesError::Usage(format!(
            "--previous {}: no such file",
            p.display()
        )));
    }
    Ok(())
}

/// Whether `a` and `b` name one file: the same name in the same directory,
/// once the directory is resolved (`./x`, `x` and `dir/../x` are one).
fn same_file(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        let dir = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        match (dir.canonicalize(), p.file_name()) {
            (Ok(d), Some(n)) => d.join(n),
            _ => p.to_path_buf(),
        }
    };
    key(a) == key(b)
}

/// A range's code is longer than a period's: check now that every report
/// id still fits, rather than skip every row as invalid_id.
fn check_report_ids(
    instance: &Instance,
    ranges: &[DateRange],
    periods: &[String],
    ids: &[String],
) -> Result<()> {
    // A relative period (`LAST_12_MONTHS`) becomes codes DHIS2 picks; the
    // others are the codes the ids will hold.
    let codes = ranges
        .iter()
        .map(|r| ("--range", r.label(), r.code()))
        .chain(
            periods
                .iter()
                .filter(|p| period_kind(p).is_some())
                .map(|p| ("--period", p.clone(), p.clone())),
        );
    for (flag, label, code) in codes {
        for dx in ids {
            let id = format!(
                "{}-{}-{}-{}",
                instance.name,
                fhir_key(dx),
                "x".repeat(UID_LEN),
                code
            );
            if !is_valid_id(&id) {
                return Err(PipesError::Usage(format!(
                    "{flag} {label}: report ids for {dx} would be {} characters, over FHIR's 64; use a shorter --instance",
                    id.len()
                )));
            }
        }
    }
    Ok(())
}

/// Everything that decides the batches and what goes in them. An
/// interrupted run is continued only under the same plan.
fn fingerprint(
    args: &Dhis2ReportsArgs,
    instance: &Instance,
    api: &Dhis2,
    ids: &[String],
    plan: &Planned,
) -> Value {
    let from_values = args.source == ReportSource::DataValues;
    json!({
        "dhis2": crate::redact::url(&args.conn.url),
        "instance": instance.name,
        "source": args.source.code(),
        "updated_since": args.updated_since,
        // Data values do not depend on the analytics build.
        "last_analytics": if from_values { None } else { api.info.last_analytics.clone() },
        "indicators": ids,
        "levels": plan.levels,
        "periods": plan.periods,
        "ranges": plan.ranges.iter().map(DateRange::label).collect::<Vec<_>>(),
        "root": args.root,
        "breakdowns": args.breakdowns,
        "chunk": args.chunk,
        "max_rows": args.max_rows,
        "campaign": args.campaign,
        "locations": file_stamp(&args.locations),
        "previous": args.previous.as_deref().map(file_stamp),
        "changed": args.changed.as_ref().map(|p| p.display().to_string()),
    })
}

/// With `--resume`, the checkpoint of the interrupted run at `--out`, if
/// its partial output is still there; it must have been planned as
/// `fingerprint`. Without, `None`.
fn resume_point(
    args: &Dhis2ReportsArgs,
    checkpoint: &Path,
    fingerprint: &Value,
) -> Result<Option<Checkpoint>> {
    let saved = if args.resume {
        let saved =
            Checkpoint::read(checkpoint)?.filter(|_| NdjsonWriter::tmp_path(&args.out).is_file());
        if saved.is_none() {
            eprintln!(
                "no interrupted run to resume at {}; starting from the beginning",
                args.out.display()
            );
        }
        saved
    } else {
        if checkpoint.is_file() {
            eprintln!(
                "starting over; the interrupted run at {} is discarded (--resume continues it)",
                args.out.display()
            );
        }
        None
    };
    if let Some(s) = &saved {
        let differ = plan_differences(&s.plan, fingerprint);
        if !differ.is_empty() {
            return Err(PipesError::Usage(format!(
                "--resume: the interrupted run at {} was planned differently ({} changed); run without --resume to start over",
                args.out.display(),
                differ.join(", ")
            )));
        }
    }
    Ok(saved)
}

/// `--out` and `--changed`, continued where `saved` stopped or new, and the
/// state the run carries on with.
fn open_outputs(
    args: &Dhis2ReportsArgs,
    saved: Option<Checkpoint>,
    fingerprint: Value,
    total: usize,
) -> Result<(NdjsonWriter, Option<NdjsonWriter>, Checkpoint)> {
    Ok(match saved {
        Some(s) => {
            let out = NdjsonWriter::resume(&args.out, s.out_bytes)?;
            let changed = match (&args.changed, s.changed_bytes) {
                (Some(p), Some(bytes)) => Some(NdjsonWriter::resume(p, bytes)?),
                (Some(p), None) => Some(NdjsonWriter::create(p)?),
                (None, _) => None,
            };
            eprintln!(
                "resuming after batch {} of {total} ({})",
                s.units_done,
                NdjsonWriter::tmp_path(&args.out).display()
            );
            (out, changed, s)
        }
        None => (
            NdjsonWriter::create(&args.out)?,
            args.changed
                .as_deref()
                .map(NdjsonWriter::create)
                .transpose()?,
            Checkpoint {
                plan: fingerprint,
                as_of: (args.source == ReportSource::DataValues)
                    .then(|| format_utc(std::time::SystemTime::now())),
                ..Checkpoint::default()
            },
        ),
    })
}

/// The ids of the previous reports this run would have written but DHIS2 no
/// longer has (`None` without `--previous`), and how many previous reports
/// were kept in `--out`. Read before `--out` is renamed, as it may be the
/// `--previous` file. With `--updated-since`, the previous reports for the
/// periods not recomputed are kept in `--out`, so it stays the whole
/// baseline.
fn compare_with_previous(
    w: &mut Writer,
    args: &Dhis2ReportsArgs,
    ids: &[String],
    plan: &Planned,
) -> Result<(Option<Vec<String>>, usize)> {
    let mut kept = 0usize;
    let keep_rest = args.updated_since.is_some();
    let Some(prev) = &w.previous else {
        return Ok((None, kept));
    };
    let instance = w.instance;
    let mut scope = Scope {
        measures: ids
            .iter()
            .map(|dx| instance.measure_url(&fhir_key(dx)))
            .collect(),
        subjects: w
            .state
            .scope_org_units
            .iter()
            .filter_map(|ou| w.mapping.get(ou))
            .map(|t| format!("Location/{}", t.id))
            .collect(),
        periods: w.state.scope_periods.iter().cloned().collect(),
    };
    scope.periods.extend(
        plan.periods
            .iter()
            .filter(|p| period_kind(p).is_some())
            .cloned(),
    );
    scope
        .periods
        .extend(plan.ranges.iter().map(DateRange::label));
    let out = &mut w.out;
    let removed = prev.removed(
        |r| scope.contains(r, &instance.urls.period_extension),
        |r| {
            if keep_rest {
                kept += 1;
                out.write(r)?;
            }
            Ok(())
        },
    )?;
    Ok((Some(removed), kept))
}

/// What the run wrote, on stdout, and the run report.
fn print_summary(
    args: &Dhis2ReportsArgs,
    indicators: usize,
    tally: &Tally,
    state: &Checkpoint,
    removed: Option<&[String]>,
    kept: usize,
) -> Result<()> {
    println!(
        "Wrote {} MeasureReports to {} ({} indicators, {} Locations, {} periods; {} values from {} {} requests)",
        tally.written,
        args.out.display(),
        indicators,
        tally.locations.len(),
        tally.periods.len(),
        state.values,
        state.requests,
        if args.source == ReportSource::DataValues {
            "dataValueSets"
        } else {
            "analytics"
        }
    );
    if !args.breakdowns.is_empty() {
        println!(
            "{} breakdowns attached ({} dimensions)",
            tally.stratified,
            args.breakdowns.len()
        );
    }
    if let (Some(prev), Some(removed)) = (&args.previous, removed) {
        println!(
            "Compared with {}: {} new, {} changed, {} unchanged, {} removed",
            prev.display(),
            tally.new,
            tally.changed,
            tally.unchanged,
            removed.len()
        );
        if let Some(p) = &args.changed {
            println!(
                "Wrote {} new or changed MeasureReports to {}",
                tally.new + tally.changed,
                p.display()
            );
        }
        if let Some(p) = &args.removed {
            println!("Wrote {} removed ids to {}", removed.len(), p.display());
        }
        if args.updated_since.is_some() {
            println!(
                "Kept {kept} MeasureReports from {} for periods with no updated data values",
                prev.display()
            );
        }
    }
    println!("{}", state.report.summary());
    if let Some(path) = &args.report {
        write_report(path, &state.report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_file_named_two_ways_is_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.ndjson");
        let dotted = dir.path().join(".").join("a.ndjson");
        let up = dir.path().join("sub/../a.ndjson");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(same_file(&a, &dotted));
        assert!(same_file(&a, &up));
        assert!(!same_file(&a, &dir.path().join("b.ndjson")));
    }

    #[test]
    fn a_long_period_code_is_checked_against_the_id_limit() {
        let instance = Instance::new("a-sixteen-chars1", &crate::profile::icr_example()).unwrap();
        let rate = ["x".repeat(UID_LEN) + ".REPORTING_RATE"];
        assert!(check_report_ids(&instance, &[], &["202610".into()], &rate).is_ok());
        // Relative periods become codes DHIS2 picks; not checked here.
        assert!(check_report_ids(&instance, &[], &["LAST_12_MONTHS".into()], &rate).is_ok());
        let err = check_report_ids(&instance, &[], &["2025AprilS1".into()], &rate).unwrap_err();
        assert!(err.to_string().contains("--period 2025AprilS1"), "{err}");
    }

    #[test]
    fn a_campaign_is_a_reference() {
        assert!(check_campaign("CarePlan/bgd-mr-sia-2026").is_ok());
        for bad in [
            "bgd-mr-sia-2026",
            "careplan/x",
            "CarePlan/",
            "CarePlan/a_b",
            "Care Plan/x",
            "CarePlan/x/y",
        ] {
            assert!(check_campaign(bad).is_err(), "{bad}");
        }
    }
}
