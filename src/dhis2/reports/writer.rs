//! Fetching units on `--concurrency` workers and writing their reports in
//! planned order, with the running totals, the checkpoint and the progress
//! line.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::{Dhis2ReportsArgs, ReportSource};
use crate::dhis2::Instance;
use crate::dhis2::api::{Dhis2, ServerInfo};
use crate::dhis2::measures::Indicator;
use crate::dhis2::rerun::{Change, Checkpoint, Previous};
use crate::error::{PipesError, Result};
use crate::fhir::ndjson::NdjsonWriter;
use crate::work::InOrder;

use super::analytics::{Analytics, AnalyticsRow, Fetched, Values, fetch_unit};
use super::build::{Target, measure_report, report_id, stratum};
use super::periods::period_value;
use super::plan::Unit;

/// Totals that can be counted from the reports themselves, so a resumed run
/// recounts them from what the interrupted run wrote.
#[derive(Debug, Default)]
pub(super) struct Tally {
    pub(super) written: usize,
    /// The ids written. Overlapping `--period`s (`LAST_3_MONTHS` and
    /// `LAST_12_MONTHS`) give some reports twice; each is written once.
    pub(super) ids: HashSet<String>,
    pub(super) locations: HashSet<String>,
    pub(super) periods: HashSet<String>,
    pub(super) stratified: usize,
    pub(super) new: usize,
    pub(super) changed: usize,
    pub(super) unchanged: usize,
}

impl Tally {
    /// Counts one written report, comparing it with the previous run's.
    fn add(&mut self, r: &Value, previous: Option<&mut Previous>) -> Option<Change> {
        self.ids
            .insert(r["id"].as_str().unwrap_or_default().to_string());
        self.written += 1;
        if let Some(s) = r.pointer("/subject/reference").and_then(Value::as_str)
            && !self.locations.contains(s)
        {
            self.locations.insert(s.to_string());
        }
        if let Some(pe) = r
            .pointer("/extension/0/valueString")
            .and_then(Value::as_str)
            && !self.periods.contains(pe)
        {
            self.periods.insert(pe.to_string());
        }
        self.stratified += r
            .pointer("/group/0/stratifier")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let change = previous.map(|p| p.classify(r));
        match change {
            Some(Change::New) => self.new += 1,
            Some(Change::Changed) => self.changed += 1,
            Some(Change::Unchanged) => self.unchanged += 1,
            None => {}
        }
        change
    }
}

/// Turns fetched units into written reports. It runs on one thread and
/// takes units in planned order, so the output and the run report are the
/// same at any `--concurrency`; it keeps running totals, not reports.
/// After each unit the output is synced and the checkpoint saved, so an
/// interrupted run can carry on from there.
pub(super) struct Writer<'a> {
    pub(super) out: NdjsonWriter,
    /// `--changed`: the new and changed reports.
    pub(super) changed: Option<NdjsonWriter>,
    pub(super) previous: Option<Previous>,
    pub(super) instance: &'a Instance,
    pub(super) info: &'a ServerInfo,
    pub(super) indicators: &'a BTreeMap<String, Indicator>,
    pub(super) mapping: &'a HashMap<String, Target>,
    /// `--campaign`, added to every report after the period extension.
    pub(super) campaign: Option<&'a str>,
    /// Analytics or data values, written in the source extension.
    pub(super) source: ReportSource,
    pub(super) state: Checkpoint,
    pub(super) checkpoint: PathBuf,
    pub(super) tally: Tally,
}

impl<'a> Writer<'a> {
    /// The indicator and Location of a row, or `None` after reporting why
    /// the row is skipped.
    fn target(&mut self, row: &AnalyticsRow) -> Option<(&'a Indicator, &'a Target)> {
        let indicators = self.indicators;
        let mapping = self.mapping;
        let Some(ind) = indicators.get(&row.dx) else {
            self.state.report.add(
                "unexpected_indicator",
                &row.dx,
                "DHIS2 returned an indicator that was not asked for; skipped",
            );
            return None;
        };
        let Some(location) = mapping.get(&row.ou) else {
            if self.state.unmapped.insert(row.ou.clone()) {
                self.state.report.add(
                    "unmapped_org_unit",
                    &row.ou,
                    "no Location carries this org unit; its values are skipped",
                );
            }
            return None;
        };
        Some((ind, location))
    }

    /// With `--previous`, notes the org units and periods a response
    /// covered: where removed values are looked for.
    fn scope(&mut self, table: &Analytics) {
        if self.previous.is_none() {
            return;
        }
        let s = &mut self.state;
        for ou in table
            .org_units
            .iter()
            .chain(table.rows.iter().map(|r| &r.ou))
        {
            if !s.scope_org_units.contains(ou) {
                s.scope_org_units.insert(ou.clone());
            }
        }
        for pe in &table.period_codes {
            s.scope_periods.insert(period_value(pe));
        }
        for row in &table.rows {
            s.scope_periods.insert(period_value(&row.pe));
        }
    }

    /// Writes one unit's reports, sorted by id, with its strata attached to
    /// its totals; returns how many values DHIS2 sent for it.
    fn unit(&mut self, f: Fetched) -> Result<usize> {
        for (kind, key, detail) in &f.issues {
            if kind != "unmapped_org_unit" || self.state.unmapped.insert(key.clone()) {
                self.state.report.add(kind, key, detail);
            }
        }
        let mut rows = 0usize;
        let mut out: BTreeMap<String, Value> = BTreeMap::new();
        for table in &f.totals {
            rows += table.rows.len();
            self.scope(table);
            for row in &table.rows {
                let Some((ind, location)) = self.target(row) else {
                    continue;
                };
                let Some(period) = table.periods.get(&row.pe) else {
                    self.state.report.add(
                        "period_without_dates",
                        &row.pe,
                        "DHIS2 gave no start and end date for this period; row skipped",
                    );
                    continue;
                };
                if let Some(mut r) = measure_report(
                    row,
                    ind,
                    location,
                    period,
                    self.instance,
                    self.info,
                    &mut self.state.report,
                ) {
                    if let Some(ext) = r["extension"].as_array_mut() {
                        if let Some(c) = self.campaign {
                            ext.push(json!({"url": self.instance.urls.campaign_extension, "valueReference": {"reference": c}}));
                        }
                        let (code, display) = self.source.coding();
                        ext.push(json!({"url": self.instance.urls.source_extension,
                            "valueCodeableConcept": {"coding": [{"system": self.instance.urls.source_system,
                                "code": code, "display": display}]}}));
                    }
                    out.insert(r["id"].as_str().unwrap_or_default().to_string(), r);
                }
            }
        }

        // (report id, breakdown) → (breakdown name, strata)
        let mut strata: BTreeMap<(String, String), (String, Vec<Value>)> = BTreeMap::new();
        for (b, tables) in &f.breakdowns {
            for table in tables {
                rows += table.rows.len();
                for row in &table.rows {
                    let Some((ind, _)) = self.target(row) else {
                        continue;
                    };
                    if let Some(st) = stratum(
                        row,
                        ind,
                        b,
                        &table.names,
                        self.instance,
                        &mut self.state.report,
                    ) {
                        let name = table.names.get(b).cloned().unwrap_or_else(|| b.clone());
                        strata
                            .entry((report_id(self.instance, row), b.clone()))
                            .or_insert_with(|| (name, Vec::new()))
                            .1
                            .push(st);
                    }
                }
            }
        }
        for ((rid, b), (name, mut list)) in strata {
            let Some(r) = out.get_mut(&rid) else {
                self.state.report.add(
                    "stratum_without_total",
                    &rid,
                    &format!("DHIS2 broke a value down by {name} but gave no total for it; breakdown left out"),
                );
                continue;
            };
            list.sort_by(|a, b| {
                a["value"]["text"]
                    .as_str()
                    .cmp(&b["value"]["text"].as_str())
            });
            let stratifier = json!({
                "code": [{"coding": [{"system": self.instance.dimension_system(None), "code": b, "display": name}], "text": name}],
                "stratum": list,
            });
            match r["group"][0]
                .get_mut("stratifier")
                .and_then(Value::as_array_mut)
            {
                Some(existing) => existing.push(stratifier),
                None => r["group"][0]["stratifier"] = json!([stratifier]),
            }
        }

        for r in out.values() {
            if self
                .tally
                .ids
                .contains(r["id"].as_str().unwrap_or_default())
            {
                continue;
            }
            let change = self.tally.add(r, self.previous.as_mut());
            self.out.write(r)?;
            if let (Some(w), Some(Change::New | Change::Changed)) = (self.changed.as_mut(), change)
            {
                w.write(r)?;
            }
        }
        self.state.values += rows;
        self.state.requests += f.requests;
        Ok(rows)
    }

    /// Syncs everything written and records that `units_done` units are in
    /// the files. A crash before this leaves the last checkpoint, and a
    /// resumed run cuts the files back to it.
    pub(super) fn save(&mut self, units_done: usize) -> Result<()> {
        self.state.out_bytes = self.out.sync()?;
        if let Some(w) = &mut self.changed {
            self.state.changed_bytes = Some(w.sync()?);
        }
        self.state.units_done = units_done;
        self.state.write(&self.checkpoint)
    }

    /// After a resume: counts the reports the interrupted run wrote, and
    /// marks their ids as written again for `--removed`.
    pub(super) fn recount(&mut self) -> Result<()> {
        let path = self.out.tmp().to_path_buf();
        let mut reader = self.out.read_back()?;
        let mut line = String::new();
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .map_err(|e| PipesError::io(&path, e))?
                == 0
            {
                return Ok(());
            }
            let r: Value = serde_json::from_str(&line)?;
            self.tally.add(&r, self.previous.as_mut());
        }
    }
}

/// `1234567` → `1,234,567`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `42s`, `4m12s`, `1h05m`; to the minute only, `4m`, when `rough`.
fn clock(d: Duration, rough: bool) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 if rough => format!("{}m", s / 60),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `[37/412] level 4 · 20260421…20260427 (7 periods) · 10 items: 33,656
/// values · 4m12s elapsed, ~38m left`. The estimate assumes the units left
/// take as long as the ones done since `start` (where a resumed run began).
fn progress(
    done: usize,
    start: usize,
    total: usize,
    unit: &Unit,
    values: usize,
    elapsed: Duration,
) -> String {
    let left = if done < total && done > start {
        let rest = elapsed.mul_f64((total - done) as f64 / (done - start) as f64);
        format!(", ~{} left", clock(rest, true))
    } else {
        String::new()
    };
    format!(
        "[{done}/{total}] {} · {} · {} items: {} values · {} elapsed{left}",
        unit.where_(),
        unit.when(),
        unit.indicators.len(),
        thousands(values),
        clock(elapsed, false)
    )
}

/// Fetches `units[start..]` on `--concurrency` workers and writes them.
/// Workers fetch units from a shared counter; this thread writes them in
/// planned order, holding back any that finish early. A worker does not
/// start a unit more than `window` ahead of the last one written, so memory
/// stays at a few units, not the whole export. On a failure what was
/// written is kept for `--resume`.
pub(super) fn fetch_and_write(
    w: &mut Writer,
    api: &Dhis2,
    values: Values,
    args: &Dhis2ReportsArgs,
    units: &[Unit],
    start: usize,
) -> Result<()> {
    let total = units.len();
    let workers = args.concurrency.min(total - start).max(1);
    let window = workers * 2;
    let stop = AtomicBool::new(false);
    let written = (Mutex::new(start), Condvar::new());
    let started = Instant::now();
    let mut failure: Option<PipesError> = None;
    let mut cursor = start;
    let mut order = InOrder::new(start);
    // Stops the workers, including any waiting for the window to move.
    let halt = || {
        // Set under the lock so a worker about to wait sees it.
        let guard = written.0.lock().unwrap_or_else(PoisonError::into_inner);
        stop.store(true, Ordering::Relaxed);
        drop(guard);
        written.1.notify_all();
    };
    crate::work::run(
        workers,
        crate::work::indices(start, total),
        |i| {
            {
                let (lock, ready) = &written;
                let mut done = lock.lock().unwrap_or_else(PoisonError::into_inner);
                while i >= *done + window && !stop.load(Ordering::Relaxed) {
                    done = ready.wait(done).unwrap_or_else(PoisonError::into_inner);
                }
            }
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            Some((i, fetch_unit(api, values, &args.breakdowns, &units[i])))
        },
        |fetched| {
            // After a failure, what workers still mid-request send is dropped.
            if failure.is_some() {
                return false;
            }
            let Some((i, fetched)) = fetched else {
                return true;
            };
            match fetched {
                Ok(f) => order.push(i, f),
                Err(e) => {
                    failure = Some(e);
                    halt();
                    return false;
                }
            }
            while let Some((_, f)) = order.pop() {
                match w
                    .unit(f)
                    .and_then(|values| w.save(cursor + 1).map(|_| values))
                {
                    Ok(values) => eprintln!(
                        "{}",
                        progress(
                            cursor + 1,
                            start,
                            total,
                            &units[cursor],
                            values,
                            started.elapsed()
                        )
                    ),
                    Err(e) => {
                        failure = Some(e);
                        halt();
                        return false;
                    }
                }
                cursor += 1;
                *written.0.lock().unwrap_or_else(PoisonError::into_inner) = cursor;
                written.1.notify_all();
            }
            true
        },
    );
    match failure {
        Some(e) => {
            eprintln!(
                "{cursor} of {total} batches written to {}; run again with --resume to continue from there",
                w.out.tmp().display()
            );
            Err(e)
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhis2::reports::plan::tests::codes;

    #[test]
    fn progress_reads_at_a_glance() {
        let unit = Unit {
            levels: vec![4],
            periods: codes(&["20260421", "20260422", "20260427"]),
            range: None,
            indicators: codes(&["a"; 10]),
        };
        assert_eq!(
            progress(37, 0, 412, &unit, 33_656, Duration::from_secs(252)),
            "[37/412] level 4 · 20260421…20260427 (3 periods) · 10 items: 33,656 values · 4m12s elapsed, ~42m left"
        );
        let one = Unit {
            periods: codes(&["2025"]),
            ..unit
        };
        assert_eq!(
            progress(2, 0, 2, &one, 5, Duration::from_secs(3725)),
            "[2/2] level 4 · 2025 · 10 items: 5 values · 1h02m elapsed"
        );
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
