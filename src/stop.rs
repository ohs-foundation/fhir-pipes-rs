//! Stopping a run on SIGTERM or Ctrl-C (Unix).
//!
//! The first signal sets a flag that `check` reports as `PipesError::Stopped`
//! at the points a run can stop (between steps, resource types and pages),
//! and sends SIGTERM to the tools the run has registered, then SIGKILL to
//! those still running 10 s later. While a `Hold` is alive (the commit
//! window, from the database step to the end of the publish) `check` returns
//! `Ok` and the tools are left alone; the stop takes effect when the last
//! hold ends. A second signal kills the tools and exits at once, held or not.
//!
//! On Windows `install` does nothing, so Ctrl-C ends the process as before.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::error::{PipesError, Result};

/// How long a tool has after SIGTERM before it gets SIGKILL.
const TOOL_GRACE: Duration = Duration::from_secs(10);

static REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicUsize = AtomicUsize::new(0);
static HOLDS: AtomicUsize = AtomicUsize::new(0);
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

/// Whether a stop has been asked for.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
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

pub fn hold() -> Hold {
    HOLDS.fetch_add(1, Ordering::SeqCst);
    Hold(())
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
    // A tool started after the stop reached the others gets it too.
    if TOOLS_STOPPED.load(Ordering::SeqCst) {
        signal(pid, Sig::Term);
    }
    Registered(pid)
}

impl Drop for Registered {
    fn drop(&mut self) {
        lock_tools().remove(&self.0);
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
    let pids: Vec<u32> = lock_tools().iter().copied().collect();
    for pid in &pids {
        signal(*pid, Sig::Term);
    }
    std::thread::spawn(|| {
        std::thread::sleep(TOOL_GRACE);
        for pid in lock_tools().iter() {
            signal(*pid, Sig::Kill);
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
    SIGNALS.fetch_add(1, Ordering::SeqCst);
    request();
}

#[cfg(test)]
pub fn reset_for_test() {
    REQUESTED.store(false, Ordering::SeqCst);
    SIGNALS.store(0, Ordering::SeqCst);
    HOLDS.store(0, Ordering::SeqCst);
    TOOLS_STOPPED.store(false, Ordering::SeqCst);
    lock_tools().clear();
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
        let held = hold();
        let inner = hold();
        request_for_test();
        check().unwrap();
        drop(inner);
        check().unwrap();
        drop(held);
        assert!(matches!(check(), Err(PipesError::Stopped)));
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
        let held = hold();
        request_for_test();
        std::thread::sleep(Duration::from_millis(200));
        assert!(child.try_wait().unwrap().is_none(), "signalled while held");
        drop(held);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(15));
        reset_for_test();
    }
}
