//! Bounded, ordered parallel pipeline (PRD §2.3 `acx-archive::scheduler`).
//!
//! ```text
//! caller thread        rayon pool (N threads)       consumer thread
//! produce --submit-->  work(item) ---bounded--->   reorder -> consume
//!     ^                                               |
//!     +-------------- permit returned ----------------+
//! ```
//!
//! At most `window` items exist between `submit` and `consume`, so memory
//! is bounded. Results reach `consume` in submission order, so output is
//! deterministic whatever the thread count.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::thread;

use acx_core::{AcxError, CancelFlag, ResourceBudget};

/// How many items may be in flight for `budget`: two per worker. While a
/// worker encodes, its item holds an input and an output buffer; a finished
/// item waiting to be consumed holds one. `2 × workers` items therefore use
/// at most `3 × workers` block-sized buffers, which `ResourceBudget` sized
/// the worker count for.
pub(crate) fn window(budget: &ResourceBudget) -> usize {
    budget.effective_threads() * 2
}

/// Calls `produce` on the current thread with a `submit` function. Each
/// submitted item runs through `work` on a pool of `threads` workers, and the
/// results go to `consume`, in submission order, on a dedicated thread.
///
/// `submit` blocks while `window` items are in flight and fails once the
/// consumer has stopped or `cancel` is set, so `produce` should stop on its
/// error. The consumer's error is returned in preference to the producer's,
/// since it is the cause (for example a full disk).
pub(crate) fn run_ordered<T, R>(
    threads: usize,
    window: usize,
    cancel: &CancelFlag,
    produce: impl FnOnce(&mut dyn FnMut(T) -> Result<(), AcxError>) -> Result<(), AcxError>,
    work: impl Fn(T) -> Result<R, AcxError> + Sync,
    mut consume: impl FnMut(R) -> Result<(), AcxError> + Send,
) -> Result<(), AcxError>
where
    T: Send,
    R: Send,
{
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("acx-worker-{i}"))
        .build()
        .map_err(|e| AcxError::ResourceLimit(format!("starting worker threads: {e}")))?;
    let window = window.max(1);
    let (permit_tx, permit_rx) = sync_channel::<()>(window);
    for _ in 0..window {
        permit_tx.send(()).expect("capacity is window");
    }
    let (result_tx, result_rx) = sync_channel::<(u64, Result<R, AcxError>)>(window);
    let work = &work;

    thread::scope(|s| {
        let consumer = thread::Builder::new()
            .name("acx-writer".into())
            .spawn_scoped(s, move || -> Result<(), AcxError> {
                let mut pending = BTreeMap::new();
                let mut next = 0u64;
                for (seq, result) in result_rx {
                    pending.insert(seq, result);
                    while let Some(result) = pending.remove(&next) {
                        consume(result?)?;
                        next += 1;
                        let _ = permit_tx.send(()); // producer may already be done
                    }
                }
                Ok(())
            })
            .map_err(|e| AcxError::ResourceLimit(format!("starting writer thread: {e}")))?;

        let produced = pool.in_place_scope(|scope| {
            let mut seq = 0u64;
            let mut submit = |item: T| -> Result<(), AcxError> {
                // A closed channel means the consumer failed; its error wins below.
                permit_rx.recv().map_err(|_| AcxError::Cancelled)?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(AcxError::Cancelled);
                }
                let tx = result_tx.clone();
                let n = seq;
                seq += 1;
                scope.spawn(move |_| {
                    let _ = tx.send((n, work(item))); // consumer may have stopped
                });
                Ok(())
            };
            produce(&mut submit)
        }); // waits for every spawned job
        drop(result_tx);
        let consumed = consumer
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        consumed.and(produced)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    fn run(
        threads: usize,
        window: usize,
        n: u64,
        fail_at: Option<u64>,
    ) -> (Result<(), AcxError>, Vec<u64>) {
        let mut out = Vec::new();
        let r = run_ordered(
            threads,
            window,
            &CancelFlag::default(),
            |submit| (0..n).try_for_each(&mut *submit),
            |i| {
                // Later items finish first, to exercise reordering.
                thread::sleep(Duration::from_micros(200 * (n - i) % 1000));
                if Some(i) == fail_at {
                    return Err(AcxError::Integrity(format!("item {i}")));
                }
                Ok(i)
            },
            |i| {
                out.push(i);
                Ok(())
            },
        );
        (r, out)
    }

    #[test]
    fn results_arrive_in_order_for_any_thread_count() {
        for threads in [1, 2, 8] {
            for window in [1, 3, 16] {
                let (r, out) = run(threads, window, 50, None);
                r.unwrap();
                assert_eq!(out, (0..50).collect::<Vec<_>>(), "{threads} {window}");
            }
        }
    }

    #[test]
    fn work_error_stops_the_pipeline_and_wins() {
        let (r, out) = run(4, 4, 100, Some(10));
        assert!(matches!(r, Err(AcxError::Integrity(_))), "{r:?}");
        assert_eq!(out, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn consumer_error_stops_the_producer() {
        let produced = AtomicUsize::new(0);
        let r = run_ordered(
            2,
            4,
            &CancelFlag::default(),
            |submit| {
                for i in 0..1_000u64 {
                    submit(i)?;
                    produced.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            },
            Ok,
            |i| match i {
                5 => Err(AcxError::ResourceLimit("disk full".into())),
                _ => Ok(()),
            },
        );
        assert!(matches!(r, Err(AcxError::ResourceLimit(_))), "{r:?}");
        assert!(produced.load(Ordering::Relaxed) < 20, "producer kept going");
    }

    #[test]
    fn in_flight_items_never_exceed_the_window() {
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        run_ordered(
            8,
            3,
            &CancelFlag::default(),
            |submit| {
                (0..200u64).try_for_each(|i| {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    submit(i)
                })
            },
            Ok,
            |_| {
                live.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        // One more than the window: the producer counts an item just before
        // submit blocks for its permit.
        assert!(peak.load(Ordering::SeqCst) <= 4, "{peak:?}");
    }

    #[test]
    fn cancel_stops_new_work() {
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let r = run_ordered(
            2,
            2,
            &cancel,
            |submit| {
                (0..1_000u64).try_for_each(|i| {
                    if i == 5 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    submit(i)
                })
            },
            Ok,
            |i: u64| {
                assert!(i < 5);
                Ok(())
            },
        );
        assert!(matches!(r, Err(AcxError::Cancelled)));
    }
}
