//! Stopping a run on SIGTERM or Ctrl-C (Unix).
//!
//! The first signal sets a flag that `check` reports as `PipesError::Stopped`
//! at the points a run can stop (between steps, resource types and pages),
//! and sends SIGTERM to the tools the run has registered, then SIGKILL to
//! those still running 10 s later. While a `Hold` is alive (the commit
//! window, from the database step to the end of the publish) `check` returns
//! `Ok` and the tools are left alone; the stop takes effect when the last
//! hold ends; a hold asked for after the stop is refused. The failure hooks
//! run under `exempt`, so the stop does not reach them. A second signal
//! kills every tool and exits at once, held or not.
//!
//! On Windows `install` does nothing, so Ctrl-C ends the process as before.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::error::{PipesError, Result};

/// How long a tool has after SIGTERM before it gets SIGKILL, in ms (10 s;
/// shorter in the unit tests).
static TOOL_GRACE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(10_000);

fn tool_grace() -> Duration {
    Duration::from_millis(TOOL_GRACE_MS.load(Ordering::SeqCst))
}

static REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicUsize = AtomicUsize::new(0);
static HOLDS: AtomicUsize = AtomicUsize::new(0);
/// While above 0, tools started after the stop (the failure hooks) are not
/// sent it; a second signal still kills them.
static EXEMPT: AtomicUsize = AtomicUsize::new(0);
/// Whether the registered tools have been sent SIGTERM for this stop.
static TOOLS_STOPPED: AtomicBool = AtomicBool::new(false);
static TOOLS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// Handle SIGTERM and SIGINT from now on. Idempotent.
pub fn install() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            use signal_hook::consts::{SIGINT, SIGTERM};
            match signal_hook::iterator::Signals::new([SIGTERM, SIGINT]) {
                Ok(mut signals) => {
                    std::thread::spawn(move || {
                        for _ in signals.forever() {
                            on_signal();
                        }
                    });
                }
                Err(e) => eprintln!("fhir-pipes: warning: cannot handle SIGTERM: {e}"),
            }
        });
    }
}

// The unit tests ask for a stop on their own thread only, so that the other
// tests in the binary, which run beside them, never see it.
#[cfg(test)]
thread_local! {
    static TEST_STOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn test_stop() -> bool {
    TEST_STOP.with(|s| s.get())
}

#[cfg(not(test))]
fn test_stop() -> bool {
    false
}

// The tools each unit test's thread has registered: a test's stop reaches
// only these, not the tools of the tests running beside it.
#[cfg(test)]
thread_local! {
    static TEST_TOOLS: std::cell::RefCell<BTreeSet<u32>> =
        const { std::cell::RefCell::new(BTreeSet::new()) };
}

/// Whether a stop on this thread reaches `pid` (every tool, outside the
/// unit tests).
#[cfg(test)]
fn reaches(pid: u32) -> bool {
    TEST_TOOLS.with(|t| t.borrow().contains(&pid))
}

#[cfg(not(test))]
fn reaches(_pid: u32) -> bool {
    true
}

/// Whether a stop has been asked for.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst) || test_stop()
}

/// Whether the stop has reached the tools (as seen from this thread, in the
/// unit tests).
fn tools_stopped() -> bool {
    TOOLS_STOPPED.load(Ordering::SeqCst) && (cfg!(not(test)) || test_stop())
}

/// `Err(Stopped)` when a stop has been asked for and nothing holds it back.
pub fn check() -> Result<()> {
    if requested() && HOLDS.load(Ordering::SeqCst) == 0 {
        Err(PipesError::Stopped)
    } else {
        Ok(())
    }
}

/// Holds a stop back while alive.
pub struct Hold(());

/// Hold a stop back from now on, or `Err(Stopped)` when a stop came first:
/// the window must not open once the stop has been decided. The count goes
/// up before the stop flag is read, and the signal thread sets the flag
/// before it reads the count, so one of the two always sees the other.
pub fn hold() -> Result<Hold> {
    HOLDS.fetch_add(1, Ordering::SeqCst);
    if requested() {
        // The signal thread may have seen this hold and left the tools to
        // it: they get the stop now.
        if HOLDS.fetch_sub(1, Ordering::SeqCst) == 1 {
            stop_tools();
        }
        return Err(PipesError::Stopped);
    }
    Ok(Hold(()))
}

impl Drop for Hold {
    fn drop(&mut self) {
        if HOLDS.fetch_sub(1, Ordering::SeqCst) == 1 && requested() {
            stop_tools();
        }
    }
}

/// A tool the run is waiting for; it is signalled on a stop until dropped.
/// Drop it only once the tool has been reaped, so its pid stays its own.
pub struct Registered(u32);

pub fn register(pid: u32) -> Registered {
    lock_tools().insert(pid);
    #[cfg(test)]
    TEST_TOOLS.with(|t| t.borrow_mut().insert(pid));
    // A tool started after the stop reached the others gets it too, unless
    // it is one the stop allows (a failure hook).
    if tools_stopped() && EXEMPT.load(Ordering::SeqCst) == 0 {
        terminate(pid);
    }
    Registered(pid)
}

/// While alive, tools started after a stop run: the failure hooks, which
/// report the stop.
pub struct Exempt(());

pub fn exempt() -> Exempt {
    EXEMPT.fetch_add(1, Ordering::SeqCst);
    Exempt(())
}

impl Drop for Exempt {
    fn drop(&mut self) {
        EXEMPT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        lock_tools().remove(&self.0);
        #[cfg(test)]
        TEST_TOOLS.with(|t| t.borrow_mut().remove(&self.0));
    }
}

fn lock_tools() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    TOOLS.lock().unwrap_or_else(|e| e.into_inner())
}

fn on_signal() {
    if SIGNALS.fetch_add(1, Ordering::SeqCst) == 0 {
        eprintln!("fhir-pipes: stopping (a second signal stops at once)");
        request();
    } else {
        for pid in lock_tools().iter() {
            signal(*pid, Sig::Kill);
        }
        eprintln!("fhir-pipes: stopped at once");
        std::process::exit(1);
    }
}

fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
    if HOLDS.load(Ordering::SeqCst) == 0 {
        stop_tools();
    }
}

/// SIGTERM to every registered tool, once per stop, then SIGKILL to those
/// still registered after TOOL_GRACE.
fn stop_tools() {
    if TOOLS_STOPPED.swap(true, Ordering::SeqCst) {
        return;
    }
    let pids: Vec<u32> = lock_tools()
        .iter()
        .copied()
        .filter(|p| reaches(*p))
        .collect();
    for pid in pids {
        terminate(pid);
    }
}

/// SIGTERM to `pid`, and SIGKILL once the grace period is over if it is
/// still registered (a registration ends only once its tool is reaped, so
/// the pid is still that tool's). Each tool the stop reaches gets its own
/// timer, those registered after the stop included; a failure hook, which
/// the stop does not reach, gets none.
fn terminate(pid: u32) {
    signal(pid, Sig::Term);
    std::thread::spawn(move || {
        std::thread::sleep(tool_grace());
        if lock_tools().contains(&pid) {
            signal(pid, Sig::Kill);
        }
    });
}

enum Sig {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal(pid: u32, sig: Sig) {
    let sig = match sig {
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    };
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) with a pid the run started and has not reaped.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _sig: Sig) {}

#[cfg(test)]
pub fn request_for_test() {
    TEST_STOP.with(|s| s.set(true));
    if HOLDS.load(Ordering::SeqCst) == 0 {
        stop_tools();
    }
}

#[cfg(test)]
pub fn set_grace_for_test(d: Duration) {
    TOOL_GRACE_MS.store(d.as_millis() as u64, Ordering::SeqCst);
}

#[cfg(test)]
pub fn reset_for_test() {
    TOOL_GRACE_MS.store(10_000, Ordering::SeqCst);
    TEST_STOP.with(|s| s.set(false));
    REQUESTED.store(false, Ordering::SeqCst);
    SIGNALS.store(0, Ordering::SeqCst);
    HOLDS.store(0, Ordering::SeqCst);
    EXEMPT.store(0, Ordering::SeqCst);
    TOOLS_STOPPED.store(false, Ordering::SeqCst);
    let mine = TEST_TOOLS.with(|t| std::mem::take(&mut *t.borrow_mut()));
    lock_tools().retain(|p| !mine.contains(p));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// The stop state is process-wide: these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn check_is_ok_until_a_stop() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        assert!(!requested());
        check().unwrap();
        request_for_test();
        assert!(requested());
        assert!(matches!(check(), Err(PipesError::Stopped)));
        reset_for_test();
    }

    #[test]
    fn a_hold_defers_the_stop_until_dropped() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let held = hold().unwrap();
        let inner = hold().unwrap();
        request_for_test();
        check().unwrap();
        drop(inner);
        check().unwrap();
        drop(held);
        assert!(matches!(check(), Err(PipesError::Stopped)));
        reset_for_test();
    }

    #[test]
    fn a_hold_after_a_stop_is_refused() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        request_for_test();
        assert!(matches!(hold(), Err(PipesError::Stopped)));
        // Refused holds do not count: nothing is held back.
        assert!(matches!(check(), Err(PipesError::Stopped)));
        reset_for_test();
    }

    #[test]
    #[cfg(unix)]
    fn tools_started_after_a_stop_under_exempt_are_left_running() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        request_for_test();
        let exempt = exempt();
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .unwrap();
        let _r = register(child.id());
        assert!(
            child.wait().unwrap().success(),
            "a failure hook was stopped"
        );
        drop(exempt);
        reset_for_test();
    }

    #[test]
    #[cfg(unix)]
    fn a_tool_registered_after_the_stop_gets_sigkill_too() {
        use std::os::unix::process::ExitStatusExt;
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        set_grace_for_test(Duration::from_millis(300));
        request_for_test();
        // Ignores SIGTERM, as a stuck tool might.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let _r = register(child.id());
        let started = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(started.elapsed() < Duration::from_secs(3), "never killed");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(9));
        reset_for_test();
    }

    #[test]
    fn a_stopped_error_exits_1_and_says_stopped_by_signal() {
        let err = anyhow::Error::from(PipesError::Stopped);
        assert_eq!(crate::error::exit_code(&err), 1);
        assert_eq!(err.to_string(), "stopped by signal");
    }

    #[test]
    #[cfg(unix)]
    fn registered_tools_get_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _r = register(child.id());
        request_for_test();
        let started = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(started.elapsed() < Duration::from_secs(2), "not stopped");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(15));
        reset_for_test();
    }

    #[test]
    #[cfg(unix)]
    fn a_held_stop_reaches_tools_when_the_hold_ends() {
        use std::os::unix::process::ExitStatusExt;
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _r = register(child.id());
        let held = hold().unwrap();
        request_for_test();
        std::thread::sleep(Duration::from_millis(200));
        assert!(child.try_wait().unwrap().is_none(), "signalled while held");
        drop(held);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(15));
        reset_for_test();
    }
}
