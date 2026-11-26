//! Go's zlib encoder at levels 1, 6 and 9, bit for bit: a port of Go 1.27's `compress/zlib`
//! writer and the part of `compress/flate` that `zlib.NewWriterLevel` (no dictionary) reaches
//! at those levels: `deflateFast` with the level 1 and level 6 match finders, `deflateLazy` for
//! level 9, the Huffman bit writer, and the Huffman code and token tables. Level 6
//! (`zlib.NewWriter`) gives the bytes of `image/png` at its default level and of pdf-go's
//! `StreamDict.Encode`; levels 1 (`BestSpeed`) and 9 (`BestCompression`) those of
//! `zlib.NewWriterLevel` and `png.Encoder` at those levels.
//!
//! Output goes to a sink, a `FnMut(&[u8]) -> Result<(), Error>` that gets one call for every
//! call Go makes to its `io.Writer`, with the same bytes. The split matters: `image/png` cuts
//! its IDAT chunks by those calls (image/png/writer.go:547-556).
//!
//! ```
//! use pdf::filter::goflate::ZlibWriter;
//! use pdf::{Error, Limits, Mem};
//! use std::sync::atomic::AtomicBool;
//!
//! # fn main() -> Result<(), Error> {
//! let mem = Mem::new(&Limits::default());
//! let cancel = AtomicBool::new(false);
//! let mut calls: Vec<Vec<u8>> = Vec::new();
//! let mut w = ZlibWriter::new(&mem)?;
//! w.write(
//!     b"hello",
//!     &mut |b: &[u8]| {
//!         calls.push(b.to_vec());
//!         Ok(())
//!     },
//!     &cancel,
//! )?;
//! w.close(&mut |b: &[u8]| {
//!     calls.push(b.to_vec());
//!     Ok(())
//! })?;
//! // The zlib header, a stored block (5 bytes are under 33), the end of the stream, the
//! // Adler-32 trailer: six calls of Go's writer.
//! assert_eq!(calls.len(), 6);
//! assert_eq!(calls[0], [0x78, 0x9c]);
//! assert_eq!(calls[5], [0x06, 0x2c, 0x02, 0x15]);
//! # Ok(())
//! # }
//! ```
//!
//! Go's output is not covered by Go's compatibility promise, so this is a port of 1.27.4; the
//! parity job compares it with the Go oracle.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

mod bit_writer;
mod deflate;
mod fast;
mod huffman_code;
mod lazy;
mod level1;
mod tables;
mod token;
mod zlib;

#[cfg(test)]
mod testdata;
#[cfg(test)]
mod tests;

pub use zlib::ZlibWriter;

use crate::error::Error;

/// The sink of a [`ZlibWriter`]: it is called with the compressed bytes, and an error stops the
/// writer, which returns it from then on.
pub type Sink<'a> = dyn FnMut(&[u8]) -> Result<(), Error> + 'a;
