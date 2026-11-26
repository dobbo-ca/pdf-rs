//! The peak of live threads during a call against `Limits::max_threads` (round-c item 6), and
//! `max_threads = 1` leaving the shared pool unstarted. Its own test binary, so no other test
//! starts the pool, and one `#[test]`, so the harness runs nothing else beside it. The thread
//! count is read from `/proc/self/task`, so this is Linux only; `par::Gauge` is not wired to any
//! call yet (phase 1 is serial, docs/threading.md).

#![cfg(target_os = "linux")]

use pdf::{Config, Limits, WriteEnv, api};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

fn live_threads() -> usize {
    fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
}

/// Reads, validates, optimizes and writes each fixture with `max_threads`, while a sampler thread
/// records the most threads of the process. Returns the count before the calls (the sampler
/// included) and the peak.
fn run(max_threads: usize) -> (usize, usize) {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let (stop, peak) = (stop.clone(), peak.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(live_threads(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(50));
            }
        })
    };
    while peak.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    let base = live_threads();

    let cfg = Config {
        limits: Limits {
            max_threads,
            ..Limits::default()
        },
        ..Config::default()
    };
    let cancel = AtomicBool::new(false);
    for name in ["testWithText.pdf", "bookletTestA6.pdf", "Walden.pdf"] {
        let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
        let data = fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut ctx = api::read_validate_and_optimize(&data, &cfg, &cancel)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        api::write_context(&mut ctx, &WriteEnv::parity(), &cancel)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap_or_default();
    (base, peak.load(Ordering::Relaxed).max(base))
}

#[test]
fn threads_stay_within_max_threads() {
    // max_threads = 1 first: no thread is spawned and the shared pool is not started.
    let (base, peak) = run(1);
    assert_eq!(peak, base, "max_threads 1 spawned a thread");
    assert!(
        !pdf::par::shared_pool_started(),
        "max_threads 1 started the shared pool"
    );

    // The calling thread counts in max_threads, so a call adds at most max_threads - 1 threads.
    for max_threads in [2, pdf::config::default_threads()] {
        let (base, peak) = run(max_threads);
        assert!(
            peak < base + max_threads,
            "max_threads {max_threads}: {peak} threads, {base} before the call"
        );
    }
}
