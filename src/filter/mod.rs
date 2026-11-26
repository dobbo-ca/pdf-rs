//! Stream filters (Go: pkg/filter/filter.go). A filter turns a byte slice into a [`Decoded`]
//! buffer, as each of Go's filters turns an `io.Reader` into a `*bytes.Buffer`. Everything that
//! scales with the input is charged to the ledger ([`Mem`]) before it is allocated, and every
//! loop checks the cancel flag.
//!
//! [`Decoded`] carries what Go's decode budget reads: the capacity of the `bytes.Buffer` that
//! Go would have made (docs/design.md section 8). The decoders that fill it through
//! `io.Copy` into a `bytes.Buffer` (every filter except ASCIIHex and RunLength) report the
//! boundaries of Go's `Read` calls through [`Source`], and [`gocap::simulate_read_from`] turns
//! them into the capacity. RunLength and the predictor path write into the buffer as Go does and
//! use the write-path simulation.
//!
//! Only Flate has an encoder (docs/scope.md section 6); `Filter::encode` of any other filter is
//! [`Error::UnsupportedFilter`]. CCITT fills its buffer through its own copy in [`ccitt`] (its
//! `Read` calls follow the buffer, not a segmentation), and DCT writes the gob messages of
//! [`gob`] with the write-path model.

pub mod ascii85;
pub mod ascii_hex;
pub mod ccitt;
mod ccitt_tables;
pub mod dct;
pub mod flate;
pub mod gob;
pub mod goflate;
pub mod gojpeg;
pub mod lzw;
pub mod run_length;

use crate::error::{Error, Sentinel};
use crate::gocap::{self, Buffer};
use crate::ledger::{Bytes, Held, Mem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// `filter.ASCII85`.
pub const ASCII85: &[u8] = b"ASCII85Decode";
/// `filter.ASCIIHex`.
pub const ASCII_HEX: &[u8] = b"ASCIIHexDecode";
/// `filter.RunLength`.
pub const RUN_LENGTH: &[u8] = b"RunLengthDecode";
/// `filter.LZW`.
pub const LZW: &[u8] = b"LZWDecode";
/// `filter.Flate`.
pub const FLATE: &[u8] = b"FlateDecode";
/// `filter.CCITTFax`.
pub const CCITT_FAX: &[u8] = b"CCITTFaxDecode";
/// `filter.JBIG2`.
pub const JBIG2: &[u8] = b"JBIG2Decode";
/// `filter.DCT`.
pub const DCT: &[u8] = b"DCTDecode";
/// `filter.JPX`.
pub const JPX: &[u8] = b"JPXDecode";

/// `filter.DefaultMaxDecodeBytes`.
pub const DEFAULT_MAX_DECODE_BYTES: i64 = 512 << 20;

/// A filter's integer decode parameters (Go's `map[string]int`).
pub type Parms = BTreeMap<Vec<u8>, i64>;

/// `SupportsDecodeParms`: whether `name` takes decode parameters.
pub fn supports_decode_parms(name: &[u8]) -> bool {
    name == CCITT_FAX || name == LZW || name == FLATE
}

/// The `bytes.Buffer` a filter returns: its bytes, the ledger charge for them, and the
/// capacity (`go_cap`) and nil-ness of the buffer Go would have had.
#[derive(Debug)]
pub struct Decoded {
    v: Vec<u8>,
    held: Held,
    go_cap: usize,
    nil: bool,
}

impl Decoded {
    /// The decoded bytes (Go's `Buffer.Bytes`).
    pub fn as_slice(&self) -> &[u8] {
        &self.v
    }

    /// The number of decoded bytes.
    pub fn len(&self) -> usize {
        self.v.len()
    }

    /// Whether there are no decoded bytes.
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// `cap(buf.Bytes())` of Go's buffer.
    pub fn go_cap(&self) -> usize {
        self.go_cap
    }

    /// Whether `Bytes()` of Go's buffer would be a nil slice (nothing was ever written to it).
    /// A stream whose decoded content is nil counts as not decoded yet.
    pub fn is_nil(&self) -> bool {
        self.nil
    }

    /// The decoded bytes as shared, charged [`Bytes`] with Go's capacity.
    pub fn into_bytes(self) -> Bytes {
        Bytes::new(self.v, self.go_cap, Some(self.held))
    }
}

/// A buffer that grows as a filter writes, charging its capacity to the ledger before each
/// growth. The charge is the capacity, not the length.
pub(crate) struct Out {
    v: Vec<u8>,
    held: Held,
}

impl Out {
    pub(crate) fn new(mem: &Mem) -> Result<Out, Error> {
        Ok(Out {
            v: Vec::new(),
            held: mem.charge(0)?,
        })
    }

    /// `make([]byte, n)`: `n` zero bytes, charged first.
    pub(crate) fn zeroed(mem: &Mem, n: usize) -> Result<Out, Error> {
        let (v, held) = mem.alloc_zeroed(n)?;
        Ok(Out { v, held })
    }

    fn grow_for(&mut self, extra: usize) -> Result<(), Error> {
        let len = self.v.len();
        let need = len.checked_add(extra).ok_or(Error::OutOfMemory)?;
        let cap = self.v.capacity();
        if need <= cap {
            return Ok(());
        }
        let target = need.max(cap.saturating_mul(2)).max(4096);
        self.held.grow_by((target - cap) as u64)?;
        self.v.try_reserve_exact(target - len)?;
        Ok(())
    }

    pub(crate) fn extend(&mut self, p: &[u8]) -> Result<(), Error> {
        self.grow_for(p.len())?;
        self.v.extend_from_slice(p);
        Ok(())
    }

    /// `n` copies of `b`.
    pub(crate) fn fill(&mut self, b: u8, n: usize) -> Result<(), Error> {
        self.grow_for(n)?;
        self.v.resize(self.v.len() + n, b);
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.v.len()
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.v
    }

    pub(crate) fn finish(self, go_cap: usize, nil: bool) -> Decoded {
        Decoded {
            v: self.v,
            held: self.held,
            go_cap,
            nil,
        }
    }
}

/// The sizes of the pieces a decoder handed to `bytes.Buffer.ReadFrom`, in order.
struct Segs {
    v: Vec<usize>,
    held: Held,
}

impl Segs {
    fn new(mem: &Mem) -> Result<Segs, Error> {
        Ok(Segs {
            v: Vec::new(),
            held: mem.charge(0)?,
        })
    }

    fn push(&mut self, n: usize) -> Result<(), Error> {
        if self.v.len() == self.v.capacity() {
            let add = self.v.capacity().max(64);
            self.held.grow_by((add as u64).saturating_mul(8))?;
            self.v.try_reserve_exact(add)?;
        }
        self.v.push(n);
        Ok(())
    }
}

/// `n` calls of `bytes.Buffer.WriteByte` on the capacity model. A byte that fits is a length
/// change; the first byte past the capacity grows the buffer.
pub(crate) fn write_bytes(buf: &mut Buffer, mut n: usize) -> Result<(), Error> {
    while n > 0 {
        let room = buf.cap().saturating_sub(buf.len());
        if room == 0 {
            buf.write(1)?;
            n -= 1;
        } else {
            let k = room.min(n);
            buf.write(k)?;
            n -= k;
        }
    }
    Ok(())
}

/// What a decoder offers `bytes.Buffer.ReadFrom`: Go's `io.Reader`, one `Read` call at a time
/// with a buffer that is never smaller than the piece. Go's `Read` can return data together
/// with the end or an error; that is split here into the data, then the end
/// (`Ok(None)`) or the error, and [`Source::end_carried`] says whether Go delivered them
/// together.
pub(crate) trait Source {
    /// The next piece of output, `Ok(None)` at a clean end (Go's `io.EOF`), or the error.
    fn next(&mut self) -> Result<Option<&[u8]>, Error>;

    /// After the end or an error: whether the last piece reached Go's `ReadFrom` in the same
    /// `Read` call as the end or the error, in which case `ReadFrom` stops without its final
    /// `grow(512)` (docs/design.md section 1, row 2).
    fn end_carried(&self) -> bool;
}

/// What `copyDecoded` returns: Go returns the buffer and the error together.
pub(crate) struct Copied {
    pub(crate) buf: Decoded,
    pub(crate) err: Option<Error>,
}

struct ReadFrom {
    out: Out,
    segs: Segs,
    err: Option<Error>,
    carried: bool,
}

/// `bytes.Buffer.ReadFrom` over `src`, behind an `io.LimitedReader` of `n` bytes if `n` is set.
/// Errors of the decoder are data (`err`); the error returned is the ledger's or the cancel
/// flag's.
fn read_from<S: Source>(
    src: &mut S,
    n: Option<u64>,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<ReadFrom, Error> {
    let mut out = Out::new(mem)?;
    let mut segs = Segs::new(mem)?;
    let mut total: u64 = 0;
    let (err, carried) = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if n.is_some_and(|n| total >= n) {
            // The limited reader reports EOF without asking the decoder.
            break (None, false);
        }
        let ended = match src.next() {
            Ok(Some(piece)) => {
                let take = match n {
                    Some(n) => piece
                        .len()
                        .min(usize::try_from(n - total).unwrap_or(usize::MAX)),
                    None => piece.len(),
                };
                out.extend(&piece[..take])?;
                segs.push(take)?;
                total = total.saturating_add(take as u64);
                None
            }
            Ok(None) => Some(None),
            Err(e) => Some(Some(e)),
        };
        if let Some(err) = ended {
            break (err, src.end_carried());
        }
    };
    Ok(ReadFrom {
        out,
        segs,
        err,
        carried,
    })
}

/// A filter: a kind, its decode parameters and the per-stream decode limit
/// (`NewFilter`'s result).
#[derive(Debug, Clone)]
pub struct Filter {
    kind: Kind,
    parms: Parms,
    max_decode_bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ascii85,
    AsciiHex,
    RunLength,
    Lzw,
    Flate,
    CcittFax,
    Dct,
}

/// `NewFilter`. `max_decode_bytes` is Go's optional variadic argument: `None` is the default
/// 512 MiB, 0 is the default too, and a negative value is no limit.
///
/// # Errors
/// [`Error::UnsupportedFilter`] for JBIG2 and JPX, [`Error::InvalidFilter`] for any other name
/// that is not a filter.
pub fn new_filter(
    name: &[u8],
    parms: Parms,
    max_decode_bytes: Option<i64>,
) -> Result<Filter, Error> {
    let kind = if name == ASCII85 {
        Kind::Ascii85
    } else if name == ASCII_HEX {
        Kind::AsciiHex
    } else if name == RUN_LENGTH {
        Kind::RunLength
    } else if name == LZW {
        Kind::Lzw
    } else if name == FLATE {
        Kind::Flate
    } else if name == CCITT_FAX {
        Kind::CcittFax
    } else if name == DCT {
        Kind::Dct
    } else if name == JBIG2 || name == JPX {
        return Err(Error::UnsupportedFilter(
            String::from_utf8_lossy(name).into_owned(),
        ));
    } else {
        return Err(Error::InvalidFilter(
            String::from_utf8_lossy(name).into_owned(),
        ));
    };
    Ok(Filter {
        kind,
        parms,
        max_decode_bytes: max_decode_bytes.unwrap_or(DEFAULT_MAX_DECODE_BYTES),
    })
}

impl Filter {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Ascii85 => "ASCII85Decode",
            Kind::AsciiHex => "ASCIIHexDecode",
            Kind::RunLength => "RunLengthDecode",
            Kind::Lzw => "LZWDecode",
            Kind::Flate => "FlateDecode",
            Kind::CcittFax => "CCITTFaxDecode",
            Kind::Dct => "DCTDecode",
        }
    }

    /// `Decode`: the whole stream.
    ///
    /// # Errors
    /// The filter's errors; [`Error::DecodeLimit`] if the output passes the decode limit;
    /// [`Error::Alloc`], [`Error::OutOfMemory`] and [`Error::Cancelled`].
    pub fn decode(&self, input: &[u8], mem: &Mem, cancel: &AtomicBool) -> Result<Decoded, Error> {
        self.decode_length(input, -1, mem, cancel)
    }

    /// `DecodeLength`: at least `max_len` bytes, or the whole stream if `max_len` is negative.
    /// The filters that cannot stop early (DCT) decode all of it.
    ///
    /// # Errors
    /// As [`Filter::decode`]; [`Error::DecodeLimit`] also if `max_len` is over the decode limit.
    pub fn decode_length(
        &self,
        input: &[u8],
        max_len: i64,
        mem: &Mem,
        cancel: &AtomicBool,
    ) -> Result<Decoded, Error> {
        match self.kind {
            Kind::Ascii85 => ascii85::decode_length(self, input, max_len, mem, cancel),
            Kind::AsciiHex => ascii_hex::decode_length(self, input, max_len, mem, cancel),
            Kind::RunLength => run_length::decode_length(self, input, max_len, mem, cancel),
            Kind::Lzw => lzw::decode_length(self, input, max_len, mem, cancel),
            Kind::Flate => flate::decode_length(self, input, max_len, mem, cancel),
            Kind::CcittFax => ccitt::decode_length(self, input, max_len, mem, cancel),
            Kind::Dct => dct::decode_length(self, input, max_len, mem, cancel),
        }
    }

    /// `Encode`. Only Flate encodes (docs/scope.md section 6).
    ///
    /// # Errors
    /// [`Error::UnsupportedFilter`] for any other filter; for Flate the predictor errors, the
    /// ledger's and the cancel flag's.
    pub fn encode(&self, input: &[u8], mem: &Mem, cancel: &AtomicBool) -> Result<Decoded, Error> {
        match self.kind {
            Kind::Flate => flate::encode(self, input, mem, cancel),
            _ => Err(Error::UnsupportedFilter(self.name().to_owned())),
        }
    }

    /// `baseFilter.decodeLimit`.
    fn decode_limit(&self, max_len: i64) -> i64 {
        if max_len >= 0 {
            max_len
        } else if self.max_decode_bytes == 0 {
            DEFAULT_MAX_DECODE_BYTES
        } else {
            self.max_decode_bytes
        }
    }

    /// `baseFilter.checkDecodeLength`.
    fn check_decode_length(&self, max_len: i64) -> Result<(), Error> {
        if max_len >= 0 {
            let limit = self.decode_limit(-1);
            if limit >= 0 && max_len > limit {
                return Err(Error::DecodeLimit);
            }
        }
        Ok(())
    }

    /// `baseFilter.copyDecoded`: `io.CopyN` of `max_len` bytes into a `bytes.Buffer`, or, for a
    /// negative `max_len`, `io.Copy` behind a limited reader of `limit + 1` bytes.
    fn copy_decoded<S: Source>(
        &self,
        src: &mut S,
        max_len: i64,
        mem: &Mem,
        cancel: &AtomicBool,
    ) -> Result<Copied, Error> {
        self.check_decode_length(max_len)?;
        if max_len >= 0 {
            let n = max_len.unsigned_abs();
            let r = read_from(src, Some(n), mem, cancel)?;
            let len = r.out.len();
            // CopyN: no error at all once n bytes are there, and EOF if the source stopped short.
            let err = if len as u64 == n {
                None
            } else {
                Some(r.err.unwrap_or(Error::Sentinel(Sentinel::Eof)))
            };
            // Nothing reads the capacity of a prefix.
            return Ok(Copied {
                buf: r.out.finish(len, false),
                err,
            });
        }
        let limit = self.decode_limit(max_len);
        let unlimited = limit < 0 || limit == i64::MAX;
        let n = if unlimited {
            None
        } else {
            Some(limit.unsigned_abs() + 1)
        };
        let r = read_from(src, n, mem, cancel)?;
        if !unlimited && r.out.len() as u64 > limit.unsigned_abs() {
            return Err(Error::DecodeLimit);
        }
        let cap = gocap::simulate_read_from(r.segs.v.iter().copied(), r.carried)?.cap();
        Ok(Copied {
            buf: r.out.finish(cap, false),
            err: r.err,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;

    pub(crate) fn mem() -> Mem {
        Mem::new(&Limits::default())
    }

    #[test]
    fn filter_support() {
        // TestFilterSupport.
        for name in [ASCII85, ASCII_HEX, RUN_LENGTH, LZW, FLATE, CCITT_FAX, DCT] {
            assert!(new_filter(name, Parms::new(), None).is_ok(), "{name:?}");
        }
        for name in [JBIG2, JPX] {
            assert!(matches!(
                new_filter(name, Parms::new(), None),
                Err(Error::UnsupportedFilter(_))
            ));
        }
        let e = new_filter(b"INVALID_FILTER", Parms::new(), None).unwrap_err();
        assert_eq!(e, Error::InvalidFilter("INVALID_FILTER".into()));
        assert_eq!(e.class(), "invalid-filter");
        assert!(supports_decode_parms(FLATE));
        assert!(supports_decode_parms(LZW));
        assert!(supports_decode_parms(CCITT_FAX));
        assert!(!supports_decode_parms(ASCII85));
    }

    #[test]
    fn only_flate_encodes() {
        let cancel = AtomicBool::new(false);
        let f = new_filter(ASCII85, Parms::new(), None).unwrap();
        assert!(matches!(
            f.encode(b"x", &mem(), &cancel),
            Err(Error::UnsupportedFilter(_))
        ));
    }

    #[test]
    fn decode_limit_rules() {
        let f = |max| new_filter(FLATE, Parms::new(), Some(max)).unwrap();
        assert_eq!(f(0).decode_limit(-1), DEFAULT_MAX_DECODE_BYTES);
        assert_eq!(f(-1).decode_limit(-1), -1);
        assert_eq!(f(7).decode_limit(-1), 7);
        assert_eq!(f(7).decode_limit(3), 3);
        assert!(f(7).check_decode_length(7).is_ok());
        assert_eq!(f(7).check_decode_length(8), Err(Error::DecodeLimit));
        assert!(f(-1).check_decode_length(i64::MAX).is_ok());
        let d = new_filter(FLATE, Parms::new(), None).unwrap();
        assert_eq!(d.decode_limit(-1), 512 << 20);
    }

    #[test]
    fn write_bytes_matches_byte_at_a_time() {
        for n in [0usize, 1, 63, 64, 65, 129, 1000, 5000] {
            let mut a = Buffer::new();
            write_bytes(&mut a, n).unwrap();
            let mut b = Buffer::new();
            for _ in 0..n {
                b.write(1).unwrap();
            }
            assert_eq!((a.len(), a.cap()), (b.len(), b.cap()), "n = {n}");
        }
    }

    /// A source of fixed pieces.
    struct Pieces {
        pieces: Vec<Vec<u8>>,
        i: usize,
        carried: bool,
        err: Option<Error>,
    }

    impl Source for Pieces {
        fn next(&mut self) -> Result<Option<&[u8]>, Error> {
            if self.i < self.pieces.len() {
                self.i += 1;
                return Ok(Some(&self.pieces[self.i - 1]));
            }
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(None),
            }
        }

        fn end_carried(&self) -> bool {
            self.carried
        }
    }

    #[test]
    fn copy_decoded_capacity_follows_read_from() {
        let cancel = AtomicBool::new(false);
        let f = new_filter(LZW, Parms::new(), None).unwrap();
        for (carried, cap) in [(false, 1024), (true, 512)] {
            let mut s = Pieces {
                pieces: vec![vec![7; 100]],
                i: 0,
                carried,
                err: None,
            };
            let c = f.copy_decoded(&mut s, -1, &mem(), &cancel).unwrap();
            assert!(c.err.is_none());
            assert_eq!((c.buf.len(), c.buf.go_cap()), (100, cap));
        }
    }

    #[test]
    fn copy_decoded_limits() {
        let cancel = AtomicBool::new(false);
        let f = new_filter(LZW, Parms::new(), Some(4)).unwrap();
        let src = |n| Pieces {
            pieces: vec![vec![1; n]],
            i: 0,
            carried: false,
            err: Some(Error::Lzw),
        };
        // At the limit the decoder's error is returned with the buffer.
        let c = f.copy_decoded(&mut src(4), -1, &mem(), &cancel).unwrap();
        assert_eq!((c.buf.len(), c.err), (4, Some(Error::Lzw)));
        // Over the limit the limit wins over the error.
        assert_eq!(
            f.copy_decoded(&mut src(5), -1, &mem(), &cancel).err(),
            Some(Error::DecodeLimit)
        );
        // A prefix over the limit.
        assert_eq!(
            f.copy_decoded(&mut src(5), 5, &mem(), &cancel).err(),
            Some(Error::DecodeLimit)
        );
        // CopyN: exactly n bytes is no error, even though the source has an error pending.
        let c = f.copy_decoded(&mut src(4), 3, &mem(), &cancel).unwrap();
        assert_eq!((c.buf.as_slice(), c.err), (&[1u8, 1, 1][..], None));
        // CopyN of more than the source has: its error, or EOF.
        let c = f.copy_decoded(&mut src(2), 3, &mem(), &cancel).unwrap();
        assert_eq!(c.err, Some(Error::Lzw));
        let mut s = Pieces {
            pieces: vec![vec![1; 2]],
            i: 0,
            carried: false,
            err: None,
        };
        let c = f.copy_decoded(&mut s, 3, &mem(), &cancel).unwrap();
        assert_eq!(c.err, Some(Error::Sentinel(Sentinel::Eof)));
    }

    #[test]
    fn out_charges_capacity_and_releases() {
        let m = Mem::with_max(1 << 20);
        {
            let mut o = Out::new(&m).unwrap();
            o.extend(&[1u8; 10]).unwrap();
            assert!(m.used() >= 10);
            o.fill(2, 5000).unwrap();
            assert_eq!(o.len(), 5010);
            let d = o.finish(5010, false);
            assert_eq!(d.len(), 5010);
            assert!(m.used() >= 5010);
        }
        assert_eq!(m.used(), 0);
        let tiny = Mem::with_max(100);
        let mut o = Out::new(&tiny).unwrap();
        assert!(matches!(o.extend(&[0u8; 101]), Err(Error::Alloc { .. })));
    }
}
