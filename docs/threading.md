# Threading

`Limits::max_threads` is the most OS threads one call uses, the calling thread included. 1 spawns
no thread and touches no pool; 0 counts as 1. Every call is serial today, so `max_threads` is
honoured because no thread is spawned, and no entry point takes a `ThreadPool`. The default is
`std::thread::available_parallelism()`, read once. The memory budget (`max_alloc`) covers all
threads together, and the cancel flag stops all of them. The output is byte-identical for every
value of `max_threads`: the parity job runs the Rust oracle with `-threads 1` and with the default,
and both outputs must equal Go's.

## Phase 1: serial

No in-scope pdf-go code starts a goroutine, and Go's budgets are order-dependent (the cumulative
decode budget is charged in the order Go touches streams). So the whole crate runs on the calling
thread today, and `max_threads` is accepted and documented but nothing yet runs on a pool. Parse,
validate, optimize, the writer, `extract_image` (it changes the shared decode budget) and PNG
deflate (bit-exact with Go) are serial on purpose.

## Phase 2: object-stream prefetch (WP 29)

Only if the benchmark ([benchmarks.md](benchmarks.md)) is above 1.10 times Go on the object-stream
or validate classes: workers decode object streams speculatively with the per-stream limit, and the
calling thread commits each result at the point where Go first touches that stream, applying the
entry count, the capacity budget and `max_parsed_bytes` there. A worker that cannot charge the
ledger drops its prefetch and that stream decodes serially, so speculation never creates an error.

## The pool

`pdf::par::ThreadPool` (src/par.rs, internal, `#[doc(hidden)]`) is tiff-rs's pool without the jpeg sub-pool:

- `ThreadPool::new(n)` makes a pool. No entry point takes one (there is no `ThreadPool` parameter
  on `Config` or any `api` function), and the module is `#[doc(hidden)]`: it is ready for phase 2.
- The crate's lazy shared pool (`OnceLock`, `par::shared_pool()`) is created on the first call that
  needs it, never more than `available_parallelism` threads. Nothing calls it today, so
  `par::shared_pool_started()` is false for every call. `ThreadPool::scope` does not cap its
  workers at `max_threads`; phase 2 must.
- A pool is not charged to `max_alloc`; its threads' stacks belong to its owner.
- `par::Gauge` counts the live threads of one call; no test uses it on an entry point yet, because
  no call spawns a thread.

## Why rayon

The only third-party dependency of the pool is `rayon = "=1.12.0"`, as in tiff-rs and jpeg-rs.
Safe Rust cannot reuse a thread for jobs that borrow short-lived buffers: `std::thread::scope`
spawns a new thread per scope, which costs more than a small document's work. Rayon's
`in_place_scope` lets a pool thread borrow the call's buffers and lets the calling thread take part
in the work, and it needs no `unsafe` here. The port conventions allow a pool crate only when it
is justified; this is the justification. `Cargo.toml` pins the exact version.
