//! Counters for `GET /metrics`, in the Prometheus text format.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::store::RunRecord;

#[derive(Default)]
pub struct Metrics {
    /// (trigger, status) → runs that ended so.
    runs: Mutex<BTreeMap<(String, String), u64>>,
    /// status → when the last run that ended so ended (Unix seconds).
    last_run: Mutex<BTreeMap<String, i64>>,
    last_success: Mutex<Option<i64>>,
    last_duration: Mutex<f64>,
    retries: AtomicU64,
    schedule_skipped: AtomicU64,
    /// destination → failed tries.
    notify_failures: Mutex<BTreeMap<String, u64>>,
    pub running: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl Metrics {
    /// A run has ended after `seconds`.
    pub fn run_ended(&self, rec: &RunRecord, seconds: f64) {
        let trigger = name(&rec.trigger);
        let status = name(&rec.status);
        *lock(&self.runs)
            .entry((trigger, status.clone()))
            .or_default() += 1;
        let now = chrono::Utc::now().timestamp();
        lock(&self.last_run).insert(status.clone(), now);
        if status == "succeeded" {
            *lock(&self.last_success) = Some(now);
        }
        *lock(&self.last_duration) = seconds;
        self.retries.fetch_add(
            rec.attempts.len().saturating_sub(1) as u64,
            Ordering::Relaxed,
        );
    }

    pub fn schedule_skipped(&self) {
        self.schedule_skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn notify_failed(&self, destination: &str) {
        *lock(&self.notify_failures)
            .entry(destination.to_string())
            .or_default() += 1;
    }

    /// The text for `/metrics`. `destinations` are the configured notify
    /// destinations, shown at 0 before any failure; the table rows come from
    /// the published `manifest`.
    pub fn render(
        &self,
        destinations: &[String],
        manifest: &Path,
        record_write_failing: bool,
        log_lines_dropped: u64,
    ) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "# TYPE fhir_pipes_runs_total counter");
        for ((trigger, status), n) in lock(&self.runs).iter() {
            let _ = writeln!(
                o,
                "fhir_pipes_runs_total{{trigger=\"{}\",status=\"{}\"}} {n}",
                label(trigger),
                label(status)
            );
        }
        let _ = writeln!(o, "# TYPE fhir_pipes_run_running gauge");
        let _ = writeln!(
            o,
            "fhir_pipes_run_running {}",
            u8::from(self.running.load(Ordering::Relaxed))
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_last_run_timestamp_seconds gauge");
        for (status, at) in lock(&self.last_run).iter() {
            let _ = writeln!(
                o,
                "fhir_pipes_last_run_timestamp_seconds{{status=\"{}\"}} {at}",
                label(status)
            );
        }
        let _ = writeln!(o, "# TYPE fhir_pipes_last_success_timestamp_seconds gauge");
        let _ = writeln!(
            o,
            "fhir_pipes_last_success_timestamp_seconds {}",
            lock(&self.last_success).unwrap_or(0)
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_last_run_duration_seconds gauge");
        let _ = writeln!(
            o,
            "fhir_pipes_last_run_duration_seconds {}",
            *lock(&self.last_duration)
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_retries_total counter");
        let _ = writeln!(
            o,
            "fhir_pipes_retries_total {}",
            self.retries.load(Ordering::Relaxed)
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_schedule_skipped_total counter");
        let _ = writeln!(
            o,
            "fhir_pipes_schedule_skipped_total {}",
            self.schedule_skipped.load(Ordering::Relaxed)
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_notify_failures_total counter");
        let failures = lock(&self.notify_failures);
        for d in destinations {
            let _ = writeln!(
                o,
                "fhir_pipes_notify_failures_total{{destination=\"{}\"}} {}",
                label(d),
                failures.get(d).copied().unwrap_or(0)
            );
        }
        let _ = writeln!(o, "# TYPE fhir_pipes_record_write_failing gauge");
        let _ = writeln!(
            o,
            "fhir_pipes_record_write_failing {}",
            u8::from(record_write_failing)
        );
        let _ = writeln!(o, "# TYPE fhir_pipes_log_lines_dropped_total counter");
        let _ = writeln!(o, "fhir_pipes_log_lines_dropped_total {log_lines_dropped}");
        let _ = writeln!(o, "# TYPE fhir_pipes_table_rows gauge");
        let tables = std::fs::read_to_string(manifest)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        if let Some(tables) = tables.as_ref().and_then(|m| m["tables"].as_object()) {
            for (table, t) in tables {
                if let Some(rows) = t["rows"].as_u64() {
                    let _ = writeln!(
                        o,
                        "fhir_pipes_table_rows{{table=\"{}\"}} {rows}",
                        label(table)
                    );
                }
            }
        }
        o
    }
}

/// The lowercase name a record uses for `v` (`api`, `succeeded`), for labels.
pub fn name(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}
