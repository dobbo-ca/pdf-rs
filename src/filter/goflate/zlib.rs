//! Go's compress/zlib writer at levels 1, 6 and 9: the zlib header, the compressor and the
//! Adler-32 trailer.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::Sink;
use super::deflate::Compressor;
use super::fast::{FAST_BYTES, MAX_STORE_BLOCK_SIZE};
use super::lazy::LAZY_BYTES;
use super::level1::L1_BYTES;
use super::token::Tokens;
use crate::error::Error;
use crate::ledger::{Held, Mem};
use std::sync::atomic::AtomicBool;

/// The bytes one [`ZlibWriter`] at `level` (1, 6 or 9) holds: the window and the match finder
/// (history and tables, or hash chains), the tokens, and a few KiB for the Huffman tables.
fn writer_bytes(level: i32) -> u64 {
    let enc = match level {
        1 => L1_BYTES + MAX_STORE_BLOCK_SIZE,
        6 => FAST_BYTES + MAX_STORE_BLOCK_SIZE,
        _ => LAZY_BYTES,
    };
    (enc + Tokens::CAP * 4 + 16 * 1024) as u64
}

/// Adler-32's modulus.
const ADLER_MOD: u32 = 65521;
/// The most bytes that can be summed before the modulus (zlib's `NMAX`).
const ADLER_NMAX: usize = 5552;

/// `adler32.Update`.
pub(super) fn adler32_update(adler: u32, p: &[u8]) -> u32 {
    let mut s1 = adler & 0xffff;
    let mut s2 = adler >> 16;
    for chunk in p.chunks(ADLER_NMAX) {
        for &x in chunk {
            s1 += u32::from(x);
            s2 += s1;
        }
        s1 %= ADLER_MOD;
        s2 %= ADLER_MOD;
    }
    (s2 << 16) | s1
}

/// Go's `zlib.Writer` at `DefaultCompression` (level 6), `BestSpeed` (1) or `BestCompression`
/// (9), without a preset dictionary.
///
/// Calls of [`write`](Self::write), [`flush`](Self::flush) and [`close`](Self::close) pass the
/// compressed bytes to the sink exactly as Go passes them to its `io.Writer`: the same bytes in
/// the same calls (`image/png` chunks its IDAT data by them). The first error of the sink, or
/// of a write after [`close`](Self::close), is kept and returned by every later call, as in Go.
///
/// A writer holds about 1 MiB, charged to the [`Mem`] it is made with and released when it
/// drops. [`reset`](Self::reset) makes it as new for another stream, as Go's pooled
/// `zlib.Writer.Reset`.
pub struct ZlibWriter {
    comp: Compressor,
    /// 1, 6 or 9.
    level: i32,
    /// Whether the compressor has been used: Go makes it with the header. A `reset` of a writer
    /// that has not been used leaves it as it is.
    started: bool,
    adler: u32,
    err: Option<Error>,
    wrote_header: bool,
    _held: Held,
}

impl ZlibWriter {
    /// `zlib.NewWriter`: [`with_level`](Self::with_level) at level 6.
    ///
    /// # Errors
    /// [`Error::Alloc`] if `mem` refuses the memory, [`Error::OutOfMemory`] if the allocator
    /// does.
    pub fn new(mem: &Mem) -> Result<ZlibWriter, Error> {
        Self::with_level(mem, 6)
    }

    /// `zlib.NewWriterLevel` at `level` 1 (`BestSpeed`), 6 (`DefaultCompression`) or 9
    /// (`BestCompression`). The level also sets the FLEVEL bits of the header.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with Go's "zlib: invalid compression level: N" for any other level
    /// (Go also takes -2, -1, 0 and 2-8; this port has only these three), [`Error::Alloc`] if
    /// `mem` refuses the memory, [`Error::OutOfMemory`] if the allocator does.
    pub fn with_level(mem: &Mem, level: i32) -> Result<ZlibWriter, Error> {
        if !matches!(level, 1 | 6 | 9) {
            return Err(Error::Corrupt(format!(
                "zlib: invalid compression level: {level}"
            )));
        }
        let held = mem.charge(writer_bytes(level))?;
        Ok(ZlibWriter {
            comp: Compressor::new(level)?,
            level,
            started: false,
            adler: 1,
            err: None,
            wrote_header: false,
            _held: held,
        })
    }

    /// `Reset`: makes the writer equivalent to a new one. The header is written again with the
    /// next call.
    pub fn reset(&mut self) {
        if self.started {
            self.comp.reset();
        }
        self.adler = 1;
        self.err = None;
        self.wrote_header = false;
    }

    /// `writeHeader`: writes the zlib header.
    fn write_header(&mut self, sink: &mut Sink<'_>) -> Result<(), Error> {
        self.wrote_header = true;
        // Two bytes (RFC 1950). The first is 7 << 4 (the default window size) | 8 (deflate).
        // In the second, the two bits of FLEVEL are 0 for level 1, 2 for the default level and
        // 3 for level 9, FDICT is unset and the five bits of FCHECK make the pair a multiple
        // of 31.
        let b0: u8 = 0x78;
        let mut b1: u8 = match self.level {
            1 => 0,
            6 => 2 << 6,
            _ => 3 << 6,
        };
        let v = u16::from_be_bytes([b0, b1]);
        b1 = b1.wrapping_add((31 - v % 31) as u8);
        sink(&[b0, b1])?;
        // Go makes the compressor here, unless the writer is being reused after a reset.
        self.started = true;
        Ok(())
    }

    /// `Write`: compresses `p`. `cancel` is checked once per 64 KiB window.
    ///
    /// # Errors
    /// The error of the sink or an earlier error of this writer, [`Error::Cancelled`] if
    /// `cancel` is set, and an error for a write after [`close`](Self::close).
    pub fn write(
        &mut self,
        p: &[u8],
        sink: &mut Sink<'_>,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if !self.wrote_header {
            self.err = self.write_header(sink).err();
        }
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        if p.is_empty() {
            return Ok(());
        }
        if let Err(e) = self.comp.write(p, sink, cancel) {
            self.err = Some(e.clone());
            return Err(e);
        }
        self.adler = adler32_update(self.adler, p);
        Ok(())
    }

    /// `Flush`: flushes the compressed data to the sink (Go's Z_SYNC_FLUSH).
    ///
    /// # Errors
    /// The error of the sink or an earlier error of this writer.
    pub fn flush(&mut self, sink: &mut Sink<'_>) -> Result<(), Error> {
        if !self.wrote_header {
            self.err = self.write_header(sink).err();
        }
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.err = self.comp.sync_flush(sink).err();
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// `Close`: flushes the remaining data and writes the end of the stream and the Adler-32
    /// trailer. Like Go, a second call writes the trailer again.
    ///
    /// # Errors
    /// The error of the sink or an earlier error of this writer.
    pub fn close(&mut self, sink: &mut Sink<'_>) -> Result<(), Error> {
        if !self.wrote_header {
            self.err = self.write_header(sink).err();
        }
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        self.err = self.comp.close(sink).err();
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        // zlib (RFC 1950) is big-endian.
        self.err = sink(&self.adler.to_be_bytes()).err();
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}
