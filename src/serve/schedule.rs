//! Runs on a schedule: five-field cron expressions in UTC. A time that comes
//! while a run is current is skipped; times missed while serve was stopped
//! are not run later.

use chrono::{DateTime, Utc};

use super::config::{Mode, Schedule};

/// Where the scheduler gets the time, and how it waits for it.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    /// Wait until `at` by this clock.
    fn sleep_until(&self, at: DateTime<Utc>);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn sleep_until(&self, at: DateTime<Utc>) {
        sleep_for(at - self.now());
    }
}

fn sleep_for(d: chrono::TimeDelta) {
    if let Ok(d) = d.to_std() {
        std::thread::sleep(d);
    }
}

/// For the tests of a debug build: a clock that starts at
/// FHIR_PIPES_SERVE_TEST_NOW and runs at the system clock's pace.
#[cfg(debug_assertions)]
pub struct OffsetClock {
    offset: chrono::TimeDelta,
}

#[cfg(debug_assertions)]
impl OffsetClock {
    pub fn from_env() -> Option<OffsetClock> {
        let at = std::env::var("FHIR_PIPES_SERVE_TEST_NOW").ok()?;
        let at = DateTime::parse_from_rfc3339(&at).ok()?.with_timezone(&Utc);
        Some(OffsetClock {
            offset: at - Utc::now(),
        })
    }
}

#[cfg(debug_assertions)]
impl Clock for OffsetClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now() + self.offset
    }

    fn sleep_until(&self, at: DateTime<Utc>) {
        sleep_for(at - self.now());
    }
}

/// The clock serve uses.
pub fn clock() -> std::sync::Arc<dyn Clock> {
    #[cfg(debug_assertions)]
    if let Some(c) = OffsetClock::from_env() {
        return std::sync::Arc::new(c);
    }
    std::sync::Arc::new(SystemClock)
}

/// The first time after `after` that a schedule fires, and its mode; the
/// first schedule listed wins a tie.
pub fn next_fire(schedules: &[Schedule], after: DateTime<Utc>) -> Option<(DateTime<Utc>, Mode)> {
    schedules
        .iter()
        .filter_map(|s| {
            let at = s.cron.find_next_occurrence(&after, false).ok()?;
            Some((at, s.mode))
        })
        // min_by_key keeps the first of equal keys.
        .min_by_key(|(at, _)| *at)
}

/// What a scheduled time did.
#[derive(Debug, PartialEq, Eq)]
pub enum Fired {
    Started,
    /// A run was current: the time is skipped.
    Skipped,
    /// The run could not be recorded.
    Refused,
}

/// Fire each scheduled time from now on through `start`, `times` of them
/// (forever with `None`).
pub fn run(
    schedules: &[Schedule],
    clock: &dyn Clock,
    mut start: impl FnMut(DateTime<Utc>, Mode) -> Fired,
    times: Option<usize>,
) {
    let mut after = clock.now();
    let mut fired = 0;
    while times.is_none_or(|n| fired < n) {
        let Some((at, mode)) = next_fire(schedules, after) else {
            return;
        };
        clock.sleep_until(at);
        // A clock woken early (a signal, a short sleep) waits again.
        if clock.now() < at {
            continue;
        }
        start(at, mode);
        fired += 1;
        after = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Mutex;

    fn schedule(cron: &str, mode: Mode) -> Schedule {
        Schedule {
            cron: croner::Cron::new(cron).parse().unwrap(),
            mode,
        }
    }

    fn utc(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, h, m, s).unwrap()
    }

    /// A clock that jumps to whatever it is asked to wait for.
    struct Manual(Mutex<DateTime<Utc>>);

    impl Clock for Manual {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
        fn sleep_until(&self, at: DateTime<Utc>) {
            *self.0.lock().unwrap() = at;
        }
    }

    #[test]
    fn next_fire_is_utc_and_picks_the_earliest() {
        let s = [
            schedule("0 */6 * * *", Mode::Incremental),
            schedule("30 2 * * *", Mode::Full),
        ];
        assert_eq!(
            next_fire(&s, utc(1, 0, 0)),
            Some((utc(2, 30, 0), Mode::Full))
        );
        assert_eq!(
            next_fire(&s, utc(2, 30, 0)),
            Some((utc(6, 0, 0), Mode::Incremental))
        );
        assert_eq!(next_fire(&[], utc(1, 0, 0)), None);
    }

    #[test]
    fn a_tie_goes_to_the_first_schedule() {
        let s = [
            schedule("0 * * * *", Mode::Full),
            schedule("0 * * * *", Mode::Incremental),
        ];
        assert_eq!(
            next_fire(&s, utc(1, 30, 0)),
            Some((utc(2, 0, 0), Mode::Full))
        );
    }

    #[test]
    fn a_missed_time_is_not_run_at_start() {
        let s = [schedule("0 * * * *", Mode::Incremental)];
        // Started at 01:30: 01:00 has passed and is not run; 02:00 is next.
        let clock = Manual(Mutex::new(utc(1, 30, 0)));
        let mut fired = Vec::new();
        run(
            &s,
            &clock,
            |at, _| {
                fired.push(at);
                Fired::Started
            },
            Some(2),
        );
        assert_eq!(fired, [utc(2, 0, 0), utc(3, 0, 0)]);
    }

    #[test]
    fn a_scheduled_time_during_a_run_is_skipped() {
        let s = [schedule("0 * * * *", Mode::Incremental)];
        let clock = Manual(Mutex::new(utc(1, 30, 0)));
        let mut results = vec![Fired::Skipped, Fired::Started];
        let mut fired = Vec::new();
        run(
            &s,
            &clock,
            |at, _| {
                fired.push(at);
                results.remove(0)
            },
            Some(2),
        );
        // The skipped 02:00 is not tried again: the next is 03:00.
        assert_eq!(fired, [utc(2, 0, 0), utc(3, 0, 0)]);
    }
}
