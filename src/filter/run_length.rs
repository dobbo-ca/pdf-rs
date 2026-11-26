//! RunLengthDecode (Go: pkg/filter/runLengthDecode.go). Decode only (the encoder is out of scope,
//! docs/scope.md section 6).
//!
//! Go writes into a `bytes.Buffer` with `WriteByte`, so the capacity is the write-path model
//! with one-byte writes ([`super::write_bytes`]). A result of no bytes is a nil slice.

use super::{Decoded, Filter, Out, write_bytes};
use crate::error::Error;
use crate::gocap::Buffer;
use crate::ledger::Mem;
use std::sync::atomic::{AtomicBool, Ordering};

/// How many input bytes are decoded between two looks at the cancel flag.
const CANCEL_STEP: usize = 1 << 16;

/// The output so far, with the capacity of Go's buffer.
struct Sink {
    out: Out,
    cap: Buffer,
}

impl Sink {
    fn put(&mut self, src: &[u8]) -> Result<(), Error> {
        self.out.extend(src)?;
        write_bytes(&mut self.cap, src.len())
    }

    fn repeat(&mut self, b: u8, n: usize) -> Result<(), Error> {
        self.out.fill(b, n)?;
        write_bytes(&mut self.cap, n)
    }
}

/// How many of the next `c` bytes `decode` may write before its limit check fires, and whether
/// it fires. Go checks `limit == written` before every byte.
fn allowed(limit: i64, written: i64, c: usize) -> (usize, bool) {
    if limit < 0 {
        return (c, false);
    }
    let room = limit.saturating_sub(written).max(0);
    match usize::try_from(room) {
        Ok(room) if room < c => (room, true),
        _ => (c, false),
    }
}

/// `decode`: `Ok(true)` is a stop at the limit of a prefix, which Go returns as success.
fn decode(
    sink: &mut Sink,
    src: &[u8],
    limit: i64,
    prefix: bool,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let mut written: i64 = 0;
    let mut i = 0;
    let mut since_check = 0usize;
    while i < src.len() {
        if since_check >= CANCEL_STEP {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            since_check = 0;
        }
        let b = src[i];
        if b == 0x80 {
            // eod
            return Ok(());
        }
        i += 1;
        if b < 0x80 {
            let c = usize::from(b) + 1;
            if src.len() - i < c {
                return Err(Error::UnexpectedEof);
            }
            let (n, hit) = allowed(limit, written, c);
            sink.put(&src[i..i + n])?;
            written += n as i64;
            if hit {
                return if prefix {
                    Ok(())
                } else {
                    Err(Error::DecodeLimit)
                };
            }
            i += c;
            since_check += c;
            continue;
        }
        if i >= src.len() {
            return Err(Error::UnexpectedEof);
        }
        let c = 257 - usize::from(b);
        let (n, hit) = allowed(limit, written, c);
        sink.repeat(src[i], n)?;
        written += n as i64;
        if hit {
            return if prefix {
                Ok(())
            } else {
                Err(Error::DecodeLimit)
            };
        }
        i += 1;
        since_check += c;
    }
    Ok(())
}

/// `DecodeLength`: a negative `max_len` is the whole stream, under the decode limit.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    f.check_decode_length(max_len)?;
    let mut sink = Sink {
        out: Out::new(mem)?,
        cap: Buffer::new(),
    };
    decode(
        &mut sink,
        input,
        f.decode_limit(max_len),
        max_len >= 0,
        cancel,
    )?;
    let nil = sink.out.len() == 0;
    let cap = sink.cap.cap();
    Ok(sink.out.finish(cap, nil))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::mem;
    use crate::filter::{Parms, RUN_LENGTH, new_filter};

    /// `encode` of the Go source, for the round trip of the tests.
    fn encode(src: &[u8]) -> Vec<u8> {
        const MAX_LEN: usize = 0x80;
        let mut w = Vec::new();
        if src.is_empty() {
            w.push(0x80);
            return w;
        }
        let detect = |mut i: usize, start: usize, b: u8| {
            while i < src.len() && src[i] == b && i - start < MAX_LEN {
                i += 1;
            }
            i
        };
        let mut i = 0;
        let mut b = src[i];
        let mut start = i;
        loop {
            i = detect(i, start, b);
            let mut c = i - start;
            if c > 1 {
                w.push((257 - c) as u8);
                w.push(b);
                if i == src.len() {
                    w.push(0x80);
                    return w;
                }
                b = src[i];
                start = i;
                continue;
            }
            while i < src.len() && src[i] != b && i - start < MAX_LEN {
                b = src[i];
                i += 1;
            }
            if i == src.len() || i - start == MAX_LEN {
                c = i - start;
                w.push((c - 1) as u8);
                w.extend_from_slice(&src[start..start + c]);
                if i == src.len() {
                    w.push(0x80);
                    return w;
                }
            } else {
                c = i - 1 - start;
                w.push((c - 1) as u8);
                w.extend_from_slice(&src[start..start + c]);
                i -= 1;
            }
            b = src[i];
            start = i;
        }
    }

    fn dec(enc: &[u8], max_len: i64, limit: Option<i64>) -> Result<Decoded, Error> {
        let f = new_filter(RUN_LENGTH, Parms::new(), limit).unwrap();
        f.decode_length(enc, max_len, &mem(), &AtomicBool::new(false))
    }

    #[test]
    fn run_length_encoding() {
        // TestRunLengthEncoding.
        let cases: [(&[u8], &[u8]); 16] = [
            (b"\x01", b"\x00\x01\x80"),
            (b"\x01\x01", b"\xFF\x01\x80"),
            (b"\x00\x00\x02\x02", b"\xFF\x00\xFF\x02\x80"),
            (b"\x00\x00\x00", b"\xFE\x00\x80"),
            (b"\x00\x00\x00\x01", b"\xFE\x00\x00\x01\x80"),
            (b"\x00\x00\x00\x00", b"\xFD\x00\x80"),
            (b"\x00\x00\x00\x00\x00", b"\xFC\x00\x80"),
            (b"\x00\x00\x01", b"\xFF\x00\x00\x01\x80"),
            (b"\x00\x01", b"\x01\x00\x01\x80"),
            (b"\x00\x01\x02", b"\x02\x00\x01\x02\x80"),
            (b"\x00\x01\x02\x03", b"\x03\x00\x01\x02\x03\x80"),
            (b"\x00\x01\x02\x03\x02", b"\x04\x00\x01\x02\x03\x02\x80"),
            (b"\x00\x01", b"\x01\x00\x01\x80"),
            (b"\x00\x01\x01", b"\x00\x00\xFF\x01\x80"),
            (b"\x00\x01\x01\x01", b"\x00\x00\xFE\x01\x80"),
            (
                b"\x00\x00\x01\x02\x00\x00",
                b"\xFF\x00\x01\x01\x02\xFF\x00\x80",
            ),
        ];
        for (raw, enc) in cases {
            assert_eq!(encode(raw), enc);
            assert_eq!(dec(enc, -1, None).unwrap().as_slice(), raw);
        }
    }

    #[test]
    fn truncated_runs_are_unexpected_eof() {
        // TestRunLengthDecodeTruncatedLiteralRun, TestRunLengthDecodeTruncatedRepeatRun.
        assert_eq!(
            dec(&[0x02, b'a'], -1, None).err(),
            Some(Error::UnexpectedEof)
        );
        assert_eq!(dec(&[0xff], -1, None).err(), Some(Error::UnexpectedEof));
    }

    #[test]
    fn length_stops_at_limit() {
        // TestRunLengthDecodeLengthStopsAtLimit.
        let d = dec(&[0x02, b'a', b'b', b'c', 0x80], 2, None).unwrap();
        assert_eq!(d.as_slice(), b"ab");
        // A repeat run stops the same way.
        assert_eq!(dec(&[0xfd, b'x', 0x80], 2, None).unwrap().as_slice(), b"xx");
        // A prefix longer than the data is not an error here (the stream dict reports it).
        assert_eq!(dec(&[0x00, b'a', 0x80], 5, None).unwrap().as_slice(), b"a");
    }

    #[test]
    fn full_decode_limit() {
        // Exactly the limit is fine, one byte more is the limit error.
        assert!(dec(&[0x02, b'a', b'b', b'c', 0x80], -1, Some(3)).is_ok());
        assert_eq!(
            dec(&[0x02, b'a', b'b', b'c', 0x80], -1, Some(2)).err(),
            Some(Error::DecodeLimit)
        );
        assert_eq!(
            dec(&[0xfe, b'x', 0x80], -1, Some(2)).err(),
            Some(Error::DecodeLimit)
        );
        // A prefix over the limit is rejected before any decoding.
        assert_eq!(
            dec(&[0x00, b'a'], 3, Some(2)).err(),
            Some(Error::DecodeLimit)
        );
        // A limit of one byte with an empty literal run of eod only.
        assert!(dec(&[0x80], -1, Some(1)).is_ok());
    }

    #[test]
    fn capacity_is_the_write_path_and_empty_is_nil() {
        let d = dec(&[0x80], -1, None).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (0, 0, true));
        // One byte: the first write makes the 64 byte buffer.
        let d = dec(&[0x00, b'a', 0x80], -1, None).unwrap();
        assert_eq!((d.len(), d.go_cap(), d.is_nil()), (1, 64, false));
        // 65 bytes written one at a time: 64, then 2 * 64 = 128.
        let d = dec(&[0xbf, b'z', 0x80], -1, None).unwrap();
        assert_eq!((d.len(), d.go_cap()), (257 - 0xbf, 128));
    }
}
