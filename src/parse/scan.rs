//! Line splitting (Go: pkg/pdfcpu/scan/scan.go) and an emulation of `bufio.Scanner`'s buffer
//! rules (Go: bufio/scan.go, behaviour only).
//!
//! `lines` and `lines_single_eol` are Go's split functions: `(advance, token)`, with `None` for
//! Go's nil token ("request more data"). [`Scanner`] runs one of them over an in-memory input
//! with the same buffer sizes as `bufio.Scanner`, so a line longer than the buffer limit fails
//! at the same byte as in Go (`bufio.ErrTooLong`). The read path (read.go:2080, :2369) builds its
//! scanners with [`Scanner::with_buffer`]; the default scanner (xreftable.go:1424) is
//! [`Scanner::new`].

use crate::error::Error;

/// A split function: `(advance, token)` for the data in the buffer and whether the input has
/// ended. `None` is Go's nil token.
pub type SplitFn = for<'a> fn(&'a [u8], bool) -> (usize, Option<&'a [u8]>);

/// `bufio.MaxScanTokenSize`: the default limit of one token.
pub const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

/// `bufio.startBufSize`: the first buffer of a scanner that was given none.
const START_BUF_SIZE: usize = 4096;

const ERR_TOO_LONG: &str = "bufio.Scanner: token too long";
const ERR_ADVANCE_TOO_FAR: &str = "bufio.Scanner: SplitFunc returns advance count beyond input";

/// `Lines`: each line without its end-of-line marker, which is one CR LF, one CR or one LF. The
/// token may be empty. The last non-empty line is returned even without a marker.
///
/// Go looks for the first CR and the first LF separately. The first marker of either kind gives
/// the same answer: an LF ends the line; a CR ends it too, together with an LF that follows it
/// directly; a CR in the last byte waits for more data, because an LF may follow.
pub fn lines(data: &[u8], at_eof: bool) -> (usize, Option<&[u8]>) {
    if at_eof && data.is_empty() {
        return (0, None);
    }
    match data.iter().position(|&b| b == b'\r' || b == b'\n') {
        Some(p) => {
            if data[p] == b'\n' {
                // \n
                return (p + 1, Some(&data[..p]));
            }
            if p + 1 == data.len() {
                if !at_eof {
                    // A LF may follow in the next read: request more data.
                    return (0, None);
                }
                return (p + 1, Some(&data[..p]));
            }
            if data[p + 1] == b'\n' {
                // \r\n
                return (p + 2, Some(&data[..p]));
            }
            // \r
            (p + 1, Some(&data[..p]))
        }
        None => {
            // If we're at EOF, we have a final, non-terminated line. Return it.
            if at_eof {
                (data.len(), Some(data))
            } else {
                // Request more data.
                (0, None)
            }
        }
    }
}

/// `LinesSingleEOL`: like [`lines`], but a CR and an LF are each a line end of their own.
pub fn lines_single_eol(data: &[u8], at_eof: bool) -> (usize, Option<&[u8]>) {
    if at_eof && data.is_empty() {
        return (0, None);
    }
    match data.iter().position(|&b| b == b'\r' || b == b'\n') {
        Some(p) => (p + 1, Some(&data[..p])),
        None => {
            if at_eof {
                (data.len(), Some(data))
            } else {
                (0, None)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Eof,
    TooLong,
    AdvanceTooFar,
}

fn full_read(room: usize) -> usize {
    room
}

/// `bufio.Scanner` over an in-memory input. The scanner keeps `bufio`'s buffer arithmetic (the
/// size, the shifts, the doubling up to the limit) but not its bytes: the buffer is a window of
/// the input, so tokens borrow the input and nothing is copied.
pub struct Scanner<'a> {
    data: &'a [u8],
    split: SplitFn,
    max_token: usize,
    /// Go's `len(s.buf)`.
    buf_len: usize,
    /// Where `buf[0]` is in `data`.
    base: usize,
    /// Go's `s.start` and `s.end`, relative to `buf[0]`.
    start: usize,
    end: usize,
    token: Option<&'a [u8]>,
    err: Option<Stop>,
    /// How many bytes one read returns out of the room in the buffer (a `Reader` may return
    /// less than it is asked for; the tests use it to model `iotest`'s readers).
    read_size: fn(usize) -> usize,
}

impl<'a> Scanner<'a> {
    /// `bufio.NewScanner`: no buffer yet (the first is 4096 bytes), tokens up to
    /// [`MAX_SCAN_TOKEN_SIZE`].
    pub fn new(data: &'a [u8], split: SplitFn) -> Scanner<'a> {
        Scanner::with_buffer(data, split, 0, MAX_SCAN_TOKEN_SIZE)
    }

    /// `bufio.NewScanner` followed by `Buffer(make([]byte, 0, initial_len), max_token)`.
    pub fn with_buffer(
        data: &'a [u8],
        split: SplitFn,
        initial_len: usize,
        max_token: usize,
    ) -> Scanner<'a> {
        Scanner {
            data,
            split,
            max_token,
            buf_len: initial_len,
            base: 0,
            start: 0,
            end: 0,
            token: None,
            err: None,
            read_size: full_read,
        }
    }

    #[cfg(test)]
    fn with_read_size(mut self, read_size: fn(usize) -> usize) -> Scanner<'a> {
        self.read_size = read_size;
        self
    }

    /// `setErr`: the first error wins, except that it replaces `io.EOF`.
    fn set_err(&mut self, e: Stop) {
        if self.err.is_none() || self.err == Some(Stop::Eof) {
            self.err = Some(e);
        }
    }

    /// `Scan`: advances to the next token. False at the end of the input or on an error.
    pub fn scan(&mut self) -> bool {
        loop {
            // See if we can get a token with what we already have. If we've run out of data but
            // have an error, give the split function a chance to recover any remaining,
            // possibly empty token.
            if self.end > self.start || self.err.is_some() {
                let data: &'a [u8] = self.data;
                let window: &'a [u8] = data
                    .get(self.base + self.start..self.base + self.end)
                    .unwrap_or(&[]);
                let (advance, token) = (self.split)(window, self.err.is_some());
                if advance > self.end - self.start {
                    self.set_err(Stop::AdvanceTooFar);
                    return false;
                }
                self.start += advance;
                self.token = token;
                if token.is_some() {
                    return true;
                }
            }
            // We cannot generate a token with what we are holding. If we've already hit EOF or
            // an I/O error, we are done.
            if self.err.is_some() {
                self.start = 0;
                self.end = 0;
                return false;
            }
            // Must read more data. First, shift data to the beginning of the buffer if there's
            // lots of empty space or space is needed.
            if self.start > 0 && (self.end == self.buf_len || self.start > self.buf_len / 2) {
                self.base += self.start;
                self.end -= self.start;
                self.start = 0;
            }
            // Is the buffer full? If so, resize.
            if self.end >= self.buf_len {
                if self.buf_len >= self.max_token || self.buf_len > usize::MAX / 2 {
                    self.set_err(Stop::TooLong);
                    return false;
                }
                let mut new_size = self.buf_len * 2;
                if new_size == 0 {
                    new_size = START_BUF_SIZE;
                }
                new_size = new_size.min(self.max_token);
                self.base += self.start;
                self.end -= self.start;
                self.start = 0;
                self.buf_len = new_size;
            }
            // Read some input. The reader returns what is left of the input, up to the room in
            // the buffer; with nothing left it returns io.EOF.
            let room = self.buf_len - self.end;
            let at = self.base + self.end;
            let avail = self.data.len().saturating_sub(at);
            if avail == 0 {
                self.set_err(Stop::Eof);
                continue;
            }
            let n = (self.read_size)(room).max(1).min(room).min(avail);
            self.end += n;
        }
    }

    /// `Bytes`: the most recent token (empty after a nil token).
    pub fn bytes(&self) -> &'a [u8] {
        self.token.unwrap_or(&[])
    }

    /// `Err`: the error that stopped the scan, `None` at a clean end of the input.
    pub fn err(&self) -> Option<Error> {
        match self.err {
            None | Some(Stop::Eof) => None,
            Some(Stop::TooLong) => Some(Error::Corrupt(ERR_TOO_LONG.to_owned())),
            Some(Stop::AdvanceTooFar) => Some(Error::Corrupt(ERR_ADVANCE_TOO_FAR.to_owned())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use super::*;

    fn one_byte(_room: usize) -> usize {
        1
    }

    fn half(room: usize) -> usize {
        room.div_ceil(2)
    }

    fn scan_all(mut s: Scanner<'_>) -> (Vec<Vec<u8>>, Option<Error>) {
        let mut got = Vec::new();
        while s.scan() {
            got.push(s.bytes().to_vec());
        }
        let err = s.err();
        (got, err)
    }

    /// Go: TestLinesAcrossReads.
    #[test]
    fn lines_across_reads() {
        let long = vec![b'a'; 4095];
        let mut boundary = long.clone();
        boundary.extend_from_slice(b"\r\nb");
        let cases: Vec<(&str, Vec<u8>, Vec<Vec<u8>>)> = vec![
            ("empty", b"".to_vec(), vec![]),
            ("unterminated", b"abc".to_vec(), vec![b"abc".to_vec()]),
            (
                "crlf",
                b"a\r\nb\r\nc".to_vec(),
                vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            ),
            ("trailing crlf", b"a\r\n".to_vec(), vec![b"a".to_vec()]),
            (
                "cr",
                b"a\rb\rc".to_vec(),
                vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            ),
            ("trailing cr", b"a\r".to_vec(), vec![b"a".to_vec()]),
            (
                "lf",
                b"a\nb\nc".to_vec(),
                vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            ),
            ("trailing lf", b"a\n".to_vec(), vec![b"a".to_vec()]),
            ("empty crlf line", b"\r\n".to_vec(), vec![b"".to_vec()]),
            ("empty cr line", b"\r".to_vec(), vec![b"".to_vec()]),
            ("empty lf line", b"\n".to_vec(), vec![b"".to_vec()]),
            (
                "mixed",
                b"\r\na\rb\nc\r\n\r\nd".to_vec(),
                vec![
                    b"".to_vec(),
                    b"a".to_vec(),
                    b"b".to_vec(),
                    b"c".to_vec(),
                    b"".to_vec(),
                    b"d".to_vec(),
                ],
            ),
            (
                "consecutive cr",
                b"a\r\rb\r\r".to_vec(),
                vec![b"a".to_vec(), b"".to_vec(), b"b".to_vec(), b"".to_vec()],
            ),
            (
                "lf then cr",
                b"a\n\rb".to_vec(),
                vec![b"a".to_vec(), b"".to_vec(), b"b".to_vec()],
            ),
            (
                "buffer boundary",
                boundary,
                vec![long.clone(), b"b".to_vec()],
            ),
        ];
        let readers: [(&str, fn(usize) -> usize); 3] = [
            ("buffered", full_read),
            ("one byte", one_byte),
            ("half", half),
        ];
        for (name, input, want) in &cases {
            for (rname, read) in readers {
                let s = Scanner::new(input, lines).with_read_size(read);
                let (got, err) = scan_all(s);
                assert!(err.is_none(), "{name}/{rname}: {err:?}");
                assert_eq!(&got, want, "{name}/{rname}");
            }
        }
    }

    #[test]
    fn single_eol_ends_a_line_at_each_marker() {
        let s = Scanner::new(b"a\r\nb\rc\nd", lines_single_eol);
        let (got, err) = scan_all(s);
        assert!(err.is_none());
        let want: Vec<Vec<u8>> = vec![
            b"a".to_vec(),
            b"".to_vec(),
            b"b".to_vec(),
            b"c".to_vec(),
            b"d".to_vec(),
        ];
        assert_eq!(got, want);
        // A CR in the last byte is a line end of its own, with or without more data to come.
        assert_eq!(lines_single_eol(b"a\r", false), (2, Some(&b"a"[..])));
        assert_eq!(lines(b"a\r", false), (0, None));
        assert_eq!(lines(b"a\r", true), (2, Some(&b"a"[..])));
        assert_eq!(lines(b"", true), (0, None));
        assert_eq!(lines_single_eol(b"", true), (0, None));
    }

    /// The `ErrTooLong` boundary: the buffer holds at most `max` bytes, and a line needs room for
    /// its marker, and for the byte after a final CR.
    #[test]
    fn too_long_boundary() {
        // (input, lines before the error, whether it ends in ErrTooLong)
        let cases: [(&[u8], &[&[u8]], bool); 7] = [
            (b"abcdefg\n", &[b"abcdefg"], false),
            (b"abcdefgh\n", &[], true),
            (b"abcdefgh", &[], true),
            (b"abcdefg", &[b"abcdefg"], false),
            (b"abcdefg\r", &[], true),
            (b"abcdefg\r\n", &[], true),
            (
                b"abc\nabcdefg\nabcdefg",
                &[b"abc", b"abcdefg", b"abcdefg"],
                false,
            ),
        ];
        for (input, want, too_long) in cases {
            let s = Scanner::with_buffer(input, lines, 4, 8);
            let (got, err) = scan_all(s);
            let want: Vec<Vec<u8>> = want.iter().map(|w| w.to_vec()).collect();
            assert_eq!(got, want, "{input:?}");
            assert_eq!(err.is_some(), too_long, "{input:?}");
        }
        // The same limit through the 1 MiB scanner of the xref reader: exactly `max - 1` bytes
        // and a LF fit, `max` bytes and a LF do not.
        let max = 1024 * 1024;
        let mut fits = vec![b'x'; max - 1];
        fits.push(b'\n');
        let (got, err) = scan_all(Scanner::with_buffer(&fits, lines, 4096, max));
        assert!(err.is_none());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].len(), max - 1);
        let mut over = vec![b'x'; max];
        over.push(b'\n');
        let (got, err) = scan_all(Scanner::with_buffer(&over, lines, 4096, max));
        assert!(got.is_empty());
        assert_eq!(err, Some(Error::Corrupt(ERR_TOO_LONG.to_owned())));
    }

    #[test]
    fn small_initial_buffer_may_exceed_the_limit() {
        // Go's Buffer: the scanner uses the full initial capacity even when it exceeds the
        // maximum; it only fails when it would have to grow.
        let (got, err) = scan_all(Scanner::with_buffer(b"abcdefghij\nk", lines, 16, 8));
        assert!(err.is_none());
        let want: Vec<Vec<u8>> = vec![b"abcdefghij".to_vec(), b"k".to_vec()];
        assert_eq!(got, want);
    }
}
