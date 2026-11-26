//! Shared by the targets (`#[path = "../common.rs"] mod common;`): the limits they run under and
//! the checks every document target makes.
#![allow(dead_code)]

use pdf::filter::{Parms, new_filter};
use pdf::model::Context;
use pdf::{Cmd, Config, Limits, Mem};
use std::sync::atomic::AtomicBool;

/// Limits that keep one input well under the `-malloc_limit_mb=512` and `-rss_limit_mb=2048` of
/// the fuzz run: a decoded stream of at most 4 MiB, 16 MiB of decoded bytes per document and
/// 128 MiB of working memory. Everything else is Go's default or a smaller cap.
pub const TIGHT: Limits = Limits {
    max_input_bytes: 8 << 20,
    max_object_bytes: 1 << 20,
    max_stream_bytes: 8 << 20,
    max_decode_bytes: 4 << 20,
    max_total_decode_bytes: 16 << 20,
    max_image_pixels: 1 << 22,
    max_image_bytes: 16 << 20,
    max_object_count: 100_000,
    max_object_stream_count: 10_000,
    max_object_stream_total_count: 100_000,
    max_parsed_bytes: 32 << 20,
    max_stream_total_bytes: 32 << 20,
    max_object_stream_first: 1 << 20,
    max_xref_entries: 100_000,
    max_recursion_depth: 100,
    max_alloc: 128 << 20,
    max_threads: 1,
};

/// The configuration of the document targets.
pub fn config(cmd: Cmd) -> Config {
    Config { limits: TIGHT, cmd }
}

/// A cancel flag that is never set: the targets fuzz the work, not the cancellation.
pub fn never() -> AtomicBool {
    AtomicBool::new(false)
}

/// Touches the page tree of a document that was read: up to 8 pages.
pub fn walk_pages(ctx: &mut Context, cancel: &AtomicBool) {
    if ctx.xref.ensure_page_count().is_err() {
        return;
    }
    for n in 1..=ctx.xref.page_count.min(8) {
        let _ = ctx.xref.page_dict(n, cancel);
    }
}

/// The ledger's contract after a call: the peak never went over `max_alloc`, and dropping the
/// document gives every charged byte back.
pub fn settle(ctx: Context) {
    let mem: Mem = ctx.xref.mem().clone();
    assert!(
        mem.peak() <= TIGHT.max_alloc,
        "peak {} over max_alloc",
        mem.peak()
    );
    drop(ctx);
    assert_eq!(mem.used(), 0, "bytes still charged after the drop");
}

/// Exercises what a failed call reports.
pub fn report(e: &pdf::Error) {
    let _ = e.class();
    let _ = e.to_string();
}

/// What a parameter byte stands for: `None` leaves the key out.
const VALUES: [Option<i64>; 16] = [
    None,
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(8),
    Some(10),
    Some(12),
    Some(14),
    Some(15),
    Some(16),
    Some(99),
    Some(-1),
    Some(1 << 20),
    Some(1728),
];

/// The per-stream decode limit of the filter targets, as `new_filter` takes it.
pub fn decode_limit() -> i64 {
    i64::try_from(TIGHT.decode_limit()).unwrap_or(i64::MAX)
}

/// Decodes `data` with the filter `name`. The first byte of the input for each of `keys` picks
/// that decode parameter from `VALUES`; the rest is the stream. Checks the limits and the
/// ledger: the output is within the decode limit, its Go capacity is not below its length, the
/// peak is within `max_alloc` and everything is given back.
pub fn run_filter(name: &[u8], keys: &[&[u8]], data: &[u8]) {
    let (head, body) = data.split_at(keys.len().min(data.len()));
    let mut parms = Parms::new();
    for (key, b) in keys.iter().zip(head) {
        if let Some(v) = VALUES[usize::from(*b) % 16] {
            parms.insert(key.to_vec(), v);
        }
    }
    let f = match new_filter(name, parms, Some(decode_limit())) {
        Ok(f) => f,
        Err(e) => return report(&e),
    };
    let mem = Mem::new(&TIGHT);
    let cancel = never();
    match f.decode(body, &mem, &cancel) {
        Ok(d) => {
            assert!(
                d.len() as u64 <= TIGHT.decode_limit(),
                "output over the decode limit"
            );
            assert!(d.go_cap() >= d.len(), "capacity under length");
        }
        Err(e) => report(&e),
    }
    // A prefix read: the filters that can stop early do.
    let max_len = i64::from(data.first().copied().unwrap_or(0)) * 64;
    if let Err(e) = f.decode_length(body, max_len, &mem, &cancel) {
        report(&e);
    }
    assert!(mem.peak() <= TIGHT.max_alloc, "peak over max_alloc");
    assert_eq!(mem.used(), 0, "bytes still charged after the drop");
}
