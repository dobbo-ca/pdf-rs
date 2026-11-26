//! A Rust port of the part of [dobbo-ca/pdf-go](https://github.com/dobbo-ca/pdf-go) (a hard fork
//! of pdfcpu) that byblos reaches: reading a PDF (cross-reference tables and streams, object
//! streams, repair, empty-password decryption), the page tree, stream filters, image extraction,
//! relaxed validation, optimization, and the write fallback. docs/scope.md lists what is in and
//! what is out; docs/design.md is the plan.
//!
//! The entry points are in [`api`]; docs/limits.md, docs/threading.md, docs/safety.md and
//! docs/go-differences.md have the details.
//!
//! # Quick start
//!
//! ```
//! use pdf::{Config, Limits};
//! use std::sync::atomic::AtomicBool;
//!
//! # fn minimal_pdf() -> Vec<u8> {
//! #     let objects = [
//! #         "<< /Type /Pages /Kids [2 0 R] /Count 1 >>",
//! #         "<< /Type /Page /Parent 1 0 R /MediaBox [0 0 100 100] /Resources <<>> >>",
//! #         "<< /Type /Catalog /Pages 1 0 R >>",
//! #     ];
//! #     let mut f = b"%PDF-1.7\n".to_vec();
//! #     let mut offsets = Vec::new();
//! #     for (i, o) in objects.iter().enumerate() {
//! #         offsets.push(f.len());
//! #         f.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
//! #     }
//! #     let xref = f.len();
//! #     let n = objects.len() + 1;
//! #     f.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
//! #     for o in offsets {
//! #         f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
//! #     }
//! #     let tail = format!("trailer\n<< /Size {n} /Root 3 0 R >>\nstartxref\n{xref}\n%%EOF\n");
//! #     f.extend_from_slice(tail.as_bytes());
//! #     f
//! # }
//! # fn main() -> Result<(), pdf::Error> {
//! # let data = minimal_pdf();
//! // At most 2 threads and 256 MiB for any one call.
//! let cfg = Config {
//!     limits: Limits {
//!         max_threads: 2,
//!         max_alloc: 256 << 20,
//!         ..Limits::default()
//!     },
//!     ..Config::default()
//! };
//! let cancel = AtomicBool::new(false);
//! let mut ctx = pdf::api::read_and_validate(&data, &cfg, &cancel)?;
//! ctx.xref.ensure_page_count()?;
//! assert_eq!(ctx.xref.page_count, 1);
//! # Ok(())
//! # }
//! ```
//!
//! # Resource limits
//!
//! Every entry point takes [`Limits`], directly or through [`Config`] or the document that
//! holds them, and every long-running call takes a cancel flag. Set them first:
//!
//! ```
//! use pdf::{Error, Limits, Mem};
//!
//! # fn main() -> Result<(), Error> {
//! // At most one thread (no thread is spawned, no pool is touched) and 64 MiB for the whole
//! // document.
//! let limits = Limits {
//!     max_threads: 1,
//!     max_alloc: 64 << 20,
//!     ..Limits::default()
//! };
//!
//! // Every buffer that scales with the input is charged to the document's ledger before it is
//! // allocated, and released when its last owner drops.
//! let mem = Mem::new(&limits);
//! let buf = mem.bytes_from(b"%PDF-1.7", 8)?;
//! assert_eq!(mem.used(), 8);
//! assert_eq!(&buf[..], b"%PDF-1.7");
//!
//! // Over `max_alloc` is an error, not an allocation.
//! assert!(matches!(mem.charge(64 << 20), Err(Error::Alloc { .. })));
//! drop(buf);
//! assert_eq!(mem.used(), 0);
//! # Ok(())
//! # }
//! ```
//!
//! The fields of [`Limits`] mirror pdf-go's `ResourceLimits`, with Go's zero rules (0 = the
//! default, or 0 = off, per field; see each field), and two fields every port shares:
//!
//! * `max_alloc` (default 4 GiB, bytes): the enforced peak of one document plus the call in
//!   progress, all threads together, nested crates included. Over it: [`Error::Alloc`]. It is a
//!   backstop above what Go's own limits allow at Go's defaults.
//! * `max_threads` (default [`std::thread::available_parallelism`]): the most threads of one
//!   call, the calling thread included. 1 spawns no thread and touches no pool; 0 counts as 1.
//!   The output is the same for every value. Every call is serial today: `max_threads` is
//!   honoured because no thread is spawned, and no entry point takes a `ThreadPool`.
//! * Shared ledger: [`api::read_with_mem`], [`api::read_and_validate_with_mem`],
//!   [`api::read_validate_and_optimize_with_mem`] and
//!   [`api::create_xref_table_with_root_dict_with_mem`] charge the caller's [`Mem`], so several
//!   documents of one call share one peak, that `Mem`'s maximum; each document's `max_alloc` is
//!   then not used.
//! * The limit errors are [`Error::Limit`] (with a [`LimitKind`]), [`Error::DecodeLimit`],
//!   [`Error::XRefObjectRange`] and [`Error::ImageLimit`]; [`Error::is_limit_exceeded`] and
//!   [`Error::is_decode_limit_exceeded`] mirror Go's `errors.Is`.
//! * Cancellation: a call that can run long takes `cancel: &AtomicBool` and returns
//!   [`Error::Cancelled`] at its next check.
//!
//! The other fields cap one document, in the units of the table below (0 is "off" or "the
//! default" as the field says; docs/limits.md has the Go rule and the error of each):
//!
//! | Field | Default | Unit |
//! |---|---|---|
//! | `max_input_bytes` | 0 (off) | bytes of the input |
//! | `max_object_bytes` | 64 MiB | bytes of one object, stream payload excluded |
//! | `max_stream_bytes` | 512 MiB | encoded bytes of a stream read without a length |
//! | `max_decode_bytes` | 512 MiB | decoded bytes of one stream |
//! | `max_total_decode_bytes` | 0 (1 GiB) | Go slice capacity decoded over the document |
//! | `max_image_pixels` | 100 000 000 | pixels of one image |
//! | `max_image_bytes` | 512 MiB | bytes of one image buffer |
//! | `max_object_count` | 10 000 000 | cross-reference object numbers |
//! | `max_object_stream_count` | 1 000 000 | `/N` of one object stream |
//! | `max_object_stream_total_count` | 0 (off) | entries of all object streams |
//! | `max_parsed_bytes` | 0 (off) | estimated heap of the parsed objects |
//! | `max_stream_total_bytes` | 0 (off) | encoded stream bytes read |
//! | `max_object_stream_first` | 16 MiB | bytes of one object stream prolog |
//! | `max_xref_entries` | 10 000 000 | entries of one cross-reference section |
//! | `max_recursion_depth` | 100 | levels of parsing and traversal |
//! | `max_alloc` | 4 GiB | peak bytes held by the call |
//! | `max_threads` | `available_parallelism` | threads, the caller included |
//!
//! A Rust port of dobbo-ca/pdf-go at 113d693.
#![forbid(unsafe_code)]
// The ported code keeps Go's index loops and long argument lists.
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::chunks_exact_to_as_chunks,
    clippy::type_complexity
)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]

// Later work packages add their `pub mod` / `mod` lines here (docs/design.md section 2).
pub mod annotation;
pub mod api;
pub mod config;
pub mod crypto;
pub mod doc;
pub mod env;
pub mod error;
pub mod filter;
#[doc(hidden)]
pub mod fuzz_api;
/// The Go capacity model. Public for the `gocap` fuzz target; not part of the API.
#[doc(hidden)]
pub mod gocap;
pub mod image;
pub mod ledger;
pub mod model;
pub mod object;
pub mod optimize;
/// The worker pool of a later phase; nothing uses it yet and no entry point takes one.
#[doc(hidden)]
pub mod par;
pub mod parse;
pub mod read;
pub mod validate;
pub mod write;

/// Compiles the README's quick start as a doc-test.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

pub use config::{Cmd, Config, Limits, decode_allowance};
pub use env::{GoTime, WriteEnv};
pub use error::{Error, LimitKind, Sentinel};
pub use ledger::{Bytes, Held, Ledger, Mem};
