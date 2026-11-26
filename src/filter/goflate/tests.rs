//! Tests of the Go zlib encoder: Go's own vectors (the Huffman bit writer against
//! `testdata/huffman-*`), the properties of Go's tests that hold at the default level (reset,
//! chunking, wraparound, persistent errors, close), hand-derived call sequences, and round
//! trips through deflate-rs. The golden bytes of whole streams come from the oracle's `-gen`
//! (docs/design.md section 12), not from here.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::bit_writer::HuffmanBitWriter;
use super::deflate::Compressor;
use super::fast::{BUFFER_RESET, FastEncL6, MAX_MATCH_OFFSET};
use super::huffman_code::{HuffmanEncoder, reverse_bits};
use super::lazy::{MAX_HASH_OFFSET, WINDOW_SIZE};
use super::level1::FastEncL1;
use super::testdata::{BLOCK_CASES, HUFF_CASES};
use super::token::Tokens;
use super::{Sink, ZlibWriter};
use crate::error::Error;
use crate::ledger::Mem;
use std::io::Write;
use std::sync::atomic::AtomicBool;

/// Runs `f` with a sink that collects the bytes.
fn collect(f: impl FnOnce(&mut Sink<'_>)) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        f(&mut sink);
    }
    out
}

fn new_writer() -> ZlibWriter {
    ZlibWriter::new(&Mem::with_max(1 << 30)).unwrap()
}

fn level_writer(level: i32) -> ZlibWriter {
    ZlibWriter::with_level(&Mem::with_max(1 << 30), level).unwrap()
}

/// Compresses `data` with `w` in writes of `chunk` bytes, closing it, and returns the bytes.
fn compress(w: &mut ZlibWriter, data: &[u8], chunk: usize) -> Vec<u8> {
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    {
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        // Go's Write of nothing still writes the header.
        w.write(&[], &mut sink, &cancel).unwrap();
        for c in data.chunks(chunk) {
            w.write(c, &mut sink, &cancel).unwrap();
        }
        w.close(&mut sink).unwrap();
    }
    out
}

fn inflate(z: &[u8]) -> Vec<u8> {
    deflate::zlib::decode(z, &deflate::Limits::default(), &AtomicBool::new(false)).unwrap()
}

/// A raw flate stream of `writes` from `c`, closed.
fn flate_stream(c: &mut Compressor, writes: &[&[u8]]) -> Vec<u8> {
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    {
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        for w in writes {
            c.write(w, &mut sink, &cancel).unwrap();
        }
        c.close(&mut sink).unwrap();
    }
    out
}

struct Rng(u64);

impl Rng {
    fn step(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Compressible text made of a few words.
fn text(n: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "alpha ", "beta ", "gamma ", "delta ", "pdf ", "stream ", "object ", "flate ", "zlib ",
        "endobj\n", "xref ", "0 0 R ", "/Type ", "/Page ", "<<", ">>",
    ];
    let mut r = Rng(seed);
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(WORDS[(r.step() % 16) as usize].as_bytes());
    }
    v.truncate(n);
    v
}

/// Bytes that do not compress.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..n).map(|_| (r.step() >> 24) as u8).collect()
}

/// The input of Go's `TestWriter_Reset`.
fn lines(n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    for i in 0..n {
        writeln!(buf, "asdasfasf{i}{i}fghfgujyut{i}yutyu").unwrap();
    }
    buf
}

/// Go's `TestBlockHuff`: `writeBlockHuff` against the reference files, then again after
/// `reset`, then a block marked as the last one.
#[test]
fn block_huff_matches_go() {
    for (name, input, golden) in HUFF_CASES {
        let mut bw = HuffmanBitWriter::new(8);
        let got = collect(|sink| {
            bw.write_block_huff(false, input, false, sink);
            bw.flush(sink);
        });
        assert!(bw.out.err.is_none(), "{name}");
        assert!(
            got == *golden,
            "{name}: output differs from the golden file"
        );

        // The writer produces the same output after reset.
        bw.reset();
        let got = collect(|sink| {
            bw.write_block_huff(false, input, false, sink);
            bw.flush(sink);
        });
        assert!(got == *golden, "{name}: output differs after reset");

        // testWriterEOF: the block is marked as the last one.
        let mut bw = HuffmanBitWriter::new(0);
        let got = collect(|sink| {
            bw.write_block_huff(true, input, true, sink);
            bw.flush(sink);
        });
        assert!(bw.out.err.is_none(), "{name}");
        assert!(!got.is_empty() && got[0] & 1 == 1, "{name}: no EOF mark");
    }
}

/// One `writeBlockDynamic` of `toks` (as `indexTokens`) on `bw`, flushed.
fn run_dynamic(
    bw: &mut HuffmanBitWriter,
    toks: &[u32],
    input: Option<&[u8]>,
    eof: bool,
    sync: bool,
) -> Vec<u8> {
    // Go passes its tokens by value, so every call starts from a fresh copy.
    let mut tokens = Tokens::new().unwrap();
    tokens.index_tokens(toks);
    let out = collect(|sink| {
        bw.write_block_dynamic(&mut tokens, eof, input, sync, sink);
        bw.flush(sink);
    });
    assert!(bw.out.err.is_none());
    out
}

fn check_dynamic(sync: bool) {
    for case in BLOCK_CASES {
        let name = case.name;
        let (want, want_noinput) = if sync {
            (case.sync_want, case.sync_noinput)
        } else {
            (case.dyn_want, case.dyn_noinput)
        };
        if let (Some(input), Some(want)) = (case.input, want) {
            let mut bw = HuffmanBitWriter::new(0);
            let got = run_dynamic(&mut bw, case.tokens, Some(input), false, sync);
            assert!(got == want, "{name}: output differs with input");
            // The writer produces the same output after reset.
            bw.reset();
            let got = run_dynamic(&mut bw, case.tokens, Some(input), false, sync);
            assert!(
                got == want,
                "{name}: output differs after reset, with input"
            );
            // testWriterEOF.
            let mut bw = HuffmanBitWriter::new(0);
            let got = run_dynamic(&mut bw, case.tokens, Some(input), true, true);
            assert!(!got.is_empty() && got[0] & 1 == 1, "{name}: no EOF mark");
        }

        let mut bw = HuffmanBitWriter::new(0);
        let got = run_dynamic(&mut bw, case.tokens, None, false, sync);
        assert!(got == want_noinput, "{name}: output differs without input");
        assert!(got[0] & 1 == 0, "{name}: unexpected EOF mark");
        bw.reset();
        let got = run_dynamic(&mut bw, case.tokens, None, false, sync);
        assert!(
            got == want_noinput,
            "{name}: output differs after reset, without input"
        );
        let mut bw = HuffmanBitWriter::new(0);
        let got = run_dynamic(&mut bw, case.tokens, None, true, true);
        assert!(!got.is_empty() && got[0] & 1 == 1, "{name}: no EOF mark");
    }
}

/// Go's `TestWriteBlockDynamic`.
#[test]
fn write_block_dynamic_matches_go() {
    check_dynamic(false);
}

/// Go's `TestWriteBlockDynamicSync`.
#[test]
fn write_block_dynamic_sync_matches_go() {
    check_dynamic(true);
}

/// Go's `reverseBitsTests`.
#[test]
fn reverse_bits_matches_go() {
    for (x, bits, want) in [
        (1u16, 1u8, 1u16),
        (1, 2, 2),
        (1, 3, 4),
        (1, 4, 8),
        (1, 5, 16),
        (17, 5, 17),
        (257, 9, 257),
        (29, 5, 23),
    ] {
        assert_eq!(reverse_bits(x, bits), want, "reverse_bits({x}, {bits})");
    }
}

/// Codes for the frequencies 1, 1, 2, worked by hand from RFC 1951 3.2.2: the literal with
/// frequency 2 gets one bit, the other two get two.
#[test]
fn huffman_codes_by_hand() {
    let mut h = HuffmanEncoder::new(3);
    h.generate(&[1, 1, 2], 15);
    // (code << 8) | length, with the code bit-reversed as flate writes it.
    assert_eq!(&h.codes[..3], &[0x102, 0x302, 0x001]);

    // With two or fewer literals everything has length 1.
    let mut h = HuffmanEncoder::new(4);
    h.generate(&[0, 5, 0, 7], 15);
    assert_eq!(&h.codes[..4], &[0, 0x001, 0, 0x101]);
}

/// A writer that is closed at once: the header, the empty final block, the checksum of
/// nothing.
#[test]
fn empty_stream() {
    let mut w = new_writer();
    let out = compress(&mut w, &[], 1);
    assert_eq!(out, [0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
    assert_eq!(inflate(&out), b"");
}

/// The calls Go makes to its `io.Writer` for `Write("hello")` and `Close`, worked by hand: the
/// header; the stored block of a window of at most 32 bytes (its 3 flag bits flushed alone, then
/// the length and its complement, then the data); the final fixed block of 10 bits; the
/// big-endian Adler-32.
#[test]
fn sink_calls_for_a_short_stream() {
    let mut w = new_writer();
    let cancel = AtomicBool::new(false);
    let mut calls: Vec<Vec<u8>> = Vec::new();
    {
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            calls.push(b.to_vec());
            Ok(())
        };
        w.write(b"hello", &mut sink, &cancel).unwrap();
        w.close(&mut sink).unwrap();
    }
    let want: Vec<Vec<u8>> = vec![
        vec![0x78, 0x9c],
        vec![0x00],
        vec![0x05, 0x00, 0xfa, 0xff],
        b"hello".to_vec(),
        vec![0x03, 0x00],
        vec![0x06, 0x2c, 0x02, 0x15],
    ];
    assert_eq!(calls, want);
}

#[test]
fn adler32_values() {
    use super::zlib::adler32_update;
    assert_eq!(adler32_update(1, b""), 1);
    assert_eq!(adler32_update(1, b"hello"), 0x062c_0215);
    assert_eq!(adler32_update(1, b"Wikipedia"), 0x11e6_0398);
    // Against the definition, with the modulus after every byte, across several blocks.
    let data = noise(30_000, 7);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    assert_eq!(adler32_update(1, &data), (b << 16) | a);
    // In pieces.
    let mut s = 1;
    for c in data.chunks(1237) {
        s = adler32_update(s, c);
    }
    assert_eq!(s, (b << 16) | a);
}

/// The inputs of Go's `deflateInflateTests` and sizes around the thresholds of the encoder
/// (32 and 128 bytes, one window of 65,535), compressed at levels 1, 6 and 9 in one write and
/// in pieces, and decoded again.
#[test]
fn round_trips() {
    let mut inputs: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x11],
        vec![0x11, 0x12],
        vec![0x11; 8],
        vec![
            0x11, 0x10, 0x13, 0x41, 0x21, 0x21, 0x41, 0x13, 0x87, 0x78, 0x13,
        ],
        (0..100_000usize).map(|i| ((i * i) & 0xff) as u8).collect(),
    ];
    for n in [
        5, 32, 33, 100, 127, 128, 129, 1000, 65_534, 65_535, 65_536, 65_537, 131_070, 300_000,
    ] {
        inputs.push(text(n, n as u64 + 1));
        inputs.push(noise(n, n as u64 + 2));
        inputs.push(vec![0; n]);
    }
    for level in [1, 6, 9] {
        let mut w = level_writer(level);
        for (i, data) in inputs.iter().enumerate() {
            for chunk in [usize::MAX, 1 << 16, 1000, 7] {
                if chunk == 7 && data.len() > 2000 {
                    continue;
                }
                w.reset();
                let out = compress(&mut w, data, chunk);
                assert!(
                    inflate(&out) == *data,
                    "level {level} input {i} (len {}) chunk {chunk}: round trip differs",
                    data.len()
                );
            }
        }
    }
}

/// Go's `TestDeterministic`: the output does not depend on how the input is cut into writes.
#[test]
fn output_does_not_depend_on_write_sizes() {
    for level in [1, 6, 9] {
        let data = text(65_535 * 6 + 500, 1);
        let mut w = level_writer(level);
        let one = compress(&mut w, &data, data.len());
        for chunk in [787, 81_761, 65_535, 65_536] {
            w.reset();
            assert!(
                compress(&mut w, &data, chunk) == one,
                "level {level} chunk {chunk} changes the output"
            );
        }
        // Low-entropy bytes, as Go's test: byte(rng & 7).
        let mut r = Rng(1);
        let low: Vec<u8> = (0..65_535 * 4 + 500)
            .map(|_| (r.step() & 7) as u8)
            .collect();
        let mut w = level_writer(level);
        let one = compress(&mut w, &low, low.len());
        w.reset();
        assert!(compress(&mut w, &low, 787) == one);
        assert_eq!(inflate(&one), low);
    }
}

/// Go's `TestWriterReset` (flate and zlib): a reused writer writes what a new one writes, and
/// an unused writer that is reset is still new.
#[test]
fn reset_equals_new() {
    for level in [1, 6, 9] {
        let a = text(200_000, 11);
        let b = lines(10_000);
        let c = noise(70_000, 3);
        let want_a = compress(&mut level_writer(level), &a, 4096);
        let want_b = compress(&mut level_writer(level), &b, 4096);
        let want_c = compress(&mut level_writer(level), &c, 4096);

        let mut w = level_writer(level);
        w.reset();
        assert!(compress(&mut w, &a, 4096) == want_a);
        for _ in 0..3 {
            w.reset();
            assert!(compress(&mut w, &b, 4096) == want_b);
            w.reset();
            assert!(compress(&mut w, &c, 4096) == want_c);
            w.reset();
            assert!(compress(&mut w, &a, 70_000) == want_a);
        }

        // A reset in the middle of a stream drops it.
        let cancel = AtomicBool::new(false);
        let mut sink = |_: &[u8]| -> Result<(), Error> { Ok(()) };
        w.reset();
        w.write(&a, &mut sink, &cancel).unwrap();
        w.reset();
        assert!(compress(&mut w, &b, 4096) == want_b);
    }
}

/// Go's `TestWriterClose` for the zlib writer: after `close`, a write and a flush fail and
/// stay failed, and nothing more is written.
#[test]
fn closed_writer() {
    let mut w = new_writer();
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    let mut sink = |b: &[u8]| -> Result<(), Error> {
        out.extend_from_slice(b);
        Ok(())
    };
    w.write(b"Test", &mut sink, &cancel).unwrap();
    w.close(&mut sink).unwrap();
    let after_close = out.len();
    let mut sink = |b: &[u8]| -> Result<(), Error> {
        out.extend_from_slice(b);
        Ok(())
    };
    let werr = w.write(b"Test", &mut sink, &cancel).unwrap_err();
    assert_eq!(w.flush(&mut sink), Err(werr.clone()));
    assert_eq!(w.close(&mut sink), Err(werr));
    assert_eq!(out.len(), after_close);
    assert_eq!(inflate(&out), b"Test");
}

/// Go writes the trailer again if `Close` is called twice.
#[test]
fn second_close_writes_the_trailer_again() {
    let mut w = new_writer();
    let mut calls: Vec<Vec<u8>> = Vec::new();
    let mut sink = |b: &[u8]| -> Result<(), Error> {
        calls.push(b.to_vec());
        Ok(())
    };
    w.close(&mut sink).unwrap();
    w.close(&mut sink).unwrap();
    assert_eq!(calls.last().unwrap(), &[0x00, 0x00, 0x00, 0x01]);
    assert_eq!(calls.len(), 4);
}

/// Go's `Flush`: the data so far can be decoded after a flush, the stream goes on, and the
/// flush ends with an empty stored block.
#[test]
fn flush_then_continue() {
    let a = text(1000, 5);
    let b = text(100_000, 6);
    let mut w = new_writer();
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    {
        let mut sink = |x: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(x);
            Ok(())
        };
        w.write(&a, &mut sink, &cancel).unwrap();
        w.flush(&mut sink).unwrap();
        w.write(&b, &mut sink, &cancel).unwrap();
        w.close(&mut sink).unwrap();
    }
    // The sync marker is an empty stored block: 00 00 ff ff.
    assert!(out.windows(4).any(|w| w == [0x00, 0x00, 0xff, 0xff]));
    let mut want = a.clone();
    want.extend_from_slice(&b);
    assert_eq!(inflate(&out), want);
}

/// An error of the sink is kept, returned by every later call, and the sink is not called
/// again: Go's `TestWriterPersistentWriteError`, `...FlushError` and `...CloseError`.
#[test]
fn sink_errors_are_persistent() {
    let d = text(10_000, 9);
    let errio = Error::Corrupt("failWriter".to_string());
    let cancel = AtomicBool::new(false);
    let mut w = new_writer();
    // Sweep over the call at which the sink fails.
    for fail_at in 0..1000usize {
        w.reset();
        let mut calls = 0usize;
        let mut failed = false;
        let mut sink = |_: &[u8]| -> Result<(), Error> {
            let n = calls;
            calls += 1;
            if n == fail_at {
                failed = true;
                return Err(Error::Corrupt("failWriter".to_string()));
            }
            assert!(n < fail_at, "the sink is called after it failed");
            Ok(())
        };
        let werr = w.write(&d, &mut sink, &cancel);
        let cerr = w.close(&mut sink);
        let ferr = w.flush(&mut sink);
        if let Err(e) = &werr {
            assert_eq!(e, &errio, "write, fail_at {fail_at}");
        }
        if failed {
            assert_eq!(cerr, Err(errio.clone()), "close, fail_at {fail_at}");
            assert_eq!(ferr, Err(errio.clone()), "flush, fail_at {fail_at}");
        } else {
            // The whole stream was written.
            assert!(werr.is_ok() && cerr.is_ok());
            return;
        }
    }
    panic!("the sink never stopped failing");
}

/// A sink that fails at the first call: header errors are kept as well.
#[test]
fn first_call_failure() {
    let errio = Error::Corrupt("failWriter".to_string());
    let cancel = AtomicBool::new(false);
    let mut w = new_writer();
    let mut sink =
        |_: &[u8]| -> Result<(), Error> { Err(Error::Corrupt("failWriter".to_string())) };
    assert_eq!(w.flush(&mut sink), Err(errio.clone()));
    assert_eq!(w.close(&mut sink), Err(errio.clone()));
    assert_eq!(w.write(b"Test", &mut sink, &cancel), Err(errio));
}

/// Go's `TestWriteError`: after a sink failure (at the 1st, 2nd, 4th ... call) a write
/// returns an error, then `reset` makes the writer work again.
#[test]
fn write_error_then_reset() {
    let data = lines(30_000);
    let cancel = AtomicBool::new(false);
    let mut w = new_writer();
    for fail in [1usize, 4, 16, 64, 256] {
        w.reset();
        let mut calls = 0usize;
        let mut sink = |_: &[u8]| -> Result<(), Error> {
            calls += 1;
            if calls > fail {
                Err(Error::Corrupt("failWriter".to_string()))
            } else {
                Ok(())
            }
        };
        let mut err = None;
        for c in data.chunks(128) {
            if let Err(e) = w.write(c, &mut sink, &cancel) {
                err = Some(e);
                break;
            }
        }
        assert!(err.is_some(), "fail {fail}: expected an error");
        assert!(w.write(&[1, 2, 2, 3, 4, 5], &mut sink, &cancel).is_err());
        assert!(w.flush(&mut sink).is_err());
        assert!(w.close(&mut sink).is_err());

        w.reset();
        let mut ok = |_: &[u8]| -> Result<(), Error> { Ok(()) };
        w.write(&[1, 2, 3, 4, 5, 6], &mut ok, &cancel).unwrap();
    }
}

#[test]
fn cancel_stops_a_long_write() {
    let mut w = new_writer();
    let cancel = AtomicBool::new(true);
    let mut sink = |_: &[u8]| -> Result<(), Error> { Ok(()) };
    // Less than a window: nothing is compressed yet.
    w.write(&[1; 1000], &mut sink, &cancel).unwrap();
    let e = w.write(&[0u8; 200_000], &mut sink, &cancel).unwrap_err();
    assert_eq!(e, Error::Cancelled);
    assert_eq!(w.write(b"x", &mut sink, &cancel), Err(Error::Cancelled));
}

#[test]
fn memory_is_charged_and_released() {
    let mem = Mem::with_max(1 << 20);
    assert!(matches!(ZlibWriter::new(&mem), Err(Error::Alloc { .. })));
    assert_eq!(mem.used(), 0);

    let mem = Mem::with_max(4 << 20);
    let w = ZlibWriter::new(&mem).unwrap();
    assert!(mem.used() > 1 << 20 && mem.used() < 2 << 20);
    let second = ZlibWriter::new(&mem).unwrap();
    assert!(mem.used() > 2 << 20);
    drop(w);
    drop(second);
    assert_eq!(mem.used(), 0);
}

#[test]
fn log2_estimate() {
    use super::token::m_fast_log2;
    assert!(m_fast_log2(1.0).abs() < 0.01);
    assert!((m_fast_log2(8.0) - 3.0).abs() < 0.01);
    assert!((m_fast_log2(0.25) + 2.0).abs() < 0.01);
}

/// Go's `TestWriter_Reset`: the match finder's offsets survive many resets and the wraparound
/// of its 32-bit position (Go issue 34121). No error and no panic.
#[test]
fn offsets_survive_wraparound() {
    let data = lines(8192);
    let cancel = AtomicBool::new(false);
    let mut sink = |_: &[u8]| -> Result<(), Error> { Ok(()) };
    for (level, offset) in [(1, 1usize), (1, 16), (1, 256), (6, 1), (6, 16), (6, 256)] {
        let mut c = Compressor::new(level).unwrap();
        for _ in 0..(BUFFER_RESET as usize - data.len() - offset - MAX_MATCH_OFFSET as usize)
            / MAX_MATCH_OFFSET as usize
        {
            c.reset();
        }
        c.reset();
        c.write(&data, &mut sink, &cancel).unwrap();
        for _ in 0..50 {
            // This wraps around.
            c.reset();
        }
        c.reset();
        c.write(&data, &mut sink, &cancel).unwrap();
        for _ in 0..(u32::MAX as usize - BUFFER_RESET as usize) / MAX_MATCH_OFFSET as usize {
            c.reset();
        }
        c.write(&data, &mut sink, &cancel).unwrap();
        c.close(&mut sink).unwrap();
    }
}

/// Go's `TestDeflateFast_Reset` (written for level 1), at levels 1 and 6: the output is the
/// same when the encoder starts just before the wraparound of its position.
#[test]
fn wraparound_output_matches_fresh() {
    let data = lines(8192);
    for level in [1, 6] {
        let want = flate_stream(&mut Compressor::new(level).unwrap(), &[&data, &data, &data]);
        for offset in [1usize, 16, 256] {
            let mut c = Compressor::new(level).unwrap();
            for _ in 0..(BUFFER_RESET as usize - data.len() - offset - MAX_MATCH_OFFSET as usize)
                / MAX_MATCH_OFFSET as usize
            {
                c.reset();
            }
            c.reset();
            let got = flate_stream(&mut c, &[&data, &data, &data]);
            assert!(
                got == want,
                "level {level} offset {offset}: output differs (len {} against {})",
                got.len(),
                want.len()
            );
        }
    }
}

/// Go's `deflateTests` at levels 1 and 9 (cases 7-14; cases 1-3 repeat case 8): raw flate
/// streams.
#[test]
fn deflate_vectors_levels_1_and_9() {
    let cases: [(&[u8], i32, &[u8]); 5] = [
        (&[], 1, &[0x3, 0x0]),
        (&[0x11], 9, &[0x12, 0x4, 0xc, 0x0]),
        (&[0x11, 0x12], 9, &[0x12, 0x14, 0x2, 0xc, 0x0]),
        (&[0x11; 9], 9, &[0x12, 0x84, 0x1, 0xc0, 0x0]),
        (&[], 9, &[0x3, 0x0]),
    ];
    for (i, (input, level, want)) in cases.into_iter().enumerate() {
        let got = flate_stream(&mut Compressor::new(level).unwrap(), &[input]);
        assert_eq!(got, want, "case {i} level {level}");
    }
}

/// `writeHeader`: FLEVEL is 0, 2 and 3 for levels 1, 6 and 9. Any other level is refused, as
/// Go refuses levels outside -2..=9.
#[test]
fn header_per_level() {
    for (level, b1) in [(1, 0x01), (6, 0x9c), (9, 0xda)] {
        let out = compress(&mut level_writer(level), b"", 1);
        assert_eq!(out[..2], [0x78, b1], "level {level}");
        assert!(inflate(&out).is_empty());
    }
    let mem = Mem::with_max(1 << 30);
    for level in [-2, -1, 0, 2, 7, 8, 10] {
        let want = format!("zlib: invalid compression level: {level}");
        assert!(
            matches!(ZlibWriter::with_level(&mem, level), Err(Error::Corrupt(m)) if m == want),
            "level {level}"
        );
    }
    assert_eq!(mem.used(), 0);
    let w = ZlibWriter::with_level(&mem, 9).unwrap();
    assert!(mem.used() > 1 << 19);
    drop(w);
    assert_eq!(mem.used(), 0);
}

/// Go's `TestBestSpeed` (the cases it runs with -short): level 1 with writes around the window
/// size, with and without a flush after each, decodes to the input.
#[test]
fn best_speed_writes_and_flushes() {
    let abc: Vec<u8> = (0..128u8).collect();
    let abcabc = abc.repeat(131_072 / abc.len());
    let cases: [&[usize]; 6] = [
        &[65536, 0],
        &[65536, 1],
        &[65536, 1, 256],
        &[65536, 1, 65536],
        &[65536, 14],
        &[65536, 15],
    ];
    let cancel = AtomicBool::new(false);
    for (i, tc) in cases.into_iter().enumerate() {
        for first_n in [1, 65534, 65535, 65536, 65537, 131_072] {
            for flush in [false, true] {
                let mut w = level_writer(1);
                let mut want = Vec::new();
                let mut out = Vec::new();
                let mut sink = |b: &[u8]| -> Result<(), Error> {
                    out.extend_from_slice(b);
                    Ok(())
                };
                for (k, &n) in tc.iter().enumerate() {
                    let n = if k == 0 { first_n } else { n };
                    want.extend_from_slice(&abcabc[..n]);
                    w.write(&abcabc[..n], &mut sink, &cancel).unwrap();
                    if flush {
                        w.flush(&mut sink).unwrap();
                    }
                }
                w.close(&mut sink).unwrap();
                assert!(
                    inflate(&out) == want,
                    "i={i}, first_n={first_n}, flush={flush}: round trip differs"
                );
            }
        }
    }
}

/// Go's `TestBestSpeedMaxMatchOffset`: two copies of a string about `MAX_MATCH_OFFSET` apart
/// at level 1.
#[test]
fn best_speed_max_match_offset() {
    const ABC: &[u8] = b"abcdefgh";
    const XYZ: &[u8] = b"stuvwxyz";
    const INPUT_MARGIN: usize = 16 - 1;
    for match_before in [false, true] {
        for extra in [
            0,
            INPUT_MARGIN - 1,
            INPUT_MARGIN,
            INPUT_MARGIN + 1,
            2 * INPUT_MARGIN,
        ] {
            for offset_adj in -5i32..=5 {
                let offset = (MAX_MATCH_OFFSET + offset_adj) as usize;
                let mut src = vec![0u8; offset + ABC.len() + extra];
                src[..ABC.len()].copy_from_slice(ABC);
                if !match_before {
                    src[offset - XYZ.len()..offset].copy_from_slice(XYZ);
                }
                src[offset..offset + ABC.len()].copy_from_slice(ABC);
                let out = compress(&mut level_writer(1), &src, src.len());
                assert!(
                    inflate(&out) == src,
                    "match_before={match_before}, extra={extra}, offset_adj={offset_adj}"
                );
            }
        }
    }
}

/// Go's `TestBestSpeedShiftOffsets` for levels 1 and 6: the table shift at `BUFFER_RESET` keeps
/// the matches in range and drops the others.
#[test]
fn shift_offsets_keep_matches() {
    // Random bytes have no matches inside one copy.
    let data = noise(100, 9);
    let val_or_len = |n: u16| if n == 0 { data.len() as u16 } else { n };
    macro_rules! check {
        ($enc:expr) => {{
            let mut enc = $enc;
            let mut first = Tokens::new().unwrap();
            let mut second = Tokens::new().unwrap();
            enc.encode(&mut first, &data);
            enc.encode(&mut second, &data);
            let want_first = val_or_len(first.n);
            let want_second = val_or_len(second.n);
            assert!(
                want_first > want_second,
                "the second copy must match the first"
            );

            // Forward the current indicator to before wraparound.
            enc.g.hist.clear();
            enc.g.cur = BUFFER_RESET - data.len() as i32;
            // Part 1 before the wrap matches the clean state.
            let mut got = Tokens::new().unwrap();
            enc.encode(&mut got, &data);
            assert_eq!(val_or_len(got.n), want_first);
            // We are about to wrap.
            assert_eq!(enc.g.cur + enc.g.hist.len() as i32, BUFFER_RESET);

            // Part 2 matches the clean state as well, though it wraps.
            got.reset();
            enc.encode(&mut got, &data);
            assert_eq!(val_or_len(got.n), want_second);
            assert!(enc.g.cur < BUFFER_RESET);

            // Forward the buffer, leaving the matches at the bottom: none is picked up.
            enc.g.cur = BUFFER_RESET;
            enc.g.hist.clear();
            got.reset();
            enc.encode(&mut got, &data);
            assert_eq!(val_or_len(got.n), want_first);
        }};
    }
    check!(FastEncL1::new().unwrap());
    check!(FastEncL6::new().unwrap());
}

/// Level 9 rebases its hash chains when `hash_offset` passes `MAX_HASH_OFFSET` (Go reaches it
/// after 2^28 bytes); the output does not change.
#[test]
fn level9_hash_offset_rebase() {
    let data = text(300_000, 5);
    let want = flate_stream(&mut Compressor::new(9).unwrap(), &[&data]);
    let mut c = Compressor::new(9).unwrap();
    if let Some(s) = c.lazy.as_mut() {
        s.hash_offset = MAX_HASH_OFFSET - WINDOW_SIZE;
    }
    let got = flate_stream(&mut c, &[&data]);
    assert!(got == want, "len {} against {}", got.len(), want.len());
    assert!(
        c.lazy
            .as_ref()
            .is_some_and(|s| s.hash_offset < MAX_HASH_OFFSET)
    );
}
