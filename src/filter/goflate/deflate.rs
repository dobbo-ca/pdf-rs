//! The compressor: Go's compress/flate deflate.go, `deflateFast` (levels 1 and 6), the
//! writer around it, and the dispatch to `deflateLazy` (level 9, in lazy.rs).
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::Sink;
use super::bit_writer::HuffmanBitWriter;
use super::fast::{FastEncL6, MAX_STORE_BLOCK_SIZE};
use super::lazy::{Lazy, WINDOW_SIZE};
use super::level1::FastEncL1;
use super::token::Tokens;
use crate::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};

/// The log2 penalty reduction for new tables on levels 1-6 (`d.w.logNewTablePenalty = 7`).
const LOG_NEW_TABLE_PENALTY: u32 = 7;

/// Go's `errWriterClosed`.
fn writer_closed() -> Error {
    Error::Corrupt("flate: closed writer".to_string())
}

/// Go's `fastEnc` for the levels this port has.
enum FastEnc {
    L1(FastEncL1),
    L6(FastEncL6),
}

impl FastEnc {
    fn encode(&mut self, dst: &mut Tokens, src: &[u8]) {
        match self {
            FastEnc::L1(e) => e.encode(dst, src),
            FastEnc::L6(e) => e.encode(dst, src),
        }
    }

    fn reset(&mut self) {
        match self {
            FastEnc::L1(e) => e.g.reset(),
            FastEnc::L6(e) => e.g.reset(),
        }
    }
}

/// Go's `compressor` at level 1, 6 or 9.
pub(super) struct Compressor {
    /// Writer for blocks.
    pub(super) w: HuffmanBitWriter,

    /// The window: `MAX_STORE_BLOCK_SIZE` bytes on levels 1 and 6, twice `WINDOW_SIZE` on 9.
    pub(super) window: Vec<u8>,
    /// Filled bytes in the window.
    pub(super) window_end: i32,
    /// Tokens of the block being built.
    pub(super) tokens: Tokens,
    /// The encoder to use for blocks on levels 1 and 6.
    fast: Option<FastEnc>,
    /// The state of level 9 (Go's `state`, with `blockStart`, `byteAvailable` and `h`).
    pub(super) lazy: Option<Box<Lazy>>,

    /// Stateful error.
    pub(super) err: Option<Error>,
    /// Whether `err` is `errWriterClosed`.
    closed: bool,
    /// Requesting a flush.
    pub(super) sync: bool,
}

impl Compressor {
    /// `init` at `level`, which is 1, 6 or 9. The caller has charged the memory.
    pub(super) fn new(level: i32) -> Result<Compressor, Error> {
        let (fast, lazy, window_len, penalty) = match level {
            1 => (
                Some(FastEnc::L1(FastEncL1::new()?)),
                None,
                MAX_STORE_BLOCK_SIZE,
                LOG_NEW_TABLE_PENALTY,
            ),
            6 => (
                Some(FastEnc::L6(FastEncL6::new()?)),
                None,
                MAX_STORE_BLOCK_SIZE,
                LOG_NEW_TABLE_PENALTY,
            ),
            // Levels 7-9: `d.w.logNewTablePenalty = 8`.
            _ => (None, Some(Lazy::new()?), 2 * WINDOW_SIZE as usize, 8),
        };
        let mut window = Vec::new();
        window.try_reserve_exact(window_len)?;
        window.resize(window_len, 0);
        Ok(Compressor {
            w: HuffmanBitWriter::new(penalty),
            window,
            window_end: 0,
            tokens: Tokens::new()?,
            fast,
            lazy,
            err: None,
            closed: false,
            sync: false,
        })
    }

    /// `d.step`: `deflateFast` or `deflateLazy`.
    fn step(&mut self, sink: &mut Sink<'_>) {
        if self.lazy.is_some() {
            self.deflate_lazy(sink);
        } else {
            self.deflate_fast(sink);
        }
    }

    /// `writeStoredBlock`: writes an uncompressed block to the stream.
    fn write_stored_block(&mut self, sink: &mut Sink<'_>) -> Option<Error> {
        let n = self.window_end as usize;
        self.w.write_stored_header(n, false, sink);
        if self.w.out.err.is_some() {
            return self.w.out.err.clone();
        }
        self.w.out.write_bytes(&self.window[..n], sink);
        self.w.out.err.clone()
    }

    /// `deflateFast`: encodes the current window if it has filled or if we are doing
    /// sync/flush.
    fn deflate_fast(&mut self, sink: &mut Sink<'_>) {
        // We only compress if we have MAX_STORE_BLOCK_SIZE.
        if (self.window_end as usize) < self.window.len() {
            if !self.sync {
                return;
            }
            // Handle extremely small sizes.
            if self.window_end < 128 {
                if self.window_end == 0 {
                    return;
                }
                if self.window_end <= 32 {
                    self.err = self.write_stored_block(sink);
                } else {
                    let n = self.window_end as usize;
                    self.w
                        .write_block_huff(false, &self.window[..n], true, sink);
                    self.err = self.w.out.err.clone();
                }
                self.tokens.reset();
                self.window_end = 0;
                if let Some(f) = &mut self.fast {
                    f.reset();
                }
                return;
            }
        }

        let n = self.window_end as usize;
        if let Some(f) = &mut self.fast {
            f.encode(&mut self.tokens, &self.window[..n]);
        }
        // If we made zero matches, store the block as is.
        if self.tokens.n == 0 {
            self.err = self.write_stored_block(sink);
        // If we removed less than 1/16th, huffman compress the block.
        } else if i32::from(self.tokens.n) > self.window_end - (self.window_end >> 4) {
            self.w
                .write_block_huff(false, &self.window[..n], self.sync, sink);
            self.err = self.w.out.err.clone();
        } else {
            self.w.write_block_dynamic(
                &mut self.tokens,
                false,
                Some(&self.window[..n]),
                self.sync,
                sink,
            );
            self.err = self.w.out.err.clone();
        }
        self.tokens.reset();
        self.window_end = 0;
    }

    /// `fillBlock`: appends `b` to the window, returning the number of bytes copied. If it is
    /// less than `b.len()`, the window is filled.
    pub(super) fn fill_block(&mut self, b: &[u8]) -> usize {
        let start = self.window_end as usize;
        let n = b.len().min(self.window.len() - start);
        self.window[start..start + n].copy_from_slice(&b[..n]);
        self.window_end += n as i32;
        n
    }

    /// `write`: adds `b` to the compressor. The cancel flag is checked once per window.
    pub(super) fn write(
        &mut self,
        b: &[u8],
        sink: &mut Sink<'_>,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        let mut b = b;
        while !b.is_empty() {
            if self.window_end as usize == self.window.len() || self.sync {
                if cancel.load(Ordering::Relaxed) {
                    let e = Error::Cancelled;
                    self.err = Some(e.clone());
                    return Err(e);
                }
                self.step(sink);
            }
            let n = if self.lazy.is_some() {
                self.fill_deflate(b)
            } else {
                self.fill_block(b)
            };
            b = &b[n..];
            if let Some(e) = &self.err {
                return Err(e.clone());
            }
        }
        Ok(())
    }

    /// `syncFlush`: flushes the compressor by writing any remaining window and writing a
    /// stored block to byte-align the output.
    pub(super) fn sync_flush(&mut self, sink: &mut Sink<'_>) -> Result<(), Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.sync = true;
        self.step(sink);
        if self.err.is_none() {
            self.w.write_stored_header(0, false, sink);
            self.w.flush(sink);
            self.err = self.w.out.err.clone();
        }
        self.sync = false;
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// `reset` followed by the `fillWindow(nil)` of `Writer.Reset`: the compressor is as new,
    /// writing to a new sink. Go's `fillWindow` of an empty dictionary encodes nothing but
    /// still runs the offset wraparound protection of the encoder.
    pub(super) fn reset(&mut self) {
        self.w.reset();
        self.sync = false;
        self.err = None;
        self.closed = false;
        self.window_end = 0;
        // We only need to reset a few things for fast encoders.
        if let Some(f) = &mut self.fast {
            f.reset();
            self.tokens.reset();
            f.encode(&mut self.tokens, &[]);
            self.tokens.reset();
        }
        if let Some(s) = &mut self.lazy {
            // `fillWindow(nil)` then changes nothing.
            s.reset();
            self.tokens.reset();
        }
    }

    /// `close`: flushes any uncompressed data and writes an end-of-stream block.
    pub(super) fn close(&mut self, sink: &mut Sink<'_>) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.sync = true;
        self.step(sink);
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.w.write_stored_header(0, true, sink);
        if let Some(e) = &self.w.out.err {
            return Err(e.clone());
        }
        self.w.flush(sink);
        if let Some(e) = &self.w.out.err {
            return Err(e.clone());
        }
        self.err = Some(writer_closed());
        self.closed = true;
        self.w.reset();
        Ok(())
    }
}
