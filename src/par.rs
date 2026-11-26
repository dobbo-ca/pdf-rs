//! The worker pool and the thread gauge. Phase 1 of the port is serial (docs/design.md section
//! 9): nothing in the crate runs on a pool yet, `Limits::max_threads` is accepted and the output
//! is the same for every value. This module is tiff-rs's `par.rs` without the jpeg sub-pool, so
//! the one parallel site of phase 2 (a speculative prefetch of object-stream decodes, committed
//! in Go's order) has its pool ready. Nothing calls it today and no entry point takes a pool, so
//! `Limits::max_threads` is honoured because no thread is spawned; when a phase uses it, a call's
//! workers must stay at most `max_threads`, the calling thread included, and `max_threads = 1`
//! must never touch a pool. Rayon's scope lets a pool thread borrow the call's
//! buffers, which `std::thread::scope` cannot do on a reused thread, so nothing here is `unsafe`.

use crate::Error;
use crate::config::default_threads;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// The stack of a pool thread: the decoders keep their large buffers on the heap.
const THREAD_STACK: usize = 512 << 10;

/// Worker threads for a later phase. Unused today: no entry point takes a pool, and the scope
/// below does not cap its workers at `Limits::max_threads` (the caller of a future phase must).
/// Clones share the threads, which stop when the last clone drops. A pool is not charged to
/// `Limits::max_alloc`; its threads' stacks are its owner's.
#[derive(Clone, Debug)]
pub struct ThreadPool {
    pool: Arc<rayon::ThreadPool>,
}

impl ThreadPool {
    /// A pool of `threads` workers (at least 1), started now.
    ///
    /// # Errors
    /// [`Error::OutOfMemory`] if the operating system will not start the threads.
    pub fn new(threads: usize) -> Result<ThreadPool, Error> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .stack_size(THREAD_STACK)
            .thread_name(|i| format!("pdf-worker-{i}"))
            .build()
            .map_err(|_| Error::OutOfMemory)?;
        Ok(ThreadPool {
            pool: Arc::new(pool),
        })
    }

    /// How many worker threads the pool has.
    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Runs `op` in a scope whose spawned jobs may borrow the caller's buffers; the calling
    /// thread takes part. Returns when every job has finished.
    pub fn scope<'scope, OP, R>(&self, op: OP) -> R
    where
        OP: FnOnce(&rayon::Scope<'scope>) -> R,
    {
        self.pool.in_place_scope(op)
    }
}

/// The crate's shared pool: built on first use, `available_parallelism` threads, no other state.
/// `None` if the threads could not be started (calls then run serially).
static SHARED: OnceLock<Option<ThreadPool>> = OnceLock::new();

/// The lazy shared pool, for a later phase. Nothing calls it today; a call with `max_threads` 1
/// must never call it.
pub fn shared_pool() -> Option<&'static ThreadPool> {
    SHARED
        .get_or_init(|| ThreadPool::new(default_threads()).ok())
        .as_ref()
}

/// Whether the shared pool has been started (for tests: `max_threads` 1 never starts it).
#[doc(hidden)]
pub fn shared_pool_started() -> bool {
    SHARED.get().is_some()
}

/// The threads of one call that are running now and the most at once, the calling thread
/// included. Per call, so a test can read it without global state.
#[doc(hidden)]
#[derive(Debug)]
pub struct Gauge {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

impl Gauge {
    pub fn new() -> Self {
        Self {
            live: AtomicUsize::new(1),
            peak: AtomicUsize::new(1),
        }
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// A pool worker counted in a [`Gauge`] while it runs jobs.
#[doc(hidden)]
pub struct Live<'a>(&'a Gauge);

impl<'a> Live<'a> {
    pub fn enter(g: &'a Gauge) -> Self {
        let n = g.live.fetch_add(1, Ordering::Relaxed) + 1;
        g.peak.fetch_max(n, Ordering::Relaxed);
        Live(g)
    }
}

impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_counts_the_caller_and_the_peak() {
        let g = Gauge::new();
        assert_eq!(g.peak(), 1);
        {
            let _a = Live::enter(&g);
            let _b = Live::enter(&g);
            assert_eq!(g.peak(), 3);
        }
        let _c = Live::enter(&g);
        assert_eq!(g.peak(), 3);
    }

    #[test]
    fn pool_runs_a_scope_that_borrows() {
        let p = ThreadPool::new(0).unwrap();
        assert_eq!(p.threads(), 1);
        let mut out = [0u8; 4];
        p.scope(|s| {
            for (i, slot) in out.iter_mut().enumerate() {
                s.spawn(move |_| *slot = i as u8 + 1);
            }
        });
        assert_eq!(out, [1, 2, 3, 4]);
    }
}
