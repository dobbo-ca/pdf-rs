//! ASCIIHexDecode (Go: pkg/filter/asciiHexDecode.go). Decode only.
//!
//! Go builds the result with `make([]byte, n)` and `bytes.NewBuffer`, so its capacity is exactly
//! its length, and a zero-length result is not nil.

use super::{Decoded, Filter, Out};
use crate::error::Error;
use crate::ledger::Mem;
use std::sync::atomic::{AtomicBool, Ordering};

const EOD: u8 = b'>';
/// How many input bytes are scanned between two looks at the cancel flag.
const CANCEL_STEP: usize = 1 << 16;

fn is_white(b: u8) -> bool {
    matches!(b, 0x00 | 0x09 | 0x0a | 0x0c | 0x0d | 0x20)
}

/// `reverseHexTable`: the value of a hex digit of either case.
fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `DecodeLength`: whitespace is dropped, the end is the first `>`, an odd digit count gets a
/// trailing `0`.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    f.check_decode_length(max_len)?;

    let mut p = Out::new(mem)?;
    for chunk in input.chunks(CANCEL_STEP) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let end = chunk.iter().position(|&b| b == EOD);
        for &b in &chunk[..end.unwrap_or(chunk.len())] {
            if !is_white(b) {
                p.fill(b, 1)?;
            }
        }
        if end.is_some() {
            break;
        }
    }
    if p.len() % 2 == 1 {
        p.fill(b'0', 1)?;
    }

    let decoded_len = i64::try_from(p.len() / 2).map_err(|_| Error::OutOfMemory)?;
    let n = if max_len < 0 {
        let limit = f.decode_limit(-1);
        if limit >= 0 && decoded_len > limit {
            return Err(Error::DecodeLimit);
        }
        decoded_len
    } else if max_len > decoded_len {
        return Err(Error::UnexpectedEof);
    } else {
        max_len
    };
    // Go also rejects a length over `maxInt/2`, which `p` cannot reach.
    let n = usize::try_from(n)
        .map_err(|_| Error::Corrupt("ASCIIHex decode: length overflow".into()))?;

    let mut dst = Out::zeroed(mem, n)?;
    {
        let d = dst.as_mut_slice();
        // The digits are scanned from `p`, which is the filtered copy of the input.
        let src = p.as_mut_slice();
        for (i, pair) in src[..n * 2].chunks_exact(2).enumerate() {
            let a = digit(pair[0]).ok_or(Error::Hex(pair[0]))?;
            let b = digit(pair[1]).ok_or(Error::Hex(pair[1]))?;
            d[i] = (a << 4) | b;
        }
    }
    Ok(dst.finish(n, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::mem;
    use crate::filter::{ASCII_HEX, Parms, new_filter};

    fn dec(input: &[u8], max_len: i64, limit: Option<i64>) -> Result<Decoded, Error> {
        let f = new_filter(ASCII_HEX, Parms::new(), limit).unwrap();
        f.decode_length(input, max_len, &mem(), &AtomicBool::new(false))
    }

    #[test]
    fn decodes_and_ignores_white_space() {
        let d = dec(b"48 65\n6c6C 6f>garbage", -1, None).unwrap();
        assert_eq!(d.as_slice(), b"Hello");
        // The capacity is the length, and nothing is nil.
        assert_eq!((d.go_cap(), d.is_nil()), (5, false));
        // Odd digit count: a 0 is added; no `>` is fine.
        assert_eq!(dec(b"414", -1, None).unwrap().as_slice(), b"A@");
        let e = dec(b">", -1, None).unwrap();
        assert_eq!((e.len(), e.go_cap(), e.is_nil()), (0, 0, false));
    }

    #[test]
    fn invalid_digit() {
        assert_eq!(dec(b"4g>", -1, None).err(), Some(Error::Hex(b'g')));
        assert_eq!(dec(b"g4>", -1, None).err(), Some(Error::Hex(b'g')));
    }

    #[test]
    fn length_too_long_returns_unexpected_eof() {
        // TestASCIIHexDecodeLengthTooLongReturnsError.
        assert_eq!(dec(b"00>", 2, None).err(), Some(Error::UnexpectedEof));
        assert_eq!(dec(b"0000>", 2, None).unwrap().as_slice(), &[0, 0]);
        assert_eq!(dec(b"0102>", 1, None).unwrap().as_slice(), &[1]);
    }

    #[test]
    fn limit_exceeded() {
        // TestASCIIHexDecodeLimitExceeded.
        assert_eq!(dec(b"0000>", -1, Some(1)).err(), Some(Error::DecodeLimit));
        assert!(dec(b"00>", -1, Some(1)).is_ok());
        // A prefix over the limit.
        assert_eq!(dec(b"0000>", 2, Some(1)).err(), Some(Error::DecodeLimit));
        // No limit.
        assert!(dec(b"0000>", -1, Some(-1)).is_ok());
    }

    #[test]
    fn cancelled() {
        let f = new_filter(ASCII_HEX, Parms::new(), None).unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            f.decode(b"00>", &mem(), &cancel).err(),
            Some(Error::Cancelled)
        );
    }
}
