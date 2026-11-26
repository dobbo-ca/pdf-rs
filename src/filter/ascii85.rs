//! ASCII85Decode (Go: pkg/filter/ascii85Decode.go, and the decoder of encoding/ascii85). Decode
//! only.
//!
//! Go cuts the input at the first `~>`, wraps the rest in `ascii85.NewDecoder` and copies it
//! with `copyDecoded`. The decoder hands out what one `Decode` call produced (at most 1024
//! bytes) and then a separate end, so its capacity is the read-from model over those batches
//! with the end not carried. The batches depend on how the decoder's 1024-byte input buffer is
//! filled, so that buffer is ported as it is.

use super::{Decoded, Filter, Source};
use crate::error::Error;
use crate::ledger::Mem;
use std::sync::atomic::AtomicBool;

const EOD: &[u8] = b"~>";
const BUF: usize = 1024;

/// `ascii85.Decode`: decodes whole 4-byte groups of `src` into `dst`. It returns the bytes
/// written and the bytes consumed. With `flush` the last short group is decoded too. A bad byte
/// is `Err` with its offset in `src` (`CorruptInputError`).
fn decode(dst: &mut [u8], src: &[u8], flush: bool) -> Result<(usize, usize), i64> {
    let mut v: u32 = 0;
    let mut nb = 0;
    let (mut ndst, mut nsrc) = (0usize, 0usize);
    for (i, &b) in src.iter().enumerate() {
        if dst.len() - ndst < 4 {
            return Ok((ndst, nsrc));
        }
        if b <= b' ' {
            continue;
        } else if b == b'z' && nb == 0 {
            nb = 5;
            v = 0;
        } else if (b'!'..=b'u').contains(&b) {
            v = v.wrapping_mul(85).wrapping_add(u32::from(b - b'!'));
            nb += 1;
        } else {
            return Err(i as i64);
        }
        if nb == 5 {
            nsrc = i + 1;
            dst[ndst..ndst + 4].copy_from_slice(&v.to_be_bytes());
            ndst += 4;
            nb = 0;
            v = 0;
        }
    }
    if flush {
        nsrc = src.len();
        if nb > 0 {
            // The number of output bytes in the last fragment is the number of leftover input
            // bytes - 1: the extra byte provides enough bits to cover the inefficiency of the
            // encoding for the block.
            if nb == 1 {
                return Err(src.len() as i64);
            }
            for _ in nb..5 {
                // The short encoding truncated the output value; assume the worst case value
                // (digit 84) so that the top bits are correct.
                v = v.wrapping_mul(85).wrapping_add(84);
            }
            for _ in 0..nb - 1 {
                dst[ndst] = (v >> 24) as u8;
                v <<= 8;
                ndst += 1;
            }
        }
    }
    Ok((ndst, nsrc))
}

/// `ascii85.NewDecoder` over a byte slice (Go's `bytes.Reader`), with Go's buffers.
pub struct A85Source<'a> {
    src: &'a [u8],
    pos: usize,
    /// `d.err`: set once the decoder is finished, `None` inside means a clean end.
    fin: Option<Option<i64>>,
    /// `d.readErr != nil`: the reader has reported EOF.
    read_eof: bool,
    buf: Vec<u8>,
    nbuf: usize,
    outbuf: Vec<u8>,
}

impl<'a> A85Source<'a> {
    pub fn new(src: &'a [u8]) -> A85Source<'a> {
        A85Source {
            src,
            pos: 0,
            fin: None,
            read_eof: false,
            buf: vec![0; BUF],
            nbuf: 0,
            outbuf: vec![0; BUF],
        }
    }
}

/// The end of the decoder: a clean end, or `CorruptInputError(off)`.
fn end(fin: Option<i64>) -> Result<Option<&'static [u8]>, Error> {
    match fin {
        None => Ok(None),
        Some(off) => Err(Error::Ascii85(off)),
    }
}

impl Source for A85Source<'_> {
    fn next(&mut self) -> Result<Option<&[u8]>, Error> {
        if let Some(f) = self.fin {
            return end(f);
        }
        loop {
            if self.nbuf > 0 {
                match decode(&mut self.outbuf, &self.buf[..self.nbuf], self.read_eof) {
                    Ok((ndst, nsrc)) => {
                        if ndst > 0 {
                            self.buf.copy_within(nsrc..self.nbuf, 0);
                            self.nbuf -= nsrc;
                            return Ok(Some(&self.outbuf[..ndst]));
                        }
                        // Special case: the input buffer is mostly filled with non-data bytes.
                        // Filter out such bytes to make room for more input.
                        let mut off = 0;
                        for i in 0..self.nbuf {
                            if self.buf[i] > b' ' {
                                self.buf[off] = self.buf[i];
                                off += 1;
                            }
                        }
                        self.nbuf = off;
                    }
                    Err(off) => {
                        self.fin = Some(Some(off));
                        return end(Some(off));
                    }
                }
            }
            // Out of input, out of decoded output. Check errors.
            if self.read_eof {
                self.fin = Some(None);
                return Ok(None);
            }
            // Read more data.
            if self.pos >= self.src.len() {
                self.read_eof = true;
            } else {
                let n = (BUF - self.nbuf).min(self.src.len() - self.pos);
                self.buf[self.nbuf..self.nbuf + n]
                    .copy_from_slice(&self.src[self.pos..self.pos + n]);
                self.pos += n;
                self.nbuf += n;
            }
        }
    }

    fn end_carried(&self) -> bool {
        false
    }
}

/// `DecodeLength`: the data up to the first `~>`.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    let Some(i) = input.windows(EOD.len()).position(|w| w == EOD) else {
        return Err(Error::Ascii85Eod);
    };
    let mut src = A85Source::new(&input[..i]);
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
    use crate::filter::{ASCII85, Parms, new_filter};

    /// `ascii85.Encode` for a short group, enough for the tests.
    fn encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in data.chunks(4) {
            let mut v = 0u32;
            for i in 0..4 {
                v = (v << 8) | u32::from(*chunk.get(i).unwrap_or(&0));
            }
            if chunk.len() == 4 && v == 0 {
                out.push(b'z');
                continue;
            }
            let mut digits = [0u8; 5];
            for d in digits.iter_mut().rev() {
                *d = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&digits[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }

    fn dec(input: &[u8], max_len: i64, limit: Option<i64>) -> Result<Decoded, Error> {
        let f = new_filter(ASCII85, Parms::new(), limit).unwrap();
        f.decode_length(input, max_len, &mem(), &AtomicBool::new(false))
    }

    #[test]
    fn eod_handling() {
        // TestASCII85DecodeEODHandling: nothing after the marker matters.
        let enc = encode(b"Hello, Gopher!");
        let endings: [&[u8]; 6] = [b"\n", b"\r", b"\r\n", b" ", b"\xA0", b""];
        for ending in endings {
            let mut input = enc.clone();
            input.extend_from_slice(ending);
            assert_eq!(dec(&input, -1, None).unwrap().as_slice(), b"Hello, Gopher!");
        }
    }

    #[test]
    fn missing_eod() {
        // TestASCII85DecodeMissingEOD.
        let e = dec(b"87cURD_*#TDfTZ)+T", -1, None).err();
        assert_eq!(e, Some(Error::Ascii85Eod));
        assert_eq!(e.map(|e| e.class()), Some("ascii85/eod"));
        // Go's test of the filter errors: an empty input has no marker either.
        assert_eq!(dec(b"", -1, None).err(), Some(Error::Ascii85Eod));
    }

    #[test]
    fn known_vectors_and_zero_group() {
        assert_eq!(
            dec(b"87cURD]i,\"Ebo80~>", -1, None).unwrap().as_slice(),
            b"Hello World!"
        );
        assert_eq!(dec(b"z~>", -1, None).unwrap().as_slice(), &[0, 0, 0, 0]);
        // Space and control bytes are ignored.
        assert_eq!(
            dec(b"87cU RD]i\n,\"Ebo80~>", -1, None).unwrap().as_slice(),
            b"Hello World!"
        );
        // Empty data before the marker.
        let d = dec(b"~>", -1, None).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (0, 512, false));
    }

    #[test]
    fn corrupt_input_offsets_are_relative_to_the_decoder_buffer() {
        assert_eq!(dec(b"87c\x80~>", -1, None).err(), Some(Error::Ascii85(3)));
        // A single leftover digit at the end: the first group is decoded and handed out, the
        // buffer then holds the digit alone, so the offset is its length.
        assert_eq!(dec(b"87cUR!~>", -1, None).err(), Some(Error::Ascii85(1)));
        assert_eq!(
            dec(b"87cUR!~>", -1, None).err().map(|e| e.class()),
            Some("ascii85/corrupt")
        );
    }

    #[test]
    fn round_trip_all_sizes_and_capacity() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + i / 13) as u8).collect();
        for n in [0usize, 1, 2, 3, 4, 5, 100, 1019, 1020, 1021, 1024, 5000] {
            let d = dec(&encode(&data[..n]), -1, None).unwrap();
            assert_eq!(d.as_slice(), &data[..n], "n = {n}");
        }
        // 100 bytes are one batch; the end is a separate read, so ReadFrom grows once more.
        let d = dec(&encode(&data[..100]), -1, None).unwrap();
        assert_eq!(d.go_cap(), 1024);
        // The capacity is the read-from model over the batches of the decoder.
        let enc = encode(&data);
        let mut src = A85Source::new(&enc[..enc.len() - 2]);
        let mut sizes = Vec::new();
        while let Some(p) = src.next().unwrap() {
            sizes.push(p.len());
        }
        let want = crate::gocap::simulate_read_from(sizes, false)
            .unwrap()
            .cap();
        assert_eq!(dec(&enc, -1, None).unwrap().go_cap(), want);
    }

    #[test]
    fn batches_are_what_one_decode_call_produces() {
        let data = vec![0x55u8; 3000];
        let enc = encode(&data);
        let body = &enc[..enc.len() - 2];
        let mut src = A85Source::new(body);
        let mut sizes = Vec::new();
        while let Some(p) = src.next().unwrap() {
            sizes.push(p.len());
        }
        assert_eq!(sizes.iter().sum::<usize>(), 3000);
        assert!(sizes.iter().all(|&s| s <= BUF));
        // After the end it stays at the end.
        assert!(src.next().unwrap().is_none());
        assert!(!src.end_carried());
    }

    #[test]
    fn limits() {
        let enc = encode(b"hello");
        assert_eq!(dec(&enc, -1, Some(4)).err(), Some(Error::DecodeLimit));
        assert!(dec(&enc, -1, Some(5)).is_ok());
        // TestDecodeLengthHonorsConfiguredLimit: a prefix over the limit, a prefix at it.
        assert_eq!(dec(&enc, 5, Some(4)).err(), Some(Error::DecodeLimit));
        assert_eq!(dec(&enc, 4, Some(4)).unwrap().as_slice(), b"hell");
    }
}
