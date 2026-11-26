# pdf-rs

Reading, validating, optimizing and writing PDF files under strict resource limits, in Rust: the
part of [dobbo-ca/pdf-go](https://github.com/dobbo-ca/pdf-go) (a pdfcpu fork) at 113d693 that
byblos uses, output-compatible with it. Pure Rust, no `unsafe` (`#![forbid(unsafe_code)]`).
Apache-2.0; see `NOTICE`. It depends on the deflate-rs and jpeg-rs ports
([docs/dependencies.md](docs/dependencies.md)). Out of scope: everything byblos never calls
(stamping, merging, forms, signing, ...); see [docs/scope.md](docs/scope.md).

```toml
pdf = { git = "https://github.com/dobbo-ca/pdf-rs", rev = "<sha>" }
```

## Quick start

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use pdf::{Config, Limits};
use std::sync::atomic::AtomicBool;

let data = std::fs::read("in.pdf")?;
// At most 2 threads and 256 MiB for any one call.
let cfg = Config {
    limits: Limits { max_threads: 2, max_alloc: 256 << 20, ..Limits::default() },
    ..Config::default()
};
let cancel = AtomicBool::new(false);
let mut ctx = pdf::api::read_and_validate(&data, &cfg, &cancel)?;
ctx.xref.ensure_page_count()?;
println!("{} pages", ctx.xref.page_count);
let out = pdf::api::write_context(&mut ctx, &pdf::WriteEnv::default(), &cancel)?;
println!("{} bytes", out.len());
# Ok(())
# }
```

## Resource limits

Every entry point takes the `Limits` through its `Config`; there is no unlimited call. A long call
takes `cancel` and returns `Error::Cancelled` at its next check. Defaults are pdf-go's, except
`max_alloc` and `max_threads`, which exist only here.

| Field | Default | Unit; over it |
|---|---|---|
| `max_input_bytes` | 0 (off) | bytes of the input: `Limit(InputBytes)` |
| `max_object_bytes` | 64 MiB | bytes of one object, stream excluded: `Limit(ObjectBytes)` |
| `max_stream_bytes` | 512 MiB | encoded bytes of a stream without `/Length`: untyped `Corrupt`, as in Go |
| `max_decode_bytes` | 512 MiB | decoded bytes of one stream: `DecodeLimit` |
| `max_total_decode_bytes` | 0 (1 GiB) | Go slice capacity decoded per document: `Limit(DecodeBudget)` |
| `max_image_pixels`, `max_image_bytes` | 100 M, 512 MiB | one image: `ImageLimit` |
| `max_object_count`, `max_xref_entries` | 10 M each | objects, entries of one xref section: `XRefObjectRange` / untyped `Corrupt`, as in Go |
| `max_object_stream_count`, `_first`, `_total_count` | 1 M, 16 MiB, 0 (off) | `/N`, `/First`: untyped `Corrupt`; entries of all: `Limit(ObjectStreamEntries)` |
| `max_parsed_bytes`, `max_stream_total_bytes` | 0 (off) | estimated parsed heap, encoded bytes read: `Limit(..)` |
| `max_recursion_depth` | 100 | levels of parsing and traversal: `Sentinel::MaxRecursionDepth` |
| `max_alloc` | 4 GiB | bytes, the enforced peak of one document and call, all threads, nested crates (deflate-rs) included: `Alloc` |
| `max_threads` | `available_parallelism` | threads, the caller included; 0 counts as 1 |

- `max_alloc` is charged before every input-sized allocation and released on drop.
- Several documents of one call share one ledger through the `_with_mem` entry points
  (`api::read_with_mem` and its siblings): the caller's `Mem` caps the whole call and each
  document's `max_alloc` is not used (docs/limits.md, "Shared ledger").
- Every call is serial today: `max_threads` is honoured because no thread is spawned, and no entry
  point takes a `ThreadPool`. The output is the same for every value.

## Docs

[limits](docs/limits.md), [threading](docs/threading.md), [safety](docs/safety.md),
[go-differences](docs/go-differences.md), [parity](docs/parity.md), [fuzzing](docs/fuzzing.md),
[benchmarks](docs/benchmarks.md), [design](docs/design.md).
