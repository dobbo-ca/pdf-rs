//! CCITTFaxDecode (Go: pkg/filter/ccittDecode.go and golang.org/x/image/ccitt v0.47.0
//! reader.go). Group 3 one-dimensional (`K = 0`) and Group 4 (`K < 0`) only; `K > 0` is an error,
//! as in Go. The reader is Go's, with its `Options` (`Invert` is `BlackIs1`, `Align` is
//! `EncodedByteAlign`) and the unknown height (`AutoDetectHeight`): it hands out one packed row
//! at a time, 1 bit per pixel, MSB first, each row padded to a byte with 0 bits.
//!
//! Go's `reader.Read` fills the whole of the buffer it is given, so its `Read` calls end where
//! `bytes.Buffer.ReadFrom` ends them: the capacity of the result is the read-from model over one
//! segment (`copy`), with the end carried unless the last read was filled exactly.

use super::ccitt_tables::{BLACK_CODES, MODE_CODES, WHITE_CODES};
use super::{Decoded, Filter, Out};
use crate::error::{Error, Sentinel};
use crate::gocap;
use crate::ledger::{Held, Mem};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// `maxWidth`: the widest row this decoder takes (inclusive).
const MAX_WIDTH: usize = 1 << 20;

/// A binary tree per code list: element 0 is invalid, the root is element 1. A positive entry
/// is a branch, a negative one is a leaf holding the complement of the value, 0 an invalid code.
struct Tables {
    mode: Vec<[i16; 2]>,
    white: Vec<[i16; 2]>,
    black: Vec<[i16; 2]>,
}

fn build(codes: &[(u16, &str)]) -> Vec<[i16; 2]> {
    let mut t = vec![[0i16; 2]; 2];
    for &(val, s) in codes {
        let mut node = 1usize;
        let n = s.len();
        for (i, c) in s.bytes().enumerate() {
            let b = usize::from(c == b'1');
            if i + 1 == n {
                t[node][b] = !(val as i16);
            } else {
                if t[node][b] <= 0 {
                    t.push([0, 0]);
                    t[node][b] = (t.len() - 1) as i16;
                }
                node = t[node][b] as usize;
            }
        }
    }
    t
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| Tables {
        mode: build(MODE_CODES),
        white: build(WHITE_CODES),
        black: build(BLACK_CODES),
    })
}

/// `bitReader` over a byte slice. The high `n` bits of `bits` are the upcoming bits.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u64,
    n: u32,
}

impl Bits<'_> {
    /// `nextBit`; `None` at the end of the data (Go's `io.EOF`).
    fn next_bit(&mut self) -> Option<u64> {
        if self.n == 0 {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            self.bits = u64::from(b) << 56;
            self.n = 8;
        }
        let bit = self.bits >> 63;
        self.bits <<= 1;
        self.n -= 1;
        Some(bit)
    }

    /// `alignToByteBoundary`.
    fn align(&mut self) {
        let n = self.n & 7;
        self.bits <<= n;
        self.n -= n;
    }

    /// `decode`: one code of `table`. An invalid code is fatal, so its bits are not unread.
    fn decode(&mut self, table: &[[i16; 2]]) -> Result<u32, Error> {
        let mut state = 1i32;
        loop {
            let Some(bit) = self.next_bit() else {
                return Err(Error::Ccitt("incomplete-code"));
            };
            let next = table
                .get(state as usize)
                .map_or(0, |e| e[usize::from(bit == 1)]);
            state = i32::from(next);
            if state < 0 {
                return Ok(!state as u32);
            } else if state == 0 {
                return Err(Error::Ccitt("invalid-code"));
            }
        }
    }

    /// `decodeEOL`: the 12-bit EOL code 0000_0000_0001. `false` (Go's `errMissingEOL`) puts the
    /// bits back.
    fn decode_eol(&mut self) -> bool {
        let (mut n_read, mut bits_read) = (0u32, 0u64);
        loop {
            let Some(bit) = self.next_bit() else {
                return false;
            };
            bits_read |= bit << (63 - n_read);
            n_read += 1;
            if n_read < 12 {
                if bit == 0 {
                    continue;
                }
            } else if bit != 0 {
                return true;
            }
            self.bits = (self.bits >> n_read) | bits_read;
            self.n += n_read;
            return false;
        }
    }
}

/// `reader.readErr`, once set: `io.EOF` or the error.
enum End {
    Eof,
    Err(Error),
}

/// `ccitt.NewReader(r, ccitt.MSB, mode, cols, rows, opts)` over a byte slice.
pub(super) struct CcittReader<'a> {
    br: Bits<'a>,
    group4: bool,
    align: bool,
    invert: bool,
    width: usize,
    /// Negative when the height is not known in advance.
    rows_remaining: i64,
    /// The current and previous rows, one byte per pixel: 0x00 black, 0xFF white.
    curr: Vec<u8>,
    prev: Vec<u8>,
    /// `len(z.prev) == len(z.curr)`: a row has been decoded.
    have_prev: bool,
    /// The packed row handed out.
    row: Vec<u8>,
    /// The three buffers' charge.
    held: Vec<Held>,
    mem: &'a Mem,
    cancel: &'a AtomicBool,
    /// The write index (the spec's a0).
    wi: usize,
    at_start_of_row: bool,
    pen_is_white: bool,
    seen_start_of_image: bool,
    truncated: bool,
    end: Option<End>,
}

impl<'a> CcittReader<'a> {
    /// A width over `MAX_WIDTH` makes every read fail with `errUnsupportedWidth`, as in Go.
    pub(super) fn new(
        data: &'a [u8],
        group4: bool,
        width: i64,
        rows: i64,
        align: bool,
        invert: bool,
        mem: &'a Mem,
        cancel: &'a AtomicBool,
    ) -> CcittReader<'a> {
        let too_wide = width > MAX_WIDTH as i64;
        CcittReader {
            br: Bits {
                data,
                pos: 0,
                bits: 0,
                n: 0,
            },
            group4,
            align,
            invert,
            width: if too_wide {
                0
            } else {
                usize::try_from(width).unwrap_or(0)
            },
            rows_remaining: rows,
            curr: Vec::new(),
            prev: Vec::new(),
            have_prev: false,
            row: Vec::new(),
            held: Vec::new(),
            mem,
            cancel,
            wi: 0,
            at_start_of_row: true,
            pen_is_white: true,
            seen_start_of_image: false,
            truncated: false,
            end: too_wide.then_some(End::Err(Error::Ccitt("unsupported-width"))),
        }
    }

    /// The next packed row, `Ok(None)` at the clean end (Go's `io.EOF`), or the error. The end
    /// and the error are sticky (`reader.readErr`).
    pub(super) fn next_row(&mut self) -> Result<Option<&[u8]>, Error> {
        match &self.end {
            Some(End::Eof) => return Ok(None),
            Some(End::Err(e)) => return Err(e.clone()),
            None => {}
        }
        match self.advance() {
            Ok(true) => Ok(Some(&self.row)),
            Ok(false) => {
                self.end = Some(End::Eof);
                Ok(None)
            }
            Err(e) => {
                self.end = Some(End::Err(e.clone()));
                Err(e)
            }
        }
    }

    /// One pass of `reader.Read`'s loop at the start of a row: `Ok(true)` if a row was decoded
    /// and packed into `row`, `Ok(false)` at the end.
    fn advance(&mut self) -> Result<bool, Error> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !self.seen_start_of_image {
            // `startDecode`.
            if !self.group4 && !self.br.decode_eol() {
                return Err(Error::Ccitt("missing-eol"));
            }
            self.seen_start_of_image = true;
            self.alloc_rows()?;
        }
        if self.rows_remaining < 0 {
            // The height is not known: an EOL here ends the image, anything else is a row.
            if self.align && self.group4 {
                self.br.align();
            }
            if self.br.decode_eol() {
                self.finish_decode(true)?;
                return Ok(false);
            }
        } else if self.rows_remaining == 0 {
            self.finish_decode(false)?;
            return Ok(false);
        } else {
            self.rows_remaining -= 1;
        }
        self.decode_row(self.rows_remaining == 0)?;
        self.pack();
        std::mem::swap(&mut self.curr, &mut self.prev);
        self.have_prev = true;
        Ok(true)
    }

    /// The two pixel rows and the packed row, charged before they are allocated.
    fn alloc_rows(&mut self) -> Result<(), Error> {
        let (curr, h1) = self.mem.alloc_zeroed(self.width)?;
        let (prev, h2) = self.mem.alloc_zeroed(self.width)?;
        let (row, h3) = self.mem.alloc_zeroed(self.width.div_ceil(8))?;
        self.curr = curr;
        self.prev = prev;
        self.row = row;
        self.held = vec![h1, h2, h3];
        Ok(())
    }

    /// `highBits` and `invertBytes` for one row: the high bit of each pixel byte, 8 to a byte,
    /// with the last byte padded with 0 bits whether or not the row is inverted.
    fn pack(&mut self) {
        for (dst, src) in self.row.iter_mut().zip(self.curr.chunks(8)) {
            let mut v = if self.invert && src.len() < 8 {
                0xFFu8 >> src.len()
            } else {
                0
            };
            for (k, &px) in src.iter().enumerate() {
                v |= (px & 0x80) >> k;
            }
            *dst = if self.invert { !v } else { v };
        }
    }

    fn pen_color(&self) -> u8 {
        if self.pen_is_white { 0xFF } else { 0x00 }
    }

    fn fill(&mut self, from: usize, to: usize) {
        let c = self.pen_color();
        if let Some(s) = self.curr.get_mut(from..to) {
            s.fill(c);
        }
    }

    /// `finishDecode`.
    fn finish_decode(&mut self, already_seen_eol: bool) -> Result<(), Error> {
        let mut n_eols: u32;
        if self.group4 {
            let auto_detect_height = self.rows_remaining < 0;
            if !auto_detect_height && self.align {
                self.br.align();
            }
            // The stream ends with two EOLs. If the first one is missing, and the height is
            // known, the trailing two are taken to be truncated.
            if !self.br.decode_eol() {
                if !auto_detect_height {
                    self.truncated = true;
                    return Ok(());
                }
                return Err(Error::Ccitt("missing-eol"));
            }
            n_eols = 1;
        } else {
            if self.truncated {
                return Ok(());
            }
            // A return to control of 6 EOLs, of which one has been seen already.
            n_eols = 5;
        }
        if already_seen_eol {
            n_eols -= 1;
        }
        for _ in 0..n_eols {
            if !self.br.decode_eol() {
                return Err(Error::Ccitt("missing-eol"));
            }
        }
        Ok(())
    }

    /// `decodeRow`.
    fn decode_row(&mut self, final_row: bool) -> Result<(), Error> {
        self.wi = 0;
        self.at_start_of_row = true;
        self.pen_is_white = true;
        if self.align {
            self.br.align();
        }
        if !self.group4 {
            while self.wi < self.width {
                self.check_cancel()?;
                self.decode_run()?;
                self.at_start_of_row = false;
            }
            if !self.br.decode_eol() {
                // The last row of a truncated stream may lack its EOL.
                if final_row {
                    self.truncated = true;
                    return Ok(());
                }
                return Err(Error::Ccitt("missing-eol"));
            }
            return Ok(());
        }
        while self.wi < self.width {
            self.check_cancel()?;
            let mode = self.br.decode(&tables().mode)?;
            match mode {
                0 => self.mode_pass()?,
                1 => {
                    // Horizontal: a1, then a2.
                    self.decode_run()?;
                    self.decode_run()?;
                }
                2..=8 => {
                    let arg = [0isize, 1, 2, 3, -1, -2, -3][mode as usize - 2];
                    self.mode_vertical(arg)?;
                }
                9 => return Err(Error::Ccitt("unsupported-mode")),
                _ => return Err(Error::Ccitt("invalid-mode")),
            }
            self.at_start_of_row = false;
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// `decodeRun`.
    fn decode_run(&mut self) -> Result<(), Error> {
        let t = tables();
        let table = if self.pen_is_white {
            &t.white
        } else {
            &t.black
        };
        let mut total = 0usize;
        loop {
            let n = self.br.decode(table)? as usize;
            total += n;
            if total > MAX_WIDTH {
                return Err(Error::Ccitt("run-length-too-long"));
            }
            // Anything 0x3F or below is a terminal code.
            if n <= 0x3F {
                break;
            }
        }
        if total > self.width - self.wi {
            return Err(Error::Ccitt("run-length-overflows-width"));
        }
        self.fill(self.wi, self.wi + total);
        self.wi += total;
        self.pen_is_white = !self.pen_is_white;
        Ok(())
    }

    /// `findB`: b1 (or b2, if `second`): the changing elements of the reference line to the right
    /// of a0.
    fn find_b(&self, second: bool) -> usize {
        // The first row's reference line is all white: no changing element.
        if !self.have_prev {
            return self.width;
        }
        let prev = &self.prev;
        let mut i = self.wi;
        if self.at_start_of_row {
            // a0 is implicitly at -1, on a white pixel: b1 is the first black pixel of the
            // previous row, b2 the first white one after that.
            while i < prev.len() && prev[i] == 0xFF {
                i += 1;
            }
            if second {
                while i < prev.len() && prev[i] == 0x00 {
                    i += 1;
                }
            }
            return i;
        }
        let pen = self.pen_color();
        let opposite = !pen;
        while i < prev.len() && prev[i] == opposite {
            i += 1;
        }
        while i < prev.len() && prev[i] == pen {
            i += 1;
        }
        if second {
            while i < prev.len() && prev[i] == opposite {
                i += 1;
            }
        }
        i
    }

    /// `readerModePass`.
    fn mode_pass(&mut self) -> Result<(), Error> {
        let b2 = self.find_b(true);
        if !(self.wi..=self.width).contains(&b2) {
            return Err(Error::Ccitt("invalid-offset"));
        }
        self.fill(self.wi, b2);
        self.wi = b2;
        Ok(())
    }

    /// `readerModeV`.
    fn mode_vertical(&mut self, arg: isize) -> Result<(), Error> {
        let a1 = self.find_b(false) as isize + arg;
        if !(self.wi as isize..=self.width as isize).contains(&a1) {
            return Err(Error::Ccitt("invalid-offset"));
        }
        self.fill(self.wi, a1 as usize);
        self.wi = a1 as usize;
        self.pen_is_white = !self.pen_is_white;
        Ok(())
    }
}

/// `ccittDecode.DecodeLength`.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    // <0: Group 4. =0: Group 3, 1-D. >0: Group 3, 2-D, which is not supported.
    let k = f.parms.get(b"K".as_slice()).copied().unwrap_or(0);
    if k > 0 {
        return Err(Error::Ccitt("k-unsupported"));
    }
    let cols = f.parms.get(b"Columns".as_slice()).copied().unwrap_or(1728);
    if cols <= 0 {
        return Err(Error::Ccitt("columns"));
    }
    // An unknown height requires decoding through the end of the data.
    let rows = match f.parms.get(b"Rows".as_slice()) {
        Some(&r) if r > 0 => r,
        _ => -1,
    };
    let black_is_1 = f.parms.get(b"BlackIs1".as_slice()) == Some(&1);
    let align = f.parms.get(b"EncodedByteAlign".as_slice()) == Some(&1);
    let mut rd = CcittReader::new(input, k < 0, cols, rows, align, black_is_1, mem, cancel);
    copy(f, &mut rd, max_len, mem, cancel)
}

/// `baseFilter.copyDecoded` over the reader. Rows are pulled only while the copy still wants
/// bytes, as `io.LimitedReader` stops asking the reader.
fn copy(
    f: &Filter,
    rd: &mut CcittReader<'_>,
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    f.check_decode_length(max_len)?;
    let prefix = max_len >= 0;
    let limit = f.decode_limit(max_len);
    let want: Option<u64> = if prefix {
        Some(max_len.unsigned_abs())
    } else if limit < 0 || limit == i64::MAX {
        None
    } else {
        Some(limit.unsigned_abs() + 1)
    };
    let mut out = Out::new(mem)?;
    let mut total: u64 = 0;
    // The reader's end: `None` is the clean end.
    let mut stop: Option<Error> = None;
    while want.is_none_or(|n| total < n) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let row = match rd.next_row() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(e) => {
                stop = Some(e);
                break;
            }
        };
        let take = match want {
            Some(n) => row
                .len()
                .min(usize::try_from(n - total).unwrap_or(usize::MAX)),
            None => row.len(),
        };
        out.extend(&row[..take])?;
        total += take as u64;
    }
    let len = out.len();
    if prefix {
        // CopyN: no error once n bytes are there; EOF, or the reader's error, if it stopped short.
        if total == max_len.unsigned_abs() {
            return Ok(out.finish(len, false));
        }
        return Err(stop.unwrap_or(Error::Sentinel(Sentinel::Eof)));
    }
    if want.is_some() && total > limit.unsigned_abs() {
        return Err(Error::DecodeLimit);
    }
    if let Some(e) = stop {
        return Err(e);
    }
    // One segment: every `Read` fills what `ReadFrom` offers. The end rides with the last read
    // unless that read filled its buffer exactly, in which case a further, empty read has it.
    let seg = Some(len).filter(|&l| l > 0);
    let mut cap = gocap::simulate_read_from(seg, true)?;
    if cap.len() == cap.cap() {
        cap = gocap::simulate_read_from(seg, false)?;
    }
    Ok(out.finish(cap.cap(), false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::mem;
    use crate::filter::{CCITT_FAX, Parms, new_filter};

    /// Two all-white 8-pixel Group 4 rows followed by EOFB (ccittTwoWhiteRows).
    const TWO_WHITE_ROWS: [u8; 4] = [0xC0, 0x04, 0x00, 0x40];

    fn parms(list: &[(&str, i64)]) -> Parms {
        list.iter()
            .map(|&(k, v)| (k.as_bytes().to_vec(), v))
            .collect()
    }

    fn decode(
        p: &[(&str, i64)],
        data: &[u8],
        limit: Option<i64>,
        max_len: i64,
    ) -> Result<Decoded, Error> {
        let cancel = AtomicBool::new(false);
        let f = new_filter(CCITT_FAX, parms(p), limit).unwrap();
        f.decode_length(data, max_len, &mem(), &cancel)
    }

    #[test]
    fn rows_without_known_height() {
        // TestCCITTRowsWithoutKnownHeight: omitted, zero and negative Rows decode to the end.
        for p in [
            vec![("K", -1), ("Columns", 8)],
            vec![("K", -1), ("Columns", 8), ("Rows", 0)],
            vec![("K", -1), ("Columns", 8), ("Rows", -1)],
        ] {
            let d = decode(&p, &TWO_WHITE_ROWS, None, -1).unwrap();
            assert_eq!(d.as_slice(), &[0xFF, 0xFF][..], "{p:?}");
            assert!(!d.is_nil());
        }
    }

    #[test]
    fn rejects_nonpositive_columns() {
        // TestCCITTRejectsNonpositiveColumns.
        for p in [
            vec![("K", -1), ("Columns", 0)],
            vec![("K", -1), ("Columns", 0), ("Rows", 1 << 30)],
            vec![("K", -1), ("Columns", -1)],
        ] {
            let e = decode(&p, &[], None, -1).unwrap_err();
            assert_eq!(e, Error::Ccitt("columns"), "{p:?}");
        }
    }

    #[test]
    fn k_over_zero_is_unsupported() {
        let e = decode(&[("K", 1), ("Columns", 8)], &[], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("k-unsupported"));
    }

    #[test]
    fn decode_length_honors_the_limit() {
        // TestDecodeLengthHonorsConfiguredLimit for CCITT: limit 1, prefixes 2 and 1.
        let p = [("K", -1), ("Columns", 8), ("Rows", 2)];
        let e = decode(&p, &TWO_WHITE_ROWS, Some(1), 2).unwrap_err();
        assert_eq!(e, Error::DecodeLimit);
        let d = decode(&p, &TWO_WHITE_ROWS, Some(1), 1).unwrap();
        assert_eq!(d.as_slice(), &[0xFF][..]);
        // No prefix: two bytes over a limit of one.
        let e = decode(&p, &TWO_WHITE_ROWS, Some(1), -1).unwrap_err();
        assert_eq!(e, Error::DecodeLimit);
        let d = decode(&p, &TWO_WHITE_ROWS, Some(2), -1).unwrap();
        assert_eq!(d.as_slice(), &[0xFF, 0xFF][..]);
    }

    #[test]
    fn prefix_longer_than_the_image_is_eof() {
        // CopyN of more than the reader has: io.EOF.
        let p = [("K", -1), ("Columns", 8), ("Rows", 2)];
        let e = decode(&p, &TWO_WHITE_ROWS, None, 3).unwrap_err();
        assert_eq!(e, Error::Sentinel(Sentinel::Eof));
    }

    #[test]
    fn go_capacity_of_the_result() {
        // Two bytes: the read that ends the data is not full, so the end rides with it and
        // ReadFrom stops after its first grow(512).
        let p = [("K", -1), ("Columns", 8), ("Rows", 2)];
        let d = decode(&p, &TWO_WHITE_ROWS, None, -1).unwrap();
        assert_eq!((d.len(), d.go_cap()), (2, 512));
    }

    const EOL: &str = "000000000001";

    /// Packs a string of 0s and 1s MSB first, the last byte padded with 0 bits.
    fn pack_bits(bits: &str) -> Vec<u8> {
        let mut data = Vec::new();
        let (mut cur, mut n) = (0u8, 0);
        for c in bits.chars() {
            cur = (cur << 1) | u8::from(c == '1');
            n += 1;
            if n == 8 {
                data.push(cur);
                (cur, n) = (0, 0);
            }
        }
        if n > 0 {
            data.push(cur << (8 - n));
        }
        data
    }

    #[test]
    fn exactly_full_last_read_is_followed_by_an_empty_read() {
        // 512 rows of one byte (V0 each) fill the first read exactly, so a second grow and an
        // empty read follow: the capacity doubles to 1024. Then EOFB.
        let data = pack_bits(&format!("{}{EOL}{EOL}", "1".repeat(512)));
        let p = [("K", -1), ("Columns", 8)];
        let d = decode(&p, &data, None, -1).unwrap();
        assert_eq!((d.len(), d.go_cap()), (512, 1024));
    }

    #[test]
    fn black_is_1_inverts_and_pads_with_zero_bits() {
        // Two white rows of 4 pixels, Group 4: V0 V0 then EOFB.
        let p = [("K", -1), ("Columns", 4), ("Rows", 2)];
        let d = decode(&p, &TWO_WHITE_ROWS, None, -1).unwrap();
        assert_eq!(d.as_slice(), &[0xF0, 0xF0][..]);
        let p = [("K", -1), ("Columns", 4), ("Rows", 2), ("BlackIs1", 1)];
        let d = decode(&p, &TWO_WHITE_ROWS, None, -1).unwrap();
        assert_eq!(d.as_slice(), &[0x00, 0x00][..]);
    }

    #[test]
    fn group4_black_run_through_horizontal_mode() {
        // One row of 8: H (001), white run 2 (0111), black run 3 (10), then V0 (1): a0 = 5 on a
        // white pen, a1 = b1 = the width because there is no previous row. The two trailing
        // EOLs are missing, which a known height tolerates.
        let data = pack_bits("0010111101");
        let p = [("K", -1), ("Columns", 8), ("Rows", 1)];
        let d = decode(&p, &data, None, -1).unwrap();
        // 2 white, 3 black, 3 white.
        assert_eq!(d.as_slice(), &[0b1100_0111][..]);
    }

    #[test]
    fn group3_one_row() {
        // K = 0, one row of a white run of 8 (10011). Without the row's own EOL the last row is
        // taken as truncated.
        let p = [("K", 0), ("Columns", 8), ("Rows", 1)];
        let data = pack_bits(&format!("{EOL}10011"));
        let d = decode(&p, &data, None, -1).unwrap();
        assert_eq!(d.as_slice(), &[0xFF][..]);
        // With it, the return to control of six EOLs must follow.
        let data = pack_bits(&format!("{EOL}10011{}", EOL.repeat(6)));
        let d = decode(&p, &data, None, -1).unwrap();
        assert_eq!(d.as_slice(), &[0xFF][..]);
        let data = pack_bits(&format!("{EOL}10011{EOL}"));
        let e = decode(&p, &data, None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("missing-eol"));
    }

    #[test]
    fn group3_without_leading_eol_fails() {
        let p = [("K", 0), ("Columns", 8), ("Rows", 1)];
        let e = decode(&p, &[0xFF, 0xFF], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("missing-eol"));
    }

    #[test]
    fn invalid_and_truncated_codes() {
        // Group 4 mode code 0000000 is the extension: unsupported.
        let p = [("K", -1), ("Columns", 8), ("Rows", 1)];
        let e = decode(&p, &[0x02], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("unsupported-mode"));
        // No data at all.
        let e = decode(&p, &[], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("incomplete-code"));
        // 0000000 0 is not a code.
        let e = decode(&p, &[0x00], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("invalid-code"));
    }

    #[test]
    fn too_wide_is_an_error_on_the_first_read() {
        let p = [("K", -1), ("Columns", (1 << 20) + 1)];
        let e = decode(&p, &[], None, -1).unwrap_err();
        assert_eq!(e, Error::Ccitt("unsupported-width"));
        // With a zero-length prefix the reader is never asked.
        let d = decode(&p, &[], None, 0).unwrap();
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn cancel_stops_decoding() {
        let cancel = AtomicBool::new(true);
        let f = new_filter(CCITT_FAX, parms(&[("K", -1), ("Columns", 8)]), None).unwrap();
        let e = f
            .decode_length(&TWO_WHITE_ROWS, -1, &mem(), &cancel)
            .unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    #[test]
    fn code_tables_are_prefix_free_and_complete_for_runs() {
        let t = tables();
        // Every code of every list decodes to its value.
        for (codes, table) in [
            (MODE_CODES, &t.mode),
            (WHITE_CODES, &t.white),
            (BLACK_CODES, &t.black),
        ] {
            for &(val, s) in codes {
                let mut state = 1i32;
                for (i, c) in s.bytes().enumerate() {
                    state = i32::from(table[state as usize][usize::from(c == b'1')]);
                    if i + 1 == s.len() {
                        assert_eq!(state, !i32::from(val as i16), "{s}");
                    } else {
                        assert!(state > 0, "{s}");
                    }
                }
            }
        }
    }
}
