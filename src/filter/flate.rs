//! FlateDecode (Go: pkg/filter/flateDecode.go and paeth.go): zlib decode with the PNG and TIFF
//! predictors, and the encoder wrapper around [`super::goflate`].
//!
//! # Decode
//!
//! Go reads through `zlib.NewReader`; here it is [`deflate::zlib::Reader`], whose `read` returns
//! the chunks of Go's `compress/flate` (a chunk ends at every 32 KiB of output, at every empty
//! stored block, and at the end). The chunks are the `Read` boundaries of the capacity model
//! (docs/design.md section 8, [`super::Source`]).
//!
//! One fact about those boundaries cannot be read from the chunks: whether Go delivered the last
//! chunk together with the end of the stream (`io.EOF`, or the error that truncates it) or
//! in a `Read` of its own, which decides whether `bytes.Buffer.ReadFrom` grows once more. Go's
//! last chunk stands alone exactly when the stream ended with no output after Go's last flush,
//! that is, after a window flush (the output length is a multiple of 32 KiB) or after a
//! sync-flush marker. [`FlateSource::end_carried`] knows the first from the length. For the
//! second it looks at the bytes of the input: a stream that is cut right after `00 00 FF FF`, or
//! that ends with `00 00 FF FF` and then an empty final block (`03 00` from zlib's `Z_FINISH`,
//! `01 00 00 FF FF` from Go's `Close`) and the Adler-32. That is a guess about bytes, not
//! the decoder's state: it is wrong when a block's data happens to end with those bytes, and for
//! any other stream whose last step has no output. An exact answer needs `deflate::Reader` to
//! say whether the step that produced the last chunk also ended the stream (docs/design.md
//! section 11, X1').
//!
//! Go's pools of readers and writers are not ported: a fresh reader or writer is used for every
//! call, which is what a reset one is equivalent to.
//!
//! # Memory
//!
//! The reader's window (`min(left, 256 KiB)`, `READER_BYTES`) is charged to the ledger and passed to it as its
//! `max_alloc`; the output is charged as it grows ([`super::Out`]).

use super::goflate::{Sink, ZlibWriter};
use super::{Decoded, Filter, Out};
use crate::error::Error;
use crate::gocap::Buffer;
use crate::ledger::{Held, Mem};
use deflate::zlib::Reader as ZlibReader;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};

/// `PredictorNo`: no prediction.
pub const PREDICTOR_NO: i64 = 1;
/// `PredictorTIFF`: TIFF prediction on every row.
pub const PREDICTOR_TIFF: i64 = 2;
/// `PredictorNone`: the PNG none filter on every row.
pub const PREDICTOR_NONE: i64 = 10;
/// `PredictorSub`.
pub const PREDICTOR_SUB: i64 = 11;
/// `PredictorUp`.
pub const PREDICTOR_UP: i64 = 12;
/// `PredictorAverage`.
pub const PREDICTOR_AVERAGE: i64 = 13;
/// `PredictorPaeth`.
pub const PREDICTOR_PAETH: i64 = 14;
/// `PredictorOptimum`: the best PNG filter for each row.
pub const PREDICTOR_OPTIMUM: i64 = 15;

/// The PNG row filters: the first byte of each row of a PNG-predicted stream.
const PNG_NONE: u8 = 0;
const PNG_SUB: u8 = 1;
const PNG_UP: u8 = 2;
const PNG_AVERAGE: u8 = 3;
const PNG_PAETH: u8 = 4;

/// Go's window: a flush of the decompressor at every multiple of this many output bytes.
const WINDOW: usize = 32 << 10;
/// What the zlib reader's window and one step of output are charged and allowed.
const READER_BYTES: u64 = 256 << 10;

fn ilen(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn params_err(msg: impl Into<Box<str>>) -> Error {
    Error::Predictor("params").context(msg)
}

fn rows_err(msg: impl Into<Box<str>>) -> Error {
    Error::Predictor("rows").context(msg)
}

fn overflow() -> Error {
    params_err("integer overflow")
}

/// The pdf-rs error for an error of deflate-rs. Its output limit is Go's limit on the decoded
/// size.
fn from_deflate(e: deflate::Error) -> Error {
    match e {
        deflate::Error::OutputLimit => Error::DecodeLimit,
        deflate::Error::Cancelled => Error::Cancelled,
        deflate::Error::OutOfMemory => Error::OutOfMemory,
        deflate::Error::UnexpectedEof => Error::UnexpectedEof,
        e => Error::Flate(e),
    }
}

/// The error of an `io::Error` that [`deflate::zlib::Reader`] returned.
fn flate_error(e: io::Error) -> Error {
    let inner = e
        .get_ref()
        .and_then(|s| s.downcast_ref::<deflate::Error>())
        .cloned();
    from_deflate(inner.unwrap_or_else(|| deflate::Error::from(e)))
}

/// The outcome of `io.ReadFull`.
enum Fill {
    /// The buffer is full.
    Full,
    /// `io.EOF`: the end, with nothing read.
    Eof,
    /// An error: the reader's own, or `io.ErrUnexpectedEOF` for an end after some bytes.
    Err(Error),
}

/// How the stream ended, as far as the reader said.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Not yet.
    No,
    /// A clean end (`io.EOF`).
    Clean,
    /// `io.ErrUnexpectedEOF`: the stream is cut.
    Truncated,
    /// Any other error.
    Failed,
}

/// A zlib stream as Go's `zlib.NewReader` and the `Read` boundaries of its output.
struct FlateSource<'a> {
    rd: ZlibReader<'a, &'a [u8]>,
    input: &'a [u8],
    buf: Vec<u8>,
    /// The output bytes returned so far.
    total: usize,
    ended: Ended,
    _window: Held,
    _buf: Held,
}

/// An empty stored block that is not final: zlib's `Z_SYNC_FLUSH` marker, after its (byte
/// aligned) block header bits.
const SYNC_MARKER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
/// What follows a sync flush when the stream is finished: an empty final fixed block (zlib's
/// `Z_FINISH`) or an empty final stored block (Go's `Close`).
const FINAL_FIXED: [u8; 2] = [0x03, 0x00];
const FINAL_STORED: [u8; 5] = [0x01, 0x00, 0x00, 0xff, 0xff];

impl<'a> FlateSource<'a> {
    /// `acquireZlibReader`. `max_output` is the most output the reader may produce.
    ///
    /// # Errors
    /// The header's errors (`zlib.ErrHeader`, `io.ErrUnexpectedEOF` for an input under two
    /// bytes, `zlib.ErrDictionary`), and the ledger's.
    fn open(
        input: &'a [u8],
        max_output: usize,
        mem: &Mem,
        cancel: &'a AtomicBool,
    ) -> Result<FlateSource<'a>, Error> {
        let window = mem.charge(mem.left().min(READER_BYTES))?;
        let (buf, buf_held) = mem.alloc_zeroed(WINDOW)?;
        let limits = deflate::Limits {
            max_output,
            max_alloc: usize::try_from(window.bytes()).unwrap_or(usize::MAX),
        };
        let rd = ZlibReader::new(input, &limits, cancel).map_err(from_deflate)?;
        Ok(FlateSource {
            rd,
            input,
            buf,
            total: 0,
            ended: Ended::No,
            _window: window,
            _buf: buf_held,
        })
    }

    /// Whether the stream ends with a sync flush and nothing after it but the empty block that
    /// ends the stream (and the Adler-32): Go's last chunk is then a flush of its own, and the
    /// end is a `Read` of its own. Read from the bytes of the input, because the chunks do not
    /// say it (see the module doc).
    fn ends_after_sync_flush(&self) -> bool {
        match self.ended {
            Ended::Truncated => self.input.ends_with(&SYNC_MARKER),
            Ended::Clean => match self.input.len().checked_sub(4) {
                Some(n) => {
                    let body = &self.input[..n];
                    [&FINAL_FIXED[..], &FINAL_STORED[..]].iter().any(|f| {
                        body.ends_with(f) && body[..body.len() - f.len()].ends_with(&SYNC_MARKER)
                    })
                }
                None => false,
            },
            Ended::No | Ended::Failed => false,
        }
    }

    /// `io.ReadFull`.
    fn read_full(&mut self, buf: &mut [u8]) -> (usize, Fill) {
        let mut n = 0;
        while n < buf.len() {
            match self.rd.read(&mut buf[n..]) {
                Ok(0) => {
                    let fill = if n == 0 {
                        Fill::Eof
                    } else {
                        Fill::Err(Error::UnexpectedEof)
                    };
                    return (n, fill);
                }
                Ok(k) => n += k,
                Err(e) => return (n, Fill::Err(flate_error(e))),
            }
        }
        (n, Fill::Full)
    }
}

impl super::Source for FlateSource<'_> {
    fn next(&mut self) -> Result<Option<&[u8]>, Error> {
        match self.rd.read(&mut self.buf) {
            Ok(0) => {
                self.ended = Ended::Clean;
                Ok(None)
            }
            Ok(n) => {
                self.total = self.total.saturating_add(n);
                Ok(Some(&self.buf[..n]))
            }
            Err(e) => {
                let e = flate_error(e);
                self.ended = if e == Error::UnexpectedEof {
                    Ended::Truncated
                } else {
                    Ended::Failed
                };
                Err(e)
            }
        }
    }

    fn end_carried(&self) -> bool {
        !self.total.is_multiple_of(WINDOW) && !self.ends_after_sync_flush()
    }
}

/// The most output the zlib reader may produce for the decode limit `limit` (Go reads at most
/// `limit + 1` bytes), or no limit.
fn reader_max_output(limit: i64) -> usize {
    if limit < 0 || limit == i64::MAX {
        usize::MAX
    } else {
        usize::try_from(limit.unsigned_abs() + 1).unwrap_or(usize::MAX)
    }
}

/// `DecodeLength`.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    f.check_decode_length(max_len)?;
    let predictor = f
        .parms
        .get(b"Predictor".as_slice())
        .copied()
        .filter(|&p| p != PREDICTOR_NO);
    // With a predictor the reader's output is consumed row by row and Go limits the rows, not
    // the reader.
    let max_output = if predictor.is_some() {
        usize::MAX
    } else {
        reader_max_output(f.decode_limit(-1))
    };
    let mut src = FlateSource::open(input, max_output, mem, cancel)?;
    match predictor {
        Some(p) => decode_post_process(f, &mut src, p, max_len, mem, cancel),
        None => pass_thru(f, &mut src, max_len, mem, cancel),
    }
}

/// `passThru`: a truncated stream is not an error, a bad checksum is.
fn pass_thru(
    f: &Filter,
    src: &mut FlateSource<'_>,
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    let c = f.copy_decoded(src, max_len, mem, cancel)?;
    match c.err {
        None => Ok(c.buf),
        Some(e @ Error::Flate(deflate::Error::Checksum)) => Err(e.context("flate decode")),
        // Workaround for missing support for partial flush in compress/flate
        // (https://github.com/golang/go/issues/31514).
        Some(Error::UnexpectedEof) => Ok(c.buf),
        Some(e) => Err(e),
    }
}

/// `validatePredictor`.
fn validate_predictor(predictor: i64) -> Result<(), Error> {
    if matches!(
        predictor,
        PREDICTOR_TIFF
            | PREDICTOR_NONE
            | PREDICTOR_SUB
            | PREDICTOR_UP
            | PREDICTOR_AVERAGE
            | PREDICTOR_PAETH
            | PREDICTOR_OPTIMUM
    ) {
        return Ok(());
    }
    Err(params_err(format!(
        "flate decode: undefined \"Predictor\" {predictor}"
    )))
}

/// `parameters`: Colors, BitsPerComponent and Columns.
fn parameters(f: &Filter) -> Result<(i64, i64, i64), Error> {
    let colors = match f.parms.get(b"Colors".as_slice()) {
        None => 1,
        Some(&c) if c <= 0 => return Err(params_err("flate decode: \"Colors\" must be > 0")),
        Some(&c) => c,
    };
    let bpc = match f.parms.get(b"BitsPerComponent".as_slice()) {
        None => 8,
        Some(&b) if [1, 2, 4, 8, 16].contains(&b) => b,
        Some(&b) => {
            return Err(params_err(format!(
                "flate decode: unexpected \"BitsPerComponent\": {b}"
            )));
        }
    };
    let columns = match f.parms.get(b"Columns".as_slice()) {
        None => 1,
        Some(&c) if c <= 0 => return Err(params_err("flate decode: \"Columns\" must be > 0")),
        Some(&c) => c,
    };
    Ok((colors, bpc, columns))
}

/// The sizes of a predicted row, in bytes.
struct RowParams {
    /// Without the PNG filter byte.
    row_size: i64,
    /// With it.
    row_len: i64,
    bytes_per_pixel: i64,
}

/// `predictorRowParams`, with Go's overflow checks.
fn predictor_row_params(
    predictor: i64,
    colors: i64,
    bpc: i64,
    columns: i64,
) -> Result<RowParams, Error> {
    let bits_per_pixel = bpc.checked_mul(colors).ok_or_else(overflow)?;
    let rounded = bits_per_pixel.checked_add(7).ok_or_else(overflow)?;
    let bytes_per_pixel = rounded / 8;

    let row_bits = bits_per_pixel.checked_mul(columns).ok_or_else(overflow)?;
    let row_bits_rounded = row_bits.checked_add(7).ok_or_else(overflow)?;
    let row_size = row_bits_rounded / 8;

    let row_len = if predictor == PREDICTOR_TIFF {
        row_size
    } else {
        // PNG prediction uses a row filter byte prefixing the pixel bytes of a row.
        row_size.checked_add(1).ok_or_else(overflow)?
    };
    Ok(RowParams {
        row_size,
        row_len,
        bytes_per_pixel,
    })
}

/// `applyHorDiff`: undoes TIFF predictor 2 on one row. Each colour component of a sample is the
/// difference from the same component of the preceding sample, modulo 2^bpc, on whole bpc-bit
/// components (7.4.4.4). 16-bit components are big-endian; narrower ones are packed MSB first,
/// and the padding bits at the end of the row are left alone.
fn apply_hor_diff(row: &mut [u8], colors: usize, bpc: i64, columns: usize) {
    match bpc {
        8 => {
            for i in colors..row.len() {
                row[i] = row[i].wrapping_add(row[i - colors]);
            }
        }
        16 => {
            let mut i = 2 * colors;
            while i + 1 < row.len() {
                let a = u16::from_be_bytes([row[i], row[i + 1]]);
                let b = u16::from_be_bytes([row[i - 2 * colors], row[i - 2 * colors + 1]]);
                let v = a.wrapping_add(b).to_be_bytes();
                row[i] = v[0];
                row[i + 1] = v[1];
                i += 2;
            }
        }
        _ => {
            // bpc 1, 2 or 4: components are packed within bytes.
            let bpc = usize::try_from(bpc).unwrap_or(1);
            let mask = (1u8 << bpc) - 1;
            let get = |row: &[u8], k: usize| -> u8 {
                let bit = k * bpc;
                (row[bit / 8] >> (8 - bpc - bit % 8)) & mask
            };
            let n = colors * columns;
            for k in colors..n {
                let bit = k * bpc;
                let shift = 8 - bpc - bit % 8;
                let v = (get(row, k) + get(row, k - colors)) & mask;
                row[bit / 8] = (row[bit / 8] & !(mask << shift)) | (v << shift);
            }
        }
    }
}

/// `abs` of paeth.go as `i32`.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let pc = i32::from(c);
    let mut pa = i32::from(b) - pc;
    let mut pb = i32::from(a) - pc;
    let pc = (pa + pb).abs();
    pa = pa.abs();
    pb = pb.abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// `filterPaeth`: undoes the Paeth filter on `cdat`, the current row, from `pdat`, the previous.
fn filter_paeth(cdat: &mut [u8], pdat: &[u8], bytes_per_pixel: usize) {
    for i in 0..bytes_per_pixel {
        let (mut a, mut c) = (0i32, 0i32);
        let mut j = i;
        while j < cdat.len() {
            let b = i32::from(pdat[j]);
            let mut pa = b - c;
            let mut pb = a - c;
            let pc = (pa + pb).abs();
            pa = pa.abs();
            pb = pb.abs();
            if pa > pb || pa > pc {
                a = if pb <= pc { b } else { c };
            }
            a += i32::from(cdat[j]);
            a &= 0xff;
            cdat[j] = a as u8;
            c = b;
            j += bytes_per_pixel;
        }
    }
}

/// `processRow`: undoes the prediction of the row `cr` in place, given the previous reconstructed
/// row `pr`, and returns the row's data (without the PNG filter byte).
fn process_row<'a>(
    pr: &[u8],
    cr: &'a mut [u8],
    predictor: i64,
    colors: usize,
    bpc: i64,
    columns: usize,
    bytes_per_pixel: usize,
) -> Result<&'a [u8], Error> {
    if predictor == PREDICTOR_TIFF {
        apply_hor_diff(cr, colors, bpc, columns);
        return Ok(cr);
    }

    // Get the row filter from the first byte. The predictor of the decoding filter need not
    // match the one used when the data was encoded if both are 10 or more.
    let (head, cdat) = cr.split_at_mut(1);
    let pdat = &pr[1..];
    match head[0] {
        PNG_NONE => {}
        PNG_SUB => {
            for i in bytes_per_pixel..cdat.len() {
                cdat[i] = cdat[i].wrapping_add(cdat[i - bytes_per_pixel]);
            }
        }
        PNG_UP => {
            for (c, &p) in cdat.iter_mut().zip(pdat) {
                *c = c.wrapping_add(p);
            }
        }
        PNG_AVERAGE => {
            // The average of the two neighbouring pixels (left and above):
            // Raw(x) - floor((Raw(x-bpp)+Prior(x))/2).
            for i in 0..bytes_per_pixel {
                cdat[i] = cdat[i].wrapping_add(pdat[i] / 2);
            }
            for i in bytes_per_pixel..cdat.len() {
                let avg = (u16::from(cdat[i - bytes_per_pixel]) + u16::from(pdat[i])) / 2;
                cdat[i] = cdat[i].wrapping_add(avg as u8);
            }
        }
        PNG_PAETH => filter_paeth(cdat, pdat, bytes_per_pixel),
        f => {
            return Err(rows_err(format!(
                "flate decode: unexpected PNG predictor {f}"
            )));
        }
    }
    Ok(cdat)
}

/// `decodePostProcess`.
fn decode_post_process(
    f: &Filter,
    src: &mut FlateSource<'_>,
    predictor: i64,
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    validate_predictor(predictor)?;
    let (colors, bpc, columns) = parameters(f)?;
    let rp = predictor_row_params(predictor, colors, bpc, columns)?;
    let limit = f.decode_limit(-1);
    if limit >= 0 && rp.row_len > limit {
        return Err(Error::DecodeLimit);
    }
    let b = decode_post_process_rows(
        f, src, max_len, &rp, predictor, colors, bpc, columns, mem, cancel,
    )?;
    if max_len < 0 && ilen(b.len()) % rp.row_size > 0 {
        return Err(rows_err("flate decode: postprocessing failed"));
    }
    Ok(b)
}

/// `decodePostProcessRows`: reads the rows, undoes the prediction and writes the data of each
/// row to the output with one `Write`.
fn decode_post_process_rows(
    f: &Filter,
    src: &mut FlateSource<'_>,
    max_len: i64,
    rp: &RowParams,
    predictor: i64,
    colors: i64,
    bpc: i64,
    columns: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    let to_usize = |n: i64| usize::try_from(n).map_err(|_| Error::OutOfMemory);
    let m = to_usize(rp.row_len)?;
    let bytes_per_pixel = to_usize(rp.bytes_per_pixel)?;
    let colors = to_usize(colors)?;
    let columns = to_usize(columns)?;
    // cr and pr are the bytes of the current and the previous row.
    let (mut cr, _cr_held) = mem.alloc_zeroed(m)?;
    let (mut pr, _pr_held) = mem.alloc_zeroed(m)?;

    let mut out = Out::new(mem)?;
    let mut cap = Buffer::new();
    let mut wrote = false;
    let limit = f.decode_limit(-1);

    while max_len < 0 || ilen(out.len()) < max_len {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // Read the decompressed bytes of one pixel row.
        let (n, fill) = src.read_full(&mut cr);
        match fill {
            Fill::Full => {}
            Fill::Eof => break,
            Fill::Err(Error::UnexpectedEof) if n == 0 => break,
            Fill::Err(e) => return Err(e),
        }

        // Reject an oversized row before reconstructing it or growing the output buffer.
        if max_len < 0 && limit >= 0 && rp.row_size > limit - ilen(out.len()) {
            return Err(Error::DecodeLimit);
        }

        let remaining = if max_len >= 0 {
            max_len - ilen(out.len())
        } else {
            -1
        };
        let d = process_row(
            &pr,
            &mut cr,
            predictor,
            colors,
            bpc,
            columns,
            bytes_per_pixel,
        )?;
        let d = match usize::try_from(remaining) {
            Ok(r) if r < d.len() => &d[..r],
            _ => d,
        };
        out.extend(d)?;
        cap.write(d.len())?;
        wrote = true;

        std::mem::swap(&mut pr, &mut cr);
    }
    Ok(out.finish(cap.cap(), !wrote))
}

/// `Encode`: zlib at the default level, with the forward predictor of the filter's parameters.
/// The output is what Go's pooled writer produces; a reset writer equals a fresh one.
pub(super) fn encode(
    f: &Filter,
    input: &[u8],
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    let mut out = Out::new(mem)?;
    let mut cap = Buffer::new();
    let mut w = ZlibWriter::new(mem)?;
    let mut sink = |b: &[u8]| -> Result<(), Error> {
        out.extend(b)?;
        cap.write(b.len())
    };
    encode_pre_process(f, &mut w, input, &mut sink, mem, cancel)?;
    w.close(&mut sink)?;
    Ok(out.finish(cap.cap(), false))
}

/// `encodePreProcess`: applies the forward predictor and compresses the rows.
fn encode_pre_process(
    f: &Filter,
    w: &mut ZlibWriter,
    input: &[u8],
    sink: &mut Sink<'_>,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let predictor = f.parms.get(b"Predictor".as_slice()).copied();
    let Some(predictor) = predictor.filter(|&p| p != PREDICTOR_NO) else {
        // io.Copy writes the whole input in one call, and none for an empty input.
        if !input.is_empty() {
            w.write(input, sink, cancel)?;
        }
        return Ok(());
    };
    validate_predictor(predictor)?;
    let (colors, bpc, columns) = parameters(f)?;
    if predictor == PREDICTOR_TIFF && bpc != 8 {
        return Err(params_err(format!(
            "flate encode: TIFF predictor with BitsPerComponent {bpc} unsupported"
        )));
    }
    let rp = predictor_row_params(predictor, colors, bpc, columns)?;
    let limit = f.decode_limit(-1);
    if limit >= 0 && rp.row_len > limit {
        return Err(Error::DecodeLimit);
    }
    encode_predictor_rows(w, input, predictor, colors, &rp, sink, mem, cancel)
}

/// `encodePredictorRows`: one `Write` per row.
fn encode_predictor_rows(
    w: &mut ZlibWriter,
    input: &[u8],
    predictor: i64,
    colors: i64,
    rp: &RowParams,
    sink: &mut Sink<'_>,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let to_usize = |n: i64| usize::try_from(n).map_err(|_| Error::OutOfMemory);
    let row_size = to_usize(rp.row_size)?;
    let row_len = to_usize(rp.row_len)?;
    let bytes_per_pixel = to_usize(rp.bytes_per_pixel)?;
    let colors = to_usize(colors)?;
    let (mut current, _h1) = mem.alloc_zeroed(row_size)?;
    let (mut previous, _h2) = mem.alloc_zeroed(row_size)?;
    let (mut encoded, _h3) = mem.alloc_zeroed(row_len)?;
    let (mut candidate, _h4) = if predictor == PREDICTOR_OPTIMUM {
        mem.alloc_zeroed(row_size)?
    } else {
        (Vec::new(), mem.charge(0)?)
    };
    let mut pos = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let rest = &input[pos..];
        if rest.is_empty() {
            return Ok(());
        }
        if rest.len() < row_size {
            return Err(Error::UnexpectedEof.context("flate encode: incomplete predictor row"));
        }
        current.copy_from_slice(&rest[..row_size]);
        pos += row_size;
        if predictor == PREDICTOR_TIFF {
            encode_tiff_row(&mut encoded, &current, colors);
        } else {
            let filter = if predictor == PREDICTOR_OPTIMUM {
                best_png_filter(
                    &mut encoded[1..],
                    &mut candidate,
                    &current,
                    &previous,
                    bytes_per_pixel,
                )
            } else {
                // The PNG filter of the predictor: 10 is none, 14 is Paeth.
                let filter = u8::try_from(predictor - PREDICTOR_NONE).unwrap_or(PNG_NONE);
                filter_png_row(
                    &mut encoded[1..],
                    &current,
                    &previous,
                    filter,
                    bytes_per_pixel,
                );
                filter
            };
            encoded[0] = filter;
        }
        w.write(&encoded, sink, cancel)?;
        std::mem::swap(&mut current, &mut previous);
    }
}

/// `encodeTIFFRow`.
fn encode_tiff_row(dst: &mut [u8], src: &[u8], colors: usize) {
    dst.copy_from_slice(src);
    for i in (colors..src.len()).rev() {
        dst[i] = dst[i].wrapping_sub(src[i - colors]);
    }
}

/// `bestPNGFilter`: the filter with the least sum of absolute values (as signed bytes) over the
/// row; its filtered row is left in `dst`.
fn best_png_filter(
    dst: &mut [u8],
    candidate: &mut [u8],
    current: &[u8],
    previous: &[u8],
    bytes_per_pixel: usize,
) -> u8 {
    let mut best = PNG_NONE;
    let mut best_score: i64 = -1;
    for filter in PNG_NONE..=PNG_PAETH {
        filter_png_row(candidate, current, previous, filter, bytes_per_pixel);
        let score: i64 = candidate
            .iter()
            .map(|&b| i64::from((b as i8).unsigned_abs()))
            .sum();
        if best_score < 0 || score < best_score {
            best = filter;
            best_score = score;
            dst.copy_from_slice(candidate);
        }
    }
    best
}

/// `filterPNGRow`: applies the PNG filter `filter` to `current`, given the `previous` row.
fn filter_png_row(
    dst: &mut [u8],
    current: &[u8],
    previous: &[u8],
    filter: u8,
    bytes_per_pixel: usize,
) {
    for (i, &b) in current.iter().enumerate() {
        let (left, upper_left) = if i >= bytes_per_pixel {
            (current[i - bytes_per_pixel], previous[i - bytes_per_pixel])
        } else {
            (0, 0)
        };
        let above = previous[i];
        dst[i] = match filter {
            PNG_NONE => b,
            PNG_SUB => b.wrapping_sub(left),
            PNG_UP => b.wrapping_sub(above),
            PNG_AVERAGE => b.wrapping_sub(((u16::from(left) + u16::from(above)) / 2) as u8),
            PNG_PAETH => b.wrapping_sub(paeth(left, above, upper_left)),
            _ => continue,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::mem;
    use crate::filter::{FLATE, Parms, new_filter};

    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn parms(p: &[(&str, i64)]) -> Parms {
        p.iter().map(|&(k, v)| (k.as_bytes().to_vec(), v)).collect()
    }

    fn filter(p: &[(&str, i64)], limit: Option<i64>) -> Filter {
        new_filter(FLATE, parms(p), limit).unwrap()
    }

    /// Go's `zlib.NewWriter` for the tests: the in-crate port of its default level.
    fn zlib(data: &[u8]) -> Vec<u8> {
        let m = mem();
        let mut out = Vec::new();
        let mut w = ZlibWriter::new(&m).unwrap();
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        w.write(data, &mut sink, &cancel()).unwrap();
        w.close(&mut sink).unwrap();
        out
    }

    /// `flatePartialFlushTestData`: written and flushed, not closed.
    fn zlib_partial_flush(data: &[u8]) -> Vec<u8> {
        let m = mem();
        let mut out = Vec::new();
        let mut w = ZlibWriter::new(&m).unwrap();
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        w.write(data, &mut sink, &cancel()).unwrap();
        w.flush(&mut sink).unwrap();
        out
    }

    fn checksum_failure(data: &[u8]) -> Vec<u8> {
        let mut b = zlib(data);
        let last = b.len() - 1;
        b[last] ^= 0xff;
        b
    }

    fn decode(f: &Filter, input: &[u8]) -> Result<Decoded, Error> {
        f.decode(input, &mem(), &cancel())
    }

    fn decoded(f: &Filter, input: &[u8]) -> Vec<u8> {
        decode(f, input).unwrap().as_slice().to_vec()
    }

    fn encode_bytes(f: &Filter, data: &[u8]) -> Result<Vec<u8>, Error> {
        f.encode(data, &mem(), &cancel())
            .map(|d| d.as_slice().to_vec())
    }

    #[test]
    fn decodes_zlib_and_reports_the_go_capacity() {
        let f = filter(&[], None);
        let d = decode(&f, &zlib(b"hello")).unwrap();
        assert_eq!(d.as_slice(), b"hello");
        // The last chunk comes with the end: no final grow of 512.
        assert_eq!((d.go_cap(), d.is_nil()), (512, false));
        // An empty stream: ReadFrom made its first 512 bytes.
        let d = decode(&f, &zlib(b"")).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (0, 512, false));
        // A stream that ends exactly at a window flush: the end is a read of its own.
        let data = vec![b'a'; 32768];
        let d = decode(&f, &zlib(&data)).unwrap();
        assert_eq!(d.as_slice(), &data[..]);
        let want = crate::gocap::simulate_read_from([32768], false)
            .unwrap()
            .cap();
        assert_eq!(d.go_cap(), want);
        // Over a window: the chunks are 32768 and the rest, the end comes with the rest.
        let data: Vec<u8> = (0..40000u32).map(|i| (i * 31 % 251) as u8).collect();
        let d = decode(&f, &zlib(&data)).unwrap();
        assert_eq!(d.as_slice(), &data[..]);
        let want = crate::gocap::simulate_read_from([32768, 7232], true)
            .unwrap()
            .cap();
        assert_eq!(d.go_cap(), want);
    }

    /// `Write`, `Flush`, `Close` of Go's writer.
    fn zlib_flush_close(data: &[u8]) -> Vec<u8> {
        let m = mem();
        let mut out = Vec::new();
        let mut w = ZlibWriter::new(&m).unwrap();
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            out.extend_from_slice(b);
            Ok(())
        };
        w.write(data, &mut sink, &cancel()).unwrap();
        w.flush(&mut sink).unwrap();
        w.close(&mut sink).unwrap();
        out
    }

    #[test]
    fn capacity_after_a_sync_flush() {
        let f = filter(&[], None);
        // Go 1.27's Close ends with an empty final fixed block (03 00); with a Flush before it,
        // the data was flushed at the sync marker and the end is a read of its own: 512, then
        // one more grow.
        let z = zlib_flush_close(b"hello");
        assert!(z[..z.len() - 4].ends_with(&[0, 0, 0xff, 0xff, 0x03, 0x00]));
        let d = decode(&f, &z).unwrap();
        assert_eq!((d.as_slice(), d.go_cap()), (&b"hello"[..], 1024));
        // Without the Flush the final empty block is what flushes the data, and ends the
        // stream in the same step.
        let d = decode(&f, &zlib(b"hello")).unwrap();
        assert_eq!(d.go_cap(), 512);
        // zlib's Z_SYNC_FLUSH then Z_FINISH: a sync marker, an empty final fixed block.
        let mut z = zlib_partial_flush(b"hello");
        z.extend_from_slice(&[0x03, 0x00]);
        z.extend_from_slice(&adler32(b"hello"));
        let d = decode(&f, &z).unwrap();
        assert_eq!((d.as_slice(), d.go_cap()), (&b"hello"[..], 1024));
    }

    /// Adler-32, for the streams that are put together by hand.
    fn adler32(data: &[u8]) -> [u8; 4] {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        ((b << 16) | a).to_be_bytes()
    }

    #[test]
    fn flate_repeated_reuse() {
        // TestFlateRepeatedReuse.
        let f = filter(&[], None);
        for i in 0..100 {
            let want = format!("payload-{i}-{}", "x".repeat(i % 31));
            let enc = encode_bytes(&f, want.as_bytes()).unwrap();
            assert_eq!(decoded(&f, &enc), want.as_bytes());
        }
    }

    #[test]
    fn reader_failures() {
        // TestFlateReaderFailuresDoNotAffectValidDecoding.
        let f = filter(&[], None);
        assert_eq!(
            decode(&f, b"not a zlib stream").err(),
            Some(Error::Flate(deflate::Error::Header))
        );
        let e = decode(&f, &checksum_failure(b"checksum")).err().unwrap();
        // TestFlateChecksumFailurePreservesSentinelWithoutCLILogging.
        assert_eq!(e.root(), &Error::Flate(deflate::Error::Checksum));
        assert!(e.to_string().contains("flate decode"));
        assert_eq!(e.class(), "flate/checksum");
        assert_eq!(decoded(&f, &zlib(b"valid")), b"valid");
        // No input is an unexpected EOF, from the header.
        assert_eq!(decode(&f, b"").err(), Some(Error::UnexpectedEof));
        // Corrupt data inside the stream.
        let mut z = zlib(b"some data that is long enough to have a block");
        z[3] ^= 0xff;
        z[4] ^= 0xff;
        let e = decode(&f, &z).err().unwrap();
        assert!(
            matches!(
                e.class(),
                "flate/corrupt" | "flate/checksum" | "flate/internal" | "unexpected-eof"
            ),
            "{e}"
        );
    }

    #[test]
    fn decode_limit_exceeded() {
        // TestDecodeLimitExceeded.
        let f = filter(&[], Some(4));
        assert_eq!(decode(&f, &zlib(b"hello")).err(), Some(Error::DecodeLimit));
        assert!(decode(&filter(&[], Some(5)), &zlib(b"hello")).is_ok());
        // TestFlateDecodeLimitSurvivesToleratedUnexpectedEOF.
        assert_eq!(
            decode(&f, &zlib_partial_flush(b"hello")).err(),
            Some(Error::DecodeLimit)
        );
        // A prefix over the limit, and the prefix at the limit.
        assert_eq!(
            f.decode_length(&zlib(b"hello"), 5, &mem(), &cancel()).err(),
            Some(Error::DecodeLimit)
        );
        let d = f
            .decode_length(&zlib(b"hello"), 4, &mem(), &cancel())
            .unwrap();
        assert_eq!(d.as_slice(), b"hell");
        // No limit.
        assert!(decode(&filter(&[], Some(-1)), &zlib(b"hello")).is_ok());
    }

    #[test]
    fn truncated_stream_is_tolerated() {
        // A stream cut after a flush: the data is kept.
        let f = filter(&[], None);
        let d = decode(&f, &zlib_partial_flush(b"hello")).unwrap();
        assert_eq!(d.as_slice(), b"hello");
        // A stream cut in the middle of the data too.
        let z = zlib(&[b'q'; 100]);
        let d = decode(&f, &z[..z.len() - 6]).unwrap();
        assert!(d.len() <= 100);
        // A prefix longer than what is there: the stream dict reports it; CopyN of a stream
        // that ends cleanly reports EOF.
        let e = f.decode_length(&zlib(b"hello"), 6, &mem(), &cancel()).err();
        assert_eq!(e, Some(Error::Sentinel(crate::error::Sentinel::Eof)));
        let d = f
            .decode_length(&zlib_partial_flush(b"hello"), 6, &mem(), &cancel())
            .unwrap();
        assert_eq!(d.as_slice(), b"hello");
        // Cut right after a sync flush: the chunk was a flush of its own, and the end is a read
        // of its own, so ReadFrom grew once more.
        let d = decode(&f, &zlib_partial_flush(b"hello")).unwrap();
        assert_eq!(d.go_cap(), 1024);
    }

    #[test]
    fn cancelled() {
        let f = filter(&[], None);
        let c = AtomicBool::new(true);
        assert_eq!(
            f.decode(&zlib(b"hello"), &mem(), &c).err(),
            Some(Error::Cancelled)
        );
    }

    #[test]
    fn predictor_rejects_invalid_parameters() {
        let empty = zlib(b"");
        // TestFlatePredictorRejectsInvalidColors / Columns.
        for (name, p) in [("Colors", -1), ("Columns", -1)] {
            let f = filter(&[("Predictor", PREDICTOR_NONE), (name, p)], None);
            let e = decode(&f, &empty).err().unwrap();
            assert_eq!(e.class(), "predictor/params");
            assert!(e.to_string().contains(name), "{e}");
        }
        // TestFlatePredictorRejectsOverflowingRowSize.
        let f = filter(
            &[
                ("Predictor", PREDICTOR_NONE),
                ("Colors", i64::MAX),
                ("Columns", 2),
            ],
            None,
        );
        let e = decode(&f, &empty).err().unwrap();
        assert!(e.to_string().contains("integer overflow"), "{e}");
        // A predictor that does not exist, a bad BitsPerComponent.
        let f = filter(&[("Predictor", 3)], None);
        assert!(
            decode(&f, &empty)
                .err()
                .unwrap()
                .to_string()
                .contains("Predictor")
        );
        let f = filter(
            &[("Predictor", PREDICTOR_SUB), ("BitsPerComponent", 3)],
            None,
        );
        assert!(
            decode(&f, &empty)
                .err()
                .unwrap()
                .to_string()
                .contains("BitsPerComponent")
        );
        // TestFlatePredictorRejectsRowLargerThanDecodeLimit.
        let f = filter(&[("Predictor", PREDICTOR_NONE), ("Columns", 8)], Some(4));
        assert_eq!(decode(&f, &empty).err(), Some(Error::DecodeLimit));
    }

    #[test]
    fn predictor_ignores_unexpected_eof() {
        // TestFlatePredictorIgnoresUnexpectedEOF.
        let f = filter(&[("Predictor", PREDICTOR_NONE), ("Columns", 3)], None);
        let d = decode(&f, &zlib_partial_flush(&[PNG_NONE, b'f', b'o', b'o'])).unwrap();
        assert_eq!(d.as_slice(), b"foo");
        assert_eq!(decoded(&f, &zlib(&[PNG_NONE, b'b', b'a', b'r'])), b"bar");
        // No rows: the output buffer was never written to, so it is nil.
        let d = decode(&f, &zlib(b"")).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (0, 0, true));
        // A partial row at a clean end is an unexpected EOF; a stream cut inside a row too.
        let e = decode(&f, &zlib(&[PNG_NONE, b'a'])).err();
        assert_eq!(e, Some(Error::UnexpectedEof));
        let e = decode(&f, &zlib_partial_flush(&[PNG_NONE, b'a'])).err();
        assert_eq!(e, Some(Error::UnexpectedEof));
        // The second row is cut after its first byte.
        let f = filter(&[("Predictor", PREDICTOR_NONE), ("Columns", 2)], None);
        assert_eq!(
            decode(&f, &zlib(&[PNG_NONE, 1, 2, PNG_NONE])).err(),
            Some(Error::UnexpectedEof)
        );
        // A bad PNG filter byte.
        let e = decode(&f, &zlib(&[9, 1, 2])).err().unwrap();
        assert_eq!(e.class(), "predictor/rows");
    }

    #[test]
    fn predictor_capacity_is_the_write_path() {
        // Three one-byte rows are three writes of 1 byte: the first makes a 64 byte buffer.
        let f = filter(&[("Predictor", PREDICTOR_NONE), ("Columns", 1)], None);
        let d = decode(&f, &zlib(&[0, 1, 0, 2, 0, 3])).unwrap();
        assert_eq!((d.as_slice(), d.go_cap()), (&[1u8, 2, 3][..], 64));
    }

    #[test]
    fn tiff_predictor_components() {
        // TestFlateTIFFPredictorComponents.
        type Case = (&'static str, i64, i64, i64, &'static [u8], &'static [u8]);
        let cases: [Case; 15] = [
            ("1 bit", 1, 1, 8, &[0x80], &[0xff]),
            ("1 bit padding", 1, 1, 3, &[0x9f], &[0xff]),
            ("1 bit RGB", 1, 3, 3, &[0xaf, 0x55], &[0xb8, 0x55]),
            ("2 bit wrap", 2, 1, 4, &[0xc1], &[0xfc]),
            (
                "2 bit colors and padding",
                2,
                2,
                3,
                &[0x6e, 0x5b],
                &[0x60, 0x5b],
            ),
            ("4 bit colors", 4, 2, 2, &[0x12, 0x11], &[0x12, 0x23]),
            (
                "4 bit RGB",
                4,
                3,
                2,
                &[0x12, 0x3f, 0xe1],
                &[0x12, 0x30, 0x04],
            ),
            ("4 bit padding", 4, 1, 3, &[0x1f, 0x2b], &[0x10, 0x2b]),
            (
                "4 bit rows",
                4,
                2,
                2,
                &[0x12, 0x11, 0xfe, 0x11],
                &[0x12, 0x23, 0xfe, 0x0f],
            ),
            (
                "8 bit RGB",
                8,
                3,
                3,
                &[10, 20, 30, 1, 2, 3, 255, 254, 253],
                &[10, 20, 30, 11, 22, 33, 10, 20, 30],
            ),
            (
                "16 bit carry",
                16,
                1,
                2,
                &[0x01, 0xff, 0, 1],
                &[0x01, 0xff, 2, 0],
            ),
            (
                "16 bit wrap",
                16,
                1,
                2,
                &[0xff, 0xff, 0, 1],
                &[0xff, 0xff, 0, 0],
            ),
            (
                "16 bit RGB",
                16,
                3,
                2,
                &[1, 0xff, 0xff, 0xff, 0x10, 0, 0, 1, 0, 1, 0xf0, 1],
                &[1, 0xff, 0xff, 0xff, 0x10, 0, 2, 0, 0, 0, 0, 1],
            ),
            (
                "16 bit rows",
                16,
                1,
                2,
                &[1, 0xff, 0, 1, 0, 1, 0, 1],
                &[1, 0xff, 2, 0, 0, 1, 0, 2],
            ),
            (
                "single pixel",
                16,
                3,
                1,
                &[1, 2, 3, 4, 5, 6],
                &[1, 2, 3, 4, 5, 6],
            ),
        ];
        for (name, bpc, colors, columns, encoded, want) in cases {
            let f = filter(
                &[
                    ("Predictor", PREDICTOR_TIFF),
                    ("Colors", colors),
                    ("BitsPerComponent", bpc),
                    ("Columns", columns),
                ],
                None,
            );
            for limit in [-1, 0, 1, ilen(want.len()) - 1, ilen(want.len())] {
                let z = zlib(encoded);
                let d = f
                    .decode_length(&z, limit, &mem(), &cancel())
                    .unwrap_or_else(|e| panic!("{name} limit {limit}: {e}"));
                let want = match usize::try_from(limit) {
                    Ok(l) => &want[..l],
                    Err(_) => want,
                };
                assert_eq!(d.as_slice(), want, "{name} limit {limit}");
            }
        }
    }

    #[test]
    fn tiff_predictor_incomplete_row() {
        // TestFlateTIFFPredictorIncompleteRow.
        for bpc in [1i64, 2, 4, 8, 16] {
            let f = filter(
                &[
                    ("Predictor", PREDICTOR_TIFF),
                    ("BitsPerComponent", bpc),
                    ("Columns", 9),
                ],
                None,
            );
            let row_size = (9 * usize::try_from(bpc).unwrap()).div_ceil(8);
            let e = decode(&f, &zlib(&vec![0; row_size - 1])).err();
            assert_eq!(e, Some(Error::UnexpectedEof), "bpc {bpc}");
        }
    }

    #[test]
    fn predictor_decode_limit_grid() {
        // TestFlatePredictorDecodeLimit and checkPredictorDecodeLimit.
        const CONTENT: &[u8] = b"abcdef";
        for predictor in [
            PREDICTOR_TIFF,
            PREDICTOR_NONE,
            PREDICTOR_SUB,
            PREDICTOR_UP,
            PREDICTOR_AVERAGE,
            PREDICTOR_PAETH,
            PREDICTOR_OPTIMUM,
        ] {
            let p = [("Predictor", predictor), ("Columns", 3)];
            let data = encode_bytes(&filter(&p, None), CONTENT).unwrap();
            for limit in [5i64, 6, 7, 0, -1] {
                for prefix in [-1i64, 0, 1, 5, 6] {
                    let f = filter(&p, Some(limit));
                    let want = match usize::try_from(prefix) {
                        Ok(n) => &CONTENT[..n],
                        Err(_) => CONTENT,
                    };
                    let r = f.decode_length(&data, prefix, &mem(), &cancel());
                    let ctx = format!("predictor {predictor} limit {limit} prefix {prefix}");
                    if limit > 0 && ilen(want.len()) > limit {
                        assert_eq!(r.err(), Some(Error::DecodeLimit), "{ctx}");
                    } else {
                        assert_eq!(r.unwrap().as_slice(), want, "{ctx}");
                    }
                }
            }
        }
    }

    #[test]
    fn predictor_limit_before_reconstruction() {
        // TestFlatePredictorLimitBeforeReconstruction: the second row is over the limit.
        for (predictor, row) in [
            (PREDICTOR_TIFF, vec![1u8, 2, 3]),
            (PREDICTOR_SUB, vec![PNG_SUB, 1, 2, 3]),
        ] {
            let f = filter(&[("Predictor", predictor), ("Columns", 3)], Some(5));
            let data = zlib(&[row.clone(), row].concat());
            assert_eq!(decode(&f, &data).err(), Some(Error::DecodeLimit));
        }
    }

    #[test]
    fn predictor_encode_round_trip() {
        // TestFlatePredictorEncodeRoundTrip.
        let data = [
            10u8, 20, 30, 11, 22, 33, 12, 24, 36, 13, 26, 39, 14, 28, 42, 15, 30, 45,
        ];
        for predictor in [
            PREDICTOR_TIFF,
            PREDICTOR_NONE,
            PREDICTOR_SUB,
            PREDICTOR_UP,
            PREDICTOR_AVERAGE,
            PREDICTOR_PAETH,
            PREDICTOR_OPTIMUM,
        ] {
            let f = filter(
                &[("Predictor", predictor), ("Colors", 3), ("Columns", 3)],
                None,
            );
            let enc = encode_bytes(&f, &data).unwrap();
            assert_eq!(decoded(&f, &enc), data, "predictor {predictor}");
        }
    }

    #[test]
    fn predictor_packed_samples() {
        // TestFlatePredictorPackedSamples.
        let cases: [(i64, i64, &[u8]); 2] = [
            (1, 9, &[0x85, 0x80, 0xc3, 0x00]),
            (16, 2, &[1, 2, 3, 4, 5, 6, 7, 8]),
        ];
        for (bpc, columns, data) in cases {
            let f = filter(
                &[
                    ("Predictor", PREDICTOR_PAETH),
                    ("BitsPerComponent", bpc),
                    ("Columns", columns),
                ],
                None,
            );
            let enc = encode_bytes(&f, data).unwrap();
            assert_eq!(decoded(&f, &enc), data, "bpc {bpc}");
        }
    }

    #[test]
    fn predictor_encoding_format() {
        // TestFlatePredictorEncodingFormat.
        let cases: [(&str, i64, &[u8], &[u8]); 2] = [
            (
                "TIFF",
                PREDICTOR_TIFF,
                &[10, 12, 15, 20, 22, 25],
                &[10, 2, 3, 20, 2, 3],
            ),
            (
                "PNG-Up",
                PREDICTOR_UP,
                &[10, 12, 15, 20, 22, 25],
                &[PNG_UP, 10, 12, 15, PNG_UP, 10, 10, 10],
            ),
        ];
        for (name, predictor, data, want) in cases {
            let f = filter(&[("Predictor", predictor), ("Columns", 3)], None);
            let enc = encode_bytes(&f, data).unwrap();
            let raw = deflate::zlib::decode(
                &enc,
                &deflate::Limits {
                    max_output: 1 << 20,
                    max_alloc: 1 << 24,
                },
                &cancel(),
            )
            .unwrap();
            assert_eq!(raw, want, "{name}");
        }
        let bad: [(&str, Vec<(&str, i64)>); 3] = [
            (
                "short row",
                vec![("Predictor", PREDICTOR_UP), ("Columns", 4)],
            ),
            (
                "unsupported TIFF depth",
                vec![
                    ("Predictor", PREDICTOR_TIFF),
                    ("BitsPerComponent", 16),
                    ("Columns", 4),
                ],
            ),
            (
                "invalid columns",
                vec![("Predictor", PREDICTOR_UP), ("Columns", 0)],
            ),
        ];
        for (name, p) in bad {
            let f = filter(&p, None);
            assert!(encode_bytes(&f, &[1, 2, 3]).is_err(), "{name}");
        }
        let f = filter(&[("Predictor", PREDICTOR_UP), ("Columns", 4)], None);
        let e = encode_bytes(&f, &[1, 2, 3]).err().unwrap();
        assert_eq!(e.class(), "unexpected-eof");
    }

    #[test]
    fn encode_matches_the_in_crate_writer_and_capacity() {
        // No predictor: one Write of the whole input; the capacity is the write path over the
        // writer's calls (the header, the blocks, the trailer).
        let f = filter(&[], None);
        let data = b"Hello, Gopher! Hello, Gopher! Hello, Gopher!";
        let enc = f.encode(data, &mem(), &cancel()).unwrap();
        assert_eq!(enc.as_slice(), &zlib(data)[..]);
        assert!(enc.go_cap() >= enc.len() && !enc.is_nil());
        // Empty content is the empty zlib stream.
        let empty = encode_bytes(&f, b"").unwrap();
        assert_eq!(empty, zlib(b""));
        assert_eq!(decoded(&f, &empty), b"");
    }

    #[test]
    fn round_trips_the_test_files() {
        // TestEncodeDecode for Flate.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/filter/");
        for name in [
            "gettysburg.txt",
            "e.txt",
            "pi.txt",
            "Mark.Twain-Tom.Sawyer.txt",
        ] {
            let data = std::fs::read(format!("{dir}{name}")).unwrap();
            let f = filter(&[], None);
            let enc = encode_bytes(&f, &data).unwrap();
            assert!(decoded(&f, &enc) == data, "{name}");
        }
    }
}
