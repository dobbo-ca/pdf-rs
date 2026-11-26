//! LZWDecode (Go: pkg/filter/lzwDecode.go and internal/filter/lzw/reader.go). Decode only (the
//! LZW writer is out of scope, docs/scope.md section 6).
//!
//! The decoder is PDF's: MSB-first codes, 8-bit literals, and the `EarlyChange` switch
//! (`oneOff`), which makes the code width grow one code early. It is Go's reader as it is,
//! including its output buffer: `Read` hands out what one `decode` call flushed (at least 4096
//! bytes, or what is left at the end), and the end or the error is a separate `Read`, so the
//! capacity is the read-from model over those flushes with the end not carried.

use super::{Decoded, Filter, Source};
use crate::error::Error;
use crate::ledger::Mem;
use std::sync::atomic::AtomicBool;

const MAX_WIDTH: u32 = 12;
const LIT_WIDTH: u32 = 8;
const INVALID: u16 = 0xffff;
const CLEAR: u16 = 1 << LIT_WIDTH;
const EOF: u16 = CLEAR + 1;
const TABLE: usize = 1 << MAX_WIDTH;
const FLUSH: usize = 1 << MAX_WIDTH;

/// Why the decoder stopped: `io.EOF`, or `errors.New("lzw: invalid code")`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Eof,
    Invalid,
}

/// `lzw.NewReader(r, oneOff)` over a byte slice. Its tables are fixed-size (about 20 KiB) and not
/// charged to the ledger.
pub struct LzwSource<'a> {
    r: &'a [u8],
    pos: usize,
    bits: u32,
    n_bits: u32,
    width: u32,
    err: Option<End>,
    /// The next free code (`hi`), capped at `TABLE`; the next code width boundary (`overflow`);
    /// the code seen last (`last`).
    hi: u16,
    overflow: u16,
    last: u16,
    /// Each dictionary code `c` in `[CLEAR + 2, hi)` expands to `prefix[c]` and then
    /// `suffix[c]`.
    suffix: Vec<u8>,
    prefix: Vec<u16>,
    /// Literal codes are accumulated from the start; the expansion of a dictionary code is first
    /// written right to left from the end, then copied to the start.
    output: Vec<u8>,
    o: usize,
    /// `toRead`: `output[..to_read]` is not handed out yet.
    to_read: usize,
    one_off: bool,
}

impl<'a> LzwSource<'a> {
    pub fn new(r: &'a [u8], one_off: bool) -> LzwSource<'a> {
        let width = 1 + LIT_WIDTH;
        LzwSource {
            r,
            pos: 0,
            bits: 0,
            n_bits: 0,
            width,
            err: None,
            hi: EOF,
            overflow: 1 << width,
            last: INVALID,
            suffix: vec![0; TABLE],
            prefix: vec![0; TABLE],
            output: vec![0; 2 * TABLE],
            o: 0,
            to_read: 0,
            one_off,
        }
    }

    /// `readMSB`: the next code, or `None` at the end of the input.
    fn read_msb(&mut self) -> Option<u16> {
        while self.n_bits < self.width {
            let x = *self.r.get(self.pos)?;
            self.pos += 1;
            self.bits |= u32::from(x) << (24 - self.n_bits);
            self.n_bits += 8;
        }
        let code = (self.bits >> (32 - self.width)) as u16;
        self.bits <<= self.width;
        self.n_bits -= self.width;
        Some(code)
    }

    fn advance_code(&mut self) {
        if usize::from(self.hi) < TABLE {
            self.hi += 1;
        }
        let ui = if self.one_off { self.hi + 1 } else { self.hi };
        if ui >= self.overflow && self.width < MAX_WIDTH {
            self.width += 1;
            self.overflow <<= 1;
        }
    }

    fn save_entry(&mut self, head: u8) {
        if self.last == INVALID || usize::from(self.hi) >= TABLE {
            return;
        }
        self.suffix[usize::from(self.hi)] = head;
        self.prefix[usize::from(self.hi)] = self.last;
    }

    /// `decode`: decompresses codes into `output` until it holds at least `FLUSH` bytes or the
    /// input ends, and leaves them in `to_read`.
    fn decode(&mut self) {
        loop {
            let Some(code) = self.read_msb() else {
                // Some PDF writers write an EOD and some don't: no EOD is not an error.
                self.err = Some(End::Eof);
                break;
            };
            if code < CLEAR {
                // A literal code.
                self.output[self.o] = code as u8;
                self.o += 1;
                self.save_entry(code as u8);
            } else if code == CLEAR {
                self.width = 1 + LIT_WIDTH;
                self.hi = EOF;
                self.overflow = 1 << self.width;
                self.last = INVALID;
                continue;
            } else if code == EOF {
                self.err = Some(End::Eof);
                break;
            } else if code <= self.hi {
                let mut c = code;
                let mut i = self.output.len() - 1;
                if code == self.hi && self.last != INVALID {
                    // code == hi is a special case which expands to the last expansion followed
                    // by the head of the last expansion. To find the head, walk the prefix
                    // chain until a literal code.
                    c = self.last;
                    while c >= CLEAR {
                        c = self.prefix[usize::from(c)];
                    }
                    self.output[i] = c as u8;
                    i -= 1;
                    c = self.last;
                }
                // Copy the suffix chain into output and then to the front.
                while c >= CLEAR {
                    self.output[i] = self.suffix[usize::from(c)];
                    i -= 1;
                    c = self.prefix[usize::from(c)];
                }
                self.output[i] = c as u8;
                let n = self.output.len() - i;
                self.output.copy_within(i.., self.o);
                self.o += n;
                self.save_entry(c as u8);
            } else {
                self.err = Some(End::Invalid);
                break;
            }
            self.last = code;
            self.advance_code();
            if self.o >= FLUSH {
                break;
            }
        }
        // Flush pending output.
        self.to_read = self.o;
        self.o = 0;
    }
}

impl Source for LzwSource<'_> {
    fn next(&mut self) -> Result<Option<&[u8]>, Error> {
        loop {
            if self.to_read > 0 {
                let n = self.to_read;
                self.to_read = 0;
                return Ok(Some(&self.output[..n]));
            }
            match self.err {
                Some(End::Eof) => return Ok(None),
                Some(End::Invalid) => return Err(Error::Lzw),
                None => {}
            }
            self.decode();
        }
    }

    fn end_carried(&self) -> bool {
        false
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
    let predictor = f.parms.get(b"Predictor".as_slice()).copied().unwrap_or(1);
    if predictor > 1 {
        return Err(Error::Predictor("params")
            .context(format!("LZW decode: unsupported predictor {predictor}")));
    }
    let early_change = f.parms.get(b"EarlyChange".as_slice()).copied().unwrap_or(1);
    let mut src = LzwSource::new(input, early_change == 1);
    let c = f.copy_decoded(&mut src, max_len, mem, cancel)?;
    match c.err {
        Some(e) => Err(e),
        None => Ok(c.buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::mem;
    use crate::filter::{LZW, Parms, new_filter};

    /// `packPDFCodes`: packs codes with the widths PDF's encoders use, independently of the
    /// decoder.
    fn pack(codes: &[u16], early_change: bool) -> Vec<u8> {
        fn width(count: usize) -> u32 {
            match count {
                0..=254 => 9,
                255..=766 => 10,
                767..=1790 => 11,
                _ => 12,
            }
        }
        let adjustment = usize::from(early_change);
        let mut out = Vec::new();
        let mut bits: u32 = 0;
        let mut n_bits: u32 = 0;
        let mut count = 0usize;
        for &code in codes {
            let w = width(count + adjustment);
            bits = (bits << w) | u32::from(code);
            n_bits += w;
            while n_bits >= 8 {
                n_bits -= 8;
                out.push((bits >> n_bits) as u8);
            }
            if code == 256 {
                count = 0;
            } else if code != 257 {
                count += 1;
            }
        }
        if n_bits > 0 {
            out.push((bits << (8 - n_bits)) as u8);
        }
        out
    }

    fn decode_all(input: &[u8], early_change: bool) -> Result<Vec<u8>, Error> {
        let mut src = LzwSource::new(input, early_change);
        let mut out = Vec::new();
        while let Some(p) = src.next()? {
            out.extend_from_slice(p);
        }
        Ok(out)
    }

    fn assert_decoded_codes(codes: &[u16], early_change: bool, want: &[u8]) {
        let got = decode_all(&pack(codes, early_change), early_change).unwrap();
        assert!(
            got == want,
            "decoded {} bytes, want {}",
            got.len(),
            want.len()
        );
    }

    fn full_dictionary_codes() -> (Vec<u16>, Vec<u8>) {
        // The first three literals define entry 259 as "BT". After 3837 literals and entry 259,
        // entry 4095 is the next free slot and must expand the previous "BT" to "BTB".
        let mut want = b"%BT".to_vec();
        want.extend(std::iter::repeat_n(b' ', 3832));
        want.extend_from_slice(b"\n%");
        let mut codes = vec![256u16];
        codes.extend(want.iter().map(|&b| u16::from(b)));
        codes.extend_from_slice(&[259, 4095]);
        want.extend_from_slice(b"BTBTB");
        (codes, want)
    }

    #[test]
    fn reader_full_dictionary() {
        // TestReaderFullDictionary.
        let (_, expected) = full_dictionary_codes();
        assert!(expected.len() == 3842 && expected.ends_with(b" \n%BTBTB"));
        for early_change in [false, true] {
            for continuation in ["end", "literals", "dictionary", "clear"] {
                let (mut codes, mut want) = full_dictionary_codes();
                match continuation {
                    "literals" => {
                        codes.extend(std::iter::repeat_n(u16::from(b'!'), 70000));
                        want.extend(std::iter::repeat_n(b'!', 70000));
                    }
                    "dictionary" => {
                        codes.extend(std::iter::repeat_n(4095u16, 70000));
                        for _ in 0..70000 {
                            want.extend_from_slice(b"BTB");
                        }
                    }
                    "clear" => {
                        codes.extend_from_slice(&[256, u16::from(b'A'), 258]);
                        want.extend_from_slice(b"AAA");
                    }
                    _ => {}
                }
                codes.push(257);
                assert_decoded_codes(&codes, early_change, &want);
            }
        }
    }

    #[test]
    fn reader_code_width_transitions() {
        // TestReaderCodeWidthTransitions.
        for early_change in [false, true] {
            for count in [253usize, 254, 255, 765, 766, 767, 1789, 1790, 1791] {
                let want: Vec<u8> = (0..count).map(|i| i as u8).collect();
                let mut codes = vec![256u16];
                codes.extend(want.iter().map(|&b| u16::from(b)));
                codes.push(257);
                assert_decoded_codes(&codes, early_change, &want);
            }
        }
    }

    #[test]
    fn reader_decodes_pdf_msb() {
        // TestReaderDecodesPDFMSB.
        let got = decode_all(b"\x80\x0b\x60\x50\x22\x0c\x0c\x85\x01", true).unwrap();
        assert_eq!(got, b"-----A---B");
    }

    #[test]
    fn invalid_code_and_missing_eod() {
        // A code past the next free one.
        let input = pack(&[256, 65, 300], true);
        let mut src = LzwSource::new(&input, true);
        // The flushed output comes first, then the error, then it stays an error.
        assert_eq!(src.next().unwrap(), Some(&b"A"[..]));
        assert_eq!(src.next().err(), Some(Error::Lzw));
        assert_eq!(src.next().err(), Some(Error::Lzw));
        // No EOD code: the end of the input is a clean end.
        assert_eq!(
            decode_all(&pack(&[256, 65, 66], true), true).unwrap(),
            b"AB"
        );
        // Nothing at all.
        assert_eq!(decode_all(b"", true).unwrap(), b"");
    }

    #[test]
    fn flushes_are_at_least_4096_bytes() {
        let want: Vec<u8> = (0..10_000usize).map(|i| (i % 251) as u8).collect();
        let mut codes = vec![256u16];
        // Restart the dictionary often enough that no code is wider than 12 bits.
        for (i, &b) in want.iter().enumerate() {
            if i > 0 && i.is_multiple_of(3000) {
                codes.push(256);
            }
            codes.push(u16::from(b));
        }
        codes.push(257);
        let input = pack(&codes, true);
        let mut src = LzwSource::new(&input, true);
        let mut sizes = Vec::new();
        while let Some(p) = src.next().unwrap() {
            sizes.push(p.len());
        }
        assert_eq!(sizes, [4096, 4096, 1808]);
    }

    fn dec(
        input: &[u8],
        max_len: i64,
        limit: Option<i64>,
        parms: &[(&str, i64)],
    ) -> Result<Decoded, Error> {
        let p: Parms = parms
            .iter()
            .map(|&(k, v)| (k.as_bytes().to_vec(), v))
            .collect();
        let f = new_filter(LZW, p, limit).unwrap();
        f.decode_length(input, max_len, &mem(), &AtomicBool::new(false))
    }

    #[test]
    fn filter_parameters() {
        let want = b"hello hello";
        let mut codes = vec![256u16];
        codes.extend(want.iter().map(|&b| u16::from(b)));
        codes.push(257);
        let input = pack(&codes, true);
        assert_eq!(dec(&input, -1, None, &[]).unwrap().as_slice(), want);
        assert_eq!(
            dec(&input, -1, None, &[("EarlyChange", 1), ("Predictor", 1)])
                .unwrap()
                .as_slice(),
            want
        );
        // A predictor is not supported.
        let e = dec(&input, -1, None, &[("Predictor", 2)]).err().unwrap();
        assert_eq!(e.class(), "predictor/params");
        assert!(e.to_string().contains("LZW decode"));
    }

    #[test]
    fn early_change_zero_differs_after_the_first_boundary() {
        // 300 literals cross the 9 to 10 bit boundary, which EarlyChange moves by one code.
        let want: Vec<u8> = (0..300usize).map(|i| i as u8).collect();
        let mut codes = vec![256u16];
        codes.extend(want.iter().map(|&b| u16::from(b)));
        codes.push(257);
        let late = pack(&codes, false);
        let early = pack(&codes, true);
        assert_ne!(late, early);
        assert_eq!(
            dec(&late, -1, None, &[("EarlyChange", 0)])
                .unwrap()
                .as_slice(),
            &want[..]
        );
        assert_eq!(dec(&early, -1, None, &[]).unwrap().as_slice(), &want[..]);
    }

    #[test]
    fn capacity_ends_with_a_separate_read() {
        // 11 bytes in one flush: 512, then the read of the end grows to 1024.
        let mut codes = vec![256u16];
        codes.extend(b"hello hello".iter().map(|&b| u16::from(b)));
        codes.push(257);
        let d = dec(&pack(&codes, true), -1, None, &[]).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (11, 1024, false));
    }

    #[test]
    fn limits() {
        // TestDecodeLengthHonorsConfiguredLimit for LZW: "hello" with limit 4.
        let mut codes = vec![256u16];
        codes.extend(b"hello".iter().map(|&b| u16::from(b)));
        codes.push(257);
        let input = pack(&codes, true);
        assert_eq!(dec(&input, 5, Some(4), &[]).err(), Some(Error::DecodeLimit));
        assert_eq!(
            dec(&input, -1, Some(4), &[]).err(),
            Some(Error::DecodeLimit)
        );
        let d = dec(&input, 4, Some(4), &[]).unwrap();
        assert_eq!(d.as_slice(), b"hell");
        assert_eq!(dec(&input, -1, Some(5), &[]).unwrap().as_slice(), b"hello");
        // A prefix longer than the data: CopyN reports EOF.
        assert_eq!(
            dec(&input, 6, None, &[]).err(),
            Some(Error::Sentinel(crate::error::Sentinel::Eof))
        );
    }
}
