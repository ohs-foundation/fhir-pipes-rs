//! The limit on requests in flight from the threads sharing a client
//! (`with_concurrency`), and the pause they all wait out after a
//! Retry-After.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::FhirClient;

/// A limit on the requests in flight from all the threads sharing a client
/// (`export --concurrency`), and a pause they all wait out after a response
/// with a Retry-After, so parallel downloads back off together. A request
/// holds its place from sending until its body is read, not while it waits
/// to retry.
pub(super) struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    limit: usize,
    in_flight: usize,
    paused_until: Option<Instant>,
}

/// A place in the gate, given back when dropped.
pub(super) struct Pass<'a>(&'a Gate);

impl Gate {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(GateState {
                limit: limit.max(1),
                in_flight: 0,
                paused_until: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait for a pause to end and a place to be free, giving up when
    /// `deadline` passes first.
    pub(super) fn enter_by(&self, deadline: Option<Instant>) -> Option<Pass<'_>> {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            if deadline.is_some_and(|d| d <= now) {
                return None;
            }
            let wait = |until: Option<Instant>| {
                // The earlier of the pause's end and the deadline; neither
                // is a wait for a place.
                match (until, deadline) {
                    (Some(u), Some(d)) => Some(u.min(d) - now),
                    (u, d) => u.or(d).map(|t| t - now),
                }
            };
            let paused = state.paused_until.filter(|u| *u > now);
            if paused.is_none() && state.in_flight < state.limit {
                state.in_flight += 1;
                return Some(Pass(self));
            }
            state = match wait(paused) {
                Some(w) => {
                    self.changed
                        .wait_timeout(state, w)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    /// No request starts for `wait`, or until an earlier pause ends.
    pub(super) fn pause(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut state = self.lock();
        if state.paused_until.is_none_or(|u| u < until) {
            state.paused_until = Some(until);
        }
    }
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.lock().in_flight -= 1;
        // All, not one: a thread waiting out a pause must not take the
        // wake-up meant for one waiting for a place.
        self.0.changed.notify_all();
    }
}

impl FhirClient {
    /// A place in the gate, when the client has one, or Err when `deadline`
    /// passes before one is free.
    pub(super) fn enter_by(
        &self,
        deadline: Option<Instant>,
    ) -> std::result::Result<Option<Pass<'_>>, ()> {
        match &self.gate {
            None => Ok(None),
            Some(gate) => gate.enter_by(deadline).map(Some).ok_or(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::tests::client;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    #[test]
    fn the_gate_lets_in_only_its_limit() {
        let gate = Gate::new(2);
        let a = gate.enter_by(None).unwrap();
        let _b = gate.enter_by(None).unwrap();
        let entered = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _c = gate.enter_by(None).unwrap();
                entered.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
            drop(a);
        });
        assert!(entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_pause_holds_every_request_back() {
        let gate = Gate::new(4);
        gate.pause(Duration::from_millis(200));
        // A shorter pause does not cut the longer one short.
        gate.pause(Duration::from_millis(10));
        let started = Instant::now();
        let _pass = gate.enter_by(None).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn a_wait_for_the_gate_gives_up_at_the_deadline() {
        let gate = Gate::new(1);
        gate.pause(Duration::from_secs(5));
        let started = Instant::now();
        assert!(
            gate.enter_by(Some(started + Duration::from_millis(200)))
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // No pause, no free place: the same.
        let gate = Gate::new(1);
        let _held = gate.enter_by(None).unwrap();
        let started = Instant::now();
        assert!(
            gate.enter_by(Some(started + Duration::from_millis(200)))
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_retry_after_pauses_the_other_threads_sharing_the_client() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/slow"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).insert_header("retry-after", "1"),
                    status_code(200),
                ]),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/other"))
                .times(1)
                .respond_with(status_code(200)),
        );
        let client = client(1).with_concurrency(4);
        client.get(&server.url_str("/slow")).unwrap_err();
        // The 429's Retry-After holds back a request to another URL too.
        let started = Instant::now();
        client.get(&server.url_str("/other")).unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        client.get(&server.url_str("/slow")).unwrap();
    }
}
