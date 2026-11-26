# Safety

## No `unsafe`

The crate has `#![forbid(unsafe_code)]` and no SIMD kernel, so there is no `unsafe` block, no
`unsafe fn`, no FFI and no `-sys` or `cc`/`bindgen` dependency to inventory. Threading (see
[threading.md](threading.md)) uses rayon's safe scope API. If a benchmark ever demands a kernel,
the convention applies: switch the root to `#![deny(unsafe_code)]`, put `#[allow(unsafe_code)]` on
that module only, validate every length at its boundary in safe code, and give each `unsafe`
block a `// SAFETY:` comment naming the invariant and where it is checked.

The only non-Rust code in the repository is the `libfuzzer-sys` dependency of the separate fuzz
crate (`tests/fuzz`), which the conventions allow, and the Go oracle under `tests/parity`, which
is a test tool and not part of the library.

## Hostile input

pdf-rs reads untrusted files. What stands between the input and the allocator or the CPU:

- **Limits first.** An input-controlled size passes the limits ([limits.md](limits.md)) before
  anything is allocated from it, and allocation is `Vec::try_reserve_exact`, mapped to
  `Error::Alloc` (the ledger refused) or `Error::OutOfMemory` (the allocator refused a size that
  is within the limits). `max_alloc` is charged before the allocation, by all threads together.
- **No panics in library code.** Outside tests the crate denies `clippy::unwrap_used`,
  `clippy::expect_used`, `clippy::panic`, `clippy::todo` and `clippy::unimplemented`. Arithmetic on
  input values is `checked_*` or saturating and returns an error.
- **Go's panics are errors.** Where pdf-go panics on hostile input (an index out of range, a nil
  dereference), the port returns `Error::Panic` at the same site, so a caller sees the same
  failure as byblos, which recovers those panics itself. `Error::class()` is `panic`; the parity
  oracle compares it with Go's recovered panic.
- **Recursion is bounded.** `max_recursion_depth` (default 100) limits the object parser and every
  graph traversal; page, name, number, structure, annotation, action, function, pattern, form
  field and other trees have cycle and duplicate checks that return the sentinels Go has.
- **Cancellation.** A call checks the cancel flag per object, row or stream, so a hostile input
  that is slow rather than large can be stopped.
- **Decode bombs.** A stream is decoded under `max_decode_bytes`, a document under
  `max_total_decode_bytes`, an image under `max_image_pixels` and `max_image_bytes`. The filters
  stop reading at the limit plus one byte, as Go's do.

## Checks

- Clippy runs with `-D warnings` in the `test` job; the lints above make an `unwrap` in library
  code a build failure.
- The fuzz targets ([fuzzing.md](fuzzing.md)) run every parser entry point under
  `-rss_limit_mb=2048 -malloc_limit_mb=512`, so an allocation outside the limits is a crash. They
  also check the ledger: the peak never passes `max_alloc`, and dropping a document gives every
  charged byte back.
- The parity job ([parity.md](parity.md)) compares the output and the error class with Go on the
  whole corpus, its mutations and the limit boundaries.
