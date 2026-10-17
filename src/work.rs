//! One work queue on scoped threads, for the commands that run requests in
//! parallel (export, extract boundaries, load, dhis2 reports).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};

/// The thread count when none is given: one per available core.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Runs `work` on `workers` scoped threads over the items `next` hands out.
/// `next` is called on the worker threads one call at a time, so items are
/// taken in order, until it returns `None` or the run is stopped. `on_done`
/// runs on the calling thread with each result as it arrives, in the order
/// the work finishes. When it returns false no new item is taken; the items
/// already being worked on finish and still reach `on_done`. Returns once
/// every worker has finished.
pub fn run<T, R>(
    workers: usize,
    next: impl FnMut() -> Option<T> + Send,
    work: impl Fn(T) -> R + Sync,
    mut on_done: impl FnMut(R) -> bool,
) where
    R: Send,
{
    let source = Mutex::new(next);
    let stop = AtomicBool::new(false);
    // Bounded, so workers wait for a slow `on_done` rather than run ahead.
    let (tx, rx) = mpsc::sync_channel::<R>(workers.max(1) * 4);
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            let tx = tx.clone();
            let (source, stop, work) = (&source, &stop, &work);
            s.spawn(move || {
                loop {
                    let item = {
                        let mut next = source.lock().unwrap_or_else(PoisonError::into_inner);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match next() {
                            Some(item) => item,
                            None => break,
                        }
                    };
                    if tx.send(work(item)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for r in rx {
            if !on_done(r) {
                stop.store(true, Ordering::Relaxed);
            }
        }
    });
}

/// Indices `from..to`, one per call: a `next` for `run` over a list.
pub fn indices(from: usize, to: usize) -> impl FnMut() -> Option<usize> + Send {
    let mut i = from;
    move || {
        (i < to).then(|| {
            i += 1;
            i - 1
        })
    }
}

/// Results that finish out of order, given back in order of their index.
pub struct InOrder<R> {
    next: usize,
    held: HashMap<usize, R>,
}

impl<R> InOrder<R> {
    /// The first index expected is `start`.
    pub fn new(start: usize) -> Self {
        Self {
            next: start,
            held: HashMap::new(),
        }
    }

    pub fn push(&mut self, i: usize, r: R) {
        self.held.insert(i, r);
    }

    /// The next result in order, once it has arrived.
    pub fn pop(&mut self) -> Option<(usize, R)> {
        let r = self.held.remove(&self.next)?;
        self.next += 1;
        Some((self.next - 1, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn every_item_is_worked_on_once() {
        let mut seen = vec![0; 100];
        run(
            4,
            indices(0, 100),
            |i| (i, i * 2),
            |(i, r)| {
                seen[i] += 1;
                assert_eq!(r, i * 2);
                true
            },
        );
        assert!(seen.iter().all(|&n| n == 1));
    }

    #[test]
    fn false_from_on_done_stops_new_items_but_delivers_running_ones() {
        let started = AtomicUsize::new(0);
        let mut done = 0;
        run(
            2,
            indices(0, 100),
            |i| {
                started.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(5));
                i
            },
            |_| {
                done += 1;
                false
            },
        );
        let started = started.into_inner();
        assert!(started < 100, "{started}");
        assert_eq!(done, started);
    }

    #[test]
    fn next_is_called_in_order_one_call_at_a_time() {
        let mut calls = Vec::new();
        let mut i = 0;
        let mut taken = Vec::new();
        run(
            8,
            || {
                if i == 50 {
                    return None;
                }
                calls.push(i);
                i += 1;
                Some(i - 1)
            },
            |i| i,
            |i| {
                taken.push(i);
                true
            },
        );
        assert_eq!(calls, (0..50).collect::<Vec<_>>());
        taken.sort_unstable();
        assert_eq!(taken, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn in_order_holds_back_early_results() {
        let mut order = InOrder::new(2);
        order.push(3, "d");
        assert!(order.pop().is_none());
        order.push(2, "c");
        assert_eq!(order.pop(), Some((2, "c")));
        assert_eq!(order.pop(), Some((3, "d")));
        assert!(order.pop().is_none());
    }
}
