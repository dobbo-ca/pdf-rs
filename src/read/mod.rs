//! The read path, part I (Go: pkg/pdfcpu/read.go:1-546, :829-2470, :2832-2991; docs/design.md
//! section 15, WP 6): `read`, the header, the cross-reference tables and streams, the trailer,
//! the repair scanner, and the object buffer (`buffer`, `object`, `resolve_object`,
//! `parse_object_at`).
//!
//! * [`xref`]: the last `startxref`, classic cross-reference sections, subsections, the trailer
//!   and the `Prev` scan.
//! * [`xrefstm`]: cross-reference streams and hybrid files.
//! * [`repair`]: the header version, the repair scanner (`bypass_xref_section`), and the loop
//!   that follows `Prev` (`build_xref_table_starting_at`, `read_xref_table`).
//! * `stream`, `objstm` and `deref` belong to WP 8. This package only declares the few functions
//!   it calls there; see the placeholder files.
//!
//! The input is an in-memory `&[u8]` (Go's `io.ReadSeeker`). A [`PosReader`] is Go's
//! `newPositionedReader` (a `bufio.Reader` after a `Seek`); the byte counts Go sees from its
//! readers are the same here, because a read of `n` bytes returns `min(n, what is left)`.
//!
//! Go's `context.Context` is a [`ReadCx`]: the cancel flag, the shared object read budget
//! (`objectReadBudgetKey`) and the set of objects being parsed (`objectParseKey`). Go's
//! validation mode is always relaxed here (docs/design.md section 6), so the strict branches
//! of the Go code are not ported; neither are the log calls, `ShowRepaired`, `ShowSkipped` and
//! the validation notices.
//!
//! Gaps in the Go line ranges the design assigns to the read packages, ported here because this
//! package calls them: `createXRefTableEntryFromXRefStream` and `xRefStreamEntryLen`
//! (read.go:747-828, in [`xrefstm`]); `objectReadBudget`, `objectExtent`, `chargeObjectRead`,
//! `streamDictForObject`, `dict` and `parseObjectWithValidationPolicy` (read.go:2720-2830, here);
//! and `dereferencedObject`, `dereferencedInteger`, `bootstrapIntegerEntry` and
//! `dereferencedDict` (read.go:2991-3137, here).

pub mod deref;
pub mod objstm;
pub mod repair;
pub mod stream;
pub mod xref;
pub mod xrefstm;

use crate::config::{Config, DEFAULT_MAX_OBJECT_BYTES, Limits};
use crate::crypto::decrypt_object_strings;
use crate::error::{Error, Sentinel};
use crate::ledger::{Held, Mem};
use crate::model::{Context, EntryObject, ReadInfo};
use crate::object::{Dict, Object, StreamDict};
use crate::parse::scan::{Scanner, lines_single_eol};
use crate::parse::{ParseCtx, detect_keywords, parse_object_attributes, parse_object_with_policy};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// `defaultBufSize`: the first growth of an object buffer.
pub const DEFAULT_BUF_SIZE: usize = 1 << 10;
/// `maxBufSize`: the largest growth step of an object buffer.
pub const MAX_BUF_SIZE: usize = 1 << 20;

/// Allow room for repair passes while bounding repeated reads of the same input
/// (`objectReadBudgetFactor`).
const OBJECT_READ_BUDGET_FACTOR: i64 = 8;

const MSG_MISSING_SCANNER_LINE: &str = "missing scanner line";

// ---------------------------------------------------------------------------------------------
// Errors

/// `ErrCorruptHeader`.
pub fn err_corrupt_header() -> Error {
    Error::Corrupt("no header version available".to_string())
}

/// `ErrEmptyInput`.
pub fn err_empty_input() -> Error {
    Error::Corrupt("the file could not be opened because it is empty".to_string())
}

/// `ErrPostScriptInput`.
pub fn err_postscript_input() -> Error {
    Error::Corrupt(
        "PostScript input is not supported; convert to PDF before processing".to_string(),
    )
}

/// `errMissingScannerLine`.
pub fn err_missing_scanner_line() -> Error {
    Error::Corrupt(MSG_MISSING_SCANNER_LINE.to_string())
}

/// `errors.Is(err, errMissingScannerLine)`.
pub fn is_missing_scanner_line(e: &Error) -> bool {
    matches!(e.root(), Error::Corrupt(m) if m == MSG_MISSING_SCANNER_LINE)
}

/// `errCorruptXRefStream`.
pub fn err_corrupt_xref_stream() -> Error {
    Error::Corrupt("corrupt xref stream".to_string())
}

/// `errCorruptTrailerDict`.
pub fn err_corrupt_trailer_dict() -> Error {
    Error::Corrupt("corrupt trailer dict".to_string())
}

/// An error that `object` reports together with the `endInd` Go returns beside it
/// (`processObject` and `parseAndLoad` use it to resume a repair scan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjFail {
    /// Where the `endobj` keyword was found, -1 for none; 0 where Go returns its zero value.
    pub end_ind: i64,
    pub err: Error,
}

impl From<Error> for ObjFail {
    fn from(err: Error) -> ObjFail {
        ObjFail { end_ind: 0, err }
    }
}

// ---------------------------------------------------------------------------------------------
// Byte helpers (Go's strings functions on the bytes of a line)

/// A non-negative index from an `int` that is known to be non-negative.
pub(crate) fn ix(v: i64) -> usize {
    usize::try_from(v).unwrap_or(0)
}

/// An `i64` limit from an unsigned field, clamped to Go's maximum.
pub(crate) fn lim(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// An `i64` from a length.
pub(crate) fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `strings.Index`.
pub(crate) fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w == n)
}

/// `strings.LastIndex`.
pub(crate) fn rfind(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(h.len());
    }
    h.windows(n.len()).rposition(|w| w == n)
}

/// `strings.Contains`.
pub(crate) fn contains(h: &[u8], n: &[u8]) -> bool {
    find(h, n).is_some()
}

/// `strings.IndexAny` for ASCII `set`.
pub(crate) fn find_any(h: &[u8], set: &[u8]) -> Option<usize> {
    h.iter().position(|b| set.contains(b))
}

/// `strings.TrimLeft` for ASCII `set`.
pub(crate) fn trim_left<'a>(s: &'a [u8], set: &[u8]) -> &'a [u8] {
    let n = s.iter().take_while(|b| set.contains(b)).count();
    &s[n..]
}

/// `strings.TrimRight` for ASCII `set`.
pub(crate) fn trim_right<'a>(s: &'a [u8], set: &[u8]) -> &'a [u8] {
    let n = s.iter().rev().take_while(|b| set.contains(b)).count();
    &s[..s.len() - n]
}

/// The first char of `s` and its length in bytes, if `s` starts with valid UTF-8.
fn first_char(s: &[u8]) -> Option<(char, usize)> {
    let chunk = &s[..s.len().min(4)];
    let valid = match std::str::from_utf8(chunk) {
        Ok(v) => v,
        Err(e) => std::str::from_utf8(&chunk[..e.valid_up_to()]).ok()?,
    };
    valid.chars().next().map(|c| (c, c.len_utf8()))
}

/// The last char of `s` and its length in bytes, if `s` ends with valid UTF-8.
fn last_char(s: &[u8]) -> Option<(char, usize)> {
    let n = s.len();
    for back in 1..=n.min(4) {
        let tail = &s[n - back..];
        if let Ok(v) = std::str::from_utf8(tail)
            && v.chars().count() == 1
        {
            return v.chars().next().map(|c| (c, back));
        }
    }
    None
}

/// `strings.TrimSpace`: Unicode white space at both ends; invalid UTF-8 is not space.
pub(crate) fn go_trim_space(s: &[u8]) -> &[u8] {
    let mut s = s;
    while let Some((c, n)) = first_char(s)
        && c.is_whitespace()
    {
        s = &s[n..];
    }
    while let Some((c, n)) = last_char(s)
        && c.is_whitespace()
    {
        s = &s[..s.len() - n];
    }
    s
}

/// `strings.Fields`: the runs of `s` between Unicode white space.
pub(crate) fn go_fields(s: &[u8]) -> Result<Vec<&[u8]>, Error> {
    fn runs(s: &[u8], mut f: impl FnMut(usize, usize)) {
        let mut start: Option<usize> = None;
        let mut i = 0;
        while i < s.len() {
            let (space, w) = match first_char(&s[i..]) {
                Some((c, w)) => (c.is_whitespace(), w),
                None => (false, 1),
            };
            if space {
                if let Some(st) = start.take() {
                    f(st, i);
                }
            } else if start.is_none() {
                start = Some(i);
            }
            i += w;
        }
        if let Some(st) = start {
            f(st, s.len());
        }
    }
    let mut n = 0usize;
    runs(s, |_, _| n += 1);
    let mut out: Vec<&[u8]> = Vec::new();
    out.try_reserve_exact(n)?;
    runs(s, |a, b| out.push(&s[a..b]));
    Ok(out)
}

/// `strconv.Atoi` and `strconv.ParseInt(s, 10, 64)` on 64-bit: an optional sign and decimal
/// digits.
pub(crate) fn atoi(s: &[u8]) -> Result<i64, Error> {
    let (neg, digits) = match s.first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    let syntax = || Error::Corrupt("strconv.Atoi: invalid syntax".to_string());
    if digits.is_empty() {
        return Err(syntax());
    }
    let mut n: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return Err(syntax());
        }
        n = n
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(c - b'0')))
            .ok_or_else(|| Error::Corrupt("strconv.Atoi: value out of range".to_string()))?;
    }
    const CUTOFF: u64 = 1 << 63;
    let range = || Error::Corrupt("strconv.Atoi: value out of range".to_string());
    if !neg {
        return i64::try_from(n).map_err(|_| range());
    }
    if n > CUTOFF {
        return Err(range());
    }
    Ok((n as i64).wrapping_neg())
}

// ---------------------------------------------------------------------------------------------
// Memory

/// A byte buffer whose growth is charged to the ledger (the object buffers and the repair
/// scanner's accumulators). The charge is released when it drops.
#[derive(Debug)]
pub struct ChargedBuf {
    pub v: Vec<u8>,
    held: Held,
}

impl ChargedBuf {
    /// An empty buffer.
    ///
    /// # Errors
    /// [`Error::Alloc`] if the ledger refuses the (empty) charge.
    pub fn new(mem: &Mem) -> Result<ChargedBuf, Error> {
        Ok(ChargedBuf {
            v: Vec::new(),
            held: mem.charge(0)?,
        })
    }

    /// Makes room for `n` more bytes. The capacity doubles as `Vec`'s would, but the added
    /// capacity is charged before it is allocated, so the spare half of a doubled buffer is on
    /// the ledger too.
    ///
    /// # Errors
    /// As [`ChargedBuf::extend`].
    fn reserve(&mut self, n: usize) -> Result<(), Error> {
        let (len, cap) = (self.v.len(), self.v.capacity());
        if cap - len >= n {
            return Ok(());
        }
        let want = len.saturating_add(n).max(cap.saturating_mul(2));
        self.held.grow_by((want - cap) as u64)?;
        self.v.try_reserve_exact(want - len)?;
        Ok(())
    }

    /// Appends `s`, charging it first.
    ///
    /// # Errors
    /// [`Error::Alloc`] when the charge goes over `max_alloc`, [`Error::OutOfMemory`] when the
    /// allocator refuses.
    pub fn extend(&mut self, s: &[u8]) -> Result<(), Error> {
        self.reserve(s.len())?;
        self.v.extend_from_slice(s);
        Ok(())
    }

    /// Appends one byte.
    ///
    /// # Errors
    /// The errors of [`ChargedBuf::extend`].
    pub fn push(&mut self, b: u8) -> Result<(), Error> {
        self.extend(&[b])
    }

    /// Empties the buffer, frees it and releases its charge.
    pub fn clear(&mut self) {
        self.v = Vec::new();
        self.held.shrink_to(0);
    }
}

// ---------------------------------------------------------------------------------------------
// Reading

/// `newPositionedReader`: a reader positioned at an offset of the input.
#[derive(Debug, Clone, Copy)]
pub struct PosReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PosReader<'a> {
    /// `Seek(offset, io.SeekStart)`. A position past the end is allowed; reads then find the end.
    ///
    /// # Errors
    /// A negative offset.
    pub fn at(data: &'a [u8], offset: i64) -> Result<PosReader<'a>, Error> {
        let pos = usize::try_from(offset)
            .map_err(|_| Error::Corrupt("seek: negative position".to_string()))?;
        Ok(PosReader {
            data,
            pos: pos.min(data.len()),
        })
    }

    /// How many bytes have been read from the start of the input.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Everything that has not been read yet.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Reads up to `n` bytes.
    pub fn take(&mut self, n: usize) -> &'a [u8] {
        let end = self.pos + n.min(self.data.len() - self.pos);
        let s = &self.data[self.pos..end];
        self.pos = end;
        s
    }

    /// `fillBuffer` for a buffer of `n` bytes: the bytes read; at the end of the input after
    /// some bytes it is not an error, with none it is `io.EOF`.
    ///
    /// # Errors
    /// [`Sentinel::Eof`] when no byte is left and `n` is not 0.
    pub fn fill(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let s = self.take(n);
        if s.is_empty() && n > 0 {
            return Err(Error::Sentinel(Sentinel::Eof));
        }
        Ok(s)
    }
}

/// The Go `context.Context` of the read path.
#[derive(Debug)]
pub struct ReadCx<'a> {
    /// The input (`ctx.Read.RS`).
    pub data: &'a [u8],
    /// `c.Err()` is [`Error::Cancelled`] once this is set.
    pub cancel: &'a AtomicBool,
    /// `objectReadBudgetState.used`.
    pub budget_used: i64,
    /// `objectReadBudgetState.limit`.
    pub budget_limit: i64,
    /// `objectParseKey`: the objects that `parse_object_at` is parsing.
    pub in_parse: BTreeSet<i64>,
}

impl<'a> ReadCx<'a> {
    /// A fresh context with an object read budget of `budget_limit`.
    pub fn new(data: &'a [u8], cancel: &'a AtomicBool, budget_limit: i64) -> ReadCx<'a> {
        ReadCx {
            data,
            cancel,
            budget_used: 0,
            budget_limit,
            in_parse: BTreeSet::new(),
        }
    }

    /// `c.Err()`.
    ///
    /// # Errors
    /// [`Error::Cancelled`] when the cancel flag is set.
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The read context of `ctx`; Go dereferences a nil one and panics.
pub(crate) fn read_info(ctx: &mut Context) -> Result<&mut ReadInfo, Error> {
    ctx.read
        .as_mut()
        .ok_or(Error::Panic("missing read context"))
}

// ---------------------------------------------------------------------------------------------
// Read

/// `hasPostScriptHeader`.
pub fn has_post_script_header(buf: &[u8]) -> bool {
    let buf = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(buf);
    let buf = trim_left(buf, b"\x00\t\n\x0c\r ");
    if !buf.starts_with(b"%!") {
        return false;
    }
    let i = find_any(buf, b"\x0A\x0D").unwrap_or(buf.len());
    contains(&buf[..i], b"PS-Adobe")
}

/// `Read`: parses `data` into a [`Context`] holding its cross-reference table, with every
/// object loaded. The cancel flag interrupts the read.
///
/// # Errors
/// [`err_empty_input`] for no input, the header, cross-reference and object errors, the limit
/// errors of [`Config`], [`Error::Cancelled`].
pub fn read(data: &[u8], conf: &Config, cancel: &AtomicBool) -> Result<Context, Error> {
    read_with_mem(data, conf, &Mem::new(&conf.limits), cancel)
}

/// As [`read`], charging the caller's ledger `mem` (shared with other documents of one call)
/// instead of a fresh one of `conf.limits.max_alloc` bytes.
///
/// # Errors
/// As [`read`]; [`Error::Alloc`] when `mem` has too little left.
pub fn read_with_mem(
    data: &[u8],
    conf: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let file_size = u64::try_from(data.len()).map_err(|_| Error::OutOfMemory)?;
    let mut ctx = Context::new_with_mem(file_size, *conf, mem.clone())?;
    if ctx.read_file_size() == 0 {
        return Err(err_empty_input());
    }
    // Share one parsing budget across xref construction and object loading.
    let budget = object_read_budget_limit(&ctx.xref.conf.limits, data.len());
    let mut rcx = ReadCx::new(data, cancel, budget);

    // Populate xRefTable.
    repair::read_xref_table(&mut rcx, &mut ctx).map_err(|e| e.context("xref table"))?;

    // Make all objects explicitly available (load into memory) in corresponding xRefTable
    // entries. Also decode any involved object streams.
    deref::dereference_xref_table(&mut rcx, &mut ctx)?;

    // Some PDFWriters write an incorrect Size into trailer.
    ctx.xref.size = Some(ctx.xref.max_obj_nr.saturating_add(1));

    Ok(ctx)
}

// ---------------------------------------------------------------------------------------------
// The object read budget

/// `objectBufferLimit`: `MaxObjectBytes`, 0 resolved to the default.
pub fn object_buffer_limit(limits: &Limits) -> i64 {
    lim(limits.object_bytes_limit())
}

/// `objectReadBudgetLimit`: the object buffer limit plus 8 times the input.
pub fn object_read_budget_limit(limits: &Limits, file_size: usize) -> i64 {
    let limit = object_buffer_limit(limits);
    let fs = to_i64(file_size);
    if fs > (i64::MAX - limit) / OBJECT_READ_BUDGET_FACTOR {
        return i64::MAX;
    }
    limit + OBJECT_READ_BUDGET_FACTOR * fs
}

/// `objectExtent`: how much of an object buffer belongs to the object.
pub fn object_extent(buf_len: usize, end_ind: i64, stream_ind: i64, stream_offset: i64) -> i64 {
    let blen = to_i64(buf_len);
    if end_ind >= 0 && (stream_ind < 0 || stream_ind > end_ind) {
        return end_ind.saturating_add(6).min(blen);
    }
    if stream_ind >= 0 {
        return stream_offset.min(blen);
    }
    blen
}

/// `chargeObjectRead`.
///
/// # Errors
/// [`Sentinel::ObjectReadBudget`].
pub fn charge_object_read(rcx: &mut ReadCx<'_>, n: i64) -> Result<(), Error> {
    if n > rcx.budget_limit.saturating_sub(rcx.budget_used) {
        return Err(Error::Sentinel(Sentinel::ObjectReadBudget).context(format!(
            "object parsing exceeded {}-byte budget for {}-byte input ({} counted, {} more)",
            rcx.budget_limit,
            rcx.data.len(),
            rcx.budget_used,
            n
        )));
    }
    rcx.budget_used = rcx.budget_used.saturating_add(n);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The object buffer

/// `newRepairScanner`: a line scanner (a CR and an LF are each a line end) over the rest of
/// `rd` whose lines are bounded by the object buffer limit.
pub fn new_repair_scanner<'a>(limits: &Limits, rd: PosReader<'a>) -> Scanner<'a> {
    let limit = usize::try_from(object_buffer_limit(limits).max(0)).unwrap_or(usize::MAX);
    // Scanner can use the full initial capacity even when it exceeds the configured maximum.
    Scanner::with_buffer(
        rd.rest(),
        lines_single_eol,
        DEFAULT_BUF_SIZE.min(limit),
        limit,
    )
}

/// `growBufBy`: appends `size` bytes to `buf`, filled from `rd`. A short read leaves zero bytes
/// at the end (Go discards the byte count).
///
/// # Errors
/// `io.EOF` ([`Sentinel::Eof`]) when `rd` has nothing left, the ledger.
pub fn grow_buf_by(buf: &mut ChargedBuf, size: usize, rd: &mut PosReader<'_>) -> Result<(), Error> {
    let got = rd.fill(size)?;
    buf.extend(got)?;
    let pad = size - got.len();
    if pad > 0 {
        buf.reserve(pad)?;
        buf.v.resize(buf.v.len() + pad, 0);
    }
    Ok(())
}

/// `growObjectBuffer`: [`grow_buf_by`] within `max_object_bytes`.
///
/// # Errors
/// The object buffer limit: [`LimitKind::ObjectBytes`](crate::LimitKind::ObjectBytes) when the
/// limit was set away from its default, else [`Sentinel::ObjectBufferLimit`].
pub fn grow_object_buffer(
    buf: &mut ChargedBuf,
    grow_size: usize,
    rd: &mut PosReader<'_>,
    max_object_bytes: i64,
) -> Result<(), Error> {
    let remaining = max_object_bytes.saturating_sub(to_i64(buf.v.len()));
    if remaining <= 0 {
        let msg = format!("maximum length {max_object_bytes}");
        if max_object_bytes != lim(DEFAULT_MAX_OBJECT_BYTES) {
            // A caller-set limit is a limit error; the built-in default keeps skipping the
            // object as before.
            return Err(Error::Limit(crate::LimitKind::ObjectBytes).context(msg));
        }
        return Err(Error::Sentinel(Sentinel::ObjectBufferLimit).context(msg));
    }
    let size = grow_size.min(usize::try_from(remaining).unwrap_or(usize::MAX));
    grow_buf_by(buf, size, rd)
}

/// `nextStreamOffset`: where the stream data begins, after the `stream` keyword at
/// `stream_ind` and its end of line.
///
/// # Errors
/// [`Sentinel::TruncatedStreamMarker`] when the line ends before the end of line, and a corrupt
/// marker.
pub fn next_stream_offset(line: &[u8], stream_ind: i64) -> Result<usize, Error> {
    const MARKER: &[u8] = b"stream";
    let corrupt = || Error::Corrupt("corrupt stream marker".to_string());
    let Ok(si) = usize::try_from(stream_ind) else {
        return Err(corrupt());
    };
    if line.len() < MARKER.len() || si > line.len() - MARKER.len() || &line[si..si + 6] != MARKER {
        return Err(corrupt());
    }
    let mut off = si + MARKER.len();

    // Relaxed compatibility: tolerate optional ASCII spaces before the required EOL.
    while off < line.len() && line[off] == 0x20 {
        off += 1;
    }
    if off == line.len() {
        return Err(Error::Sentinel(Sentinel::TruncatedStreamMarker));
    }

    // Skip 0A eol.
    if line[off] == b'\n' {
        return Ok(off + 1);
    }

    // Skip 0D eol.
    if line[off] == b'\r' {
        off += 1;
        // Skip 0D0A eol.
        if off < line.len() && line[off] == b'\n' {
            off += 1;
        }
    }

    Ok(off)
}

/// `lastStreamMarker`: moves `stream_ind` to the next `stream` keyword of `line`, or to -1.
pub fn last_stream_marker(stream_ind: &mut i64, end_ind: i64, line: &[u8]) {
    let Some(limit) = line.len().checked_sub(6) else {
        *stream_ind = -1;
        return;
    };
    let si = ix(*stream_ind);
    if si > limit {
        // No space for another stream marker.
        *stream_ind = -1;
        return;
    }

    // We start searching after this stream marker.
    let bufpos = si + 6;

    // Search for next stream marker.
    let Some(i) = find(&line[bufpos..], b"stream") else {
        // No stream marker within line buffer.
        *stream_ind = -1;
        return;
    };

    // We found the next stream marker.
    *stream_ind += to_i64(6 + i);

    if end_ind > 0 && *stream_ind > end_ind {
        // We found a stream marker of another object.
        *stream_ind = -1;
    }
}

/// `streamOffsetForBuffer`: the offset of the stream data, growing the buffer while the marker
/// is truncated.
fn stream_offset_for_buffer(
    buf: &mut ChargedBuf,
    stream_ind: i64,
    rd: &mut PosReader<'_>,
    max_object_bytes: i64,
) -> Result<i64, Error> {
    loop {
        match next_stream_offset(&buf.v, stream_ind) {
            Ok(off) => return Ok(to_i64(off)),
            Err(e) if e.is(Sentinel::TruncatedStreamMarker) => {}
            Err(e) => return Err(e),
        }
        grow_object_buffer(buf, DEFAULT_BUF_SIZE, rd, max_object_bytes)?;
    }
}

/// `keywordStreamRightAfterEndOfDict`: true if `stream` follows the end of the dict, as in
/// `>>{whitespace}stream`.
pub fn keyword_stream_right_after_end_of_dict(buf: &[u8], stream_ind: i64) -> bool {
    // Get a slice of the chunk right in front of 'stream'.
    let b = &buf[..ix(stream_ind).min(buf.len())];

    // Look for last end of dict marker.
    let Some(eod) = rfind(b, b">>") else {
        // No end of dict in buf.
        return false;
    };

    // We found the last >>. Return true if after end of dict only whitespace.
    go_trim_space(&b[eod..]) == b">>"
}

/// What `buffer` returns: the buffer and where its keywords are.
#[derive(Debug)]
pub struct Buffered {
    pub buf: ChargedBuf,
    /// Where `endobj` is, -1 if absent.
    pub end_ind: i64,
    /// Where `stream` is, -1 if absent.
    pub stream_ind: i64,
    /// The offset where the stream data begins, right after the end of line after `stream`.
    pub stream_offset: i64,
}

/// `buffer`: reads from `rd` until the buffer holds a whole object without its stream data
/// (`# gen obj ... obj dict ... {stream ... data ... endstream} ... endobj`).
///
/// # Errors
/// The object buffer limit, `io.EOF` ([`Sentinel::Eof`]), the keyword scan, the ledger,
/// [`Error::Cancelled`].
pub fn buffer(
    rcx: &ReadCx<'_>,
    mem: &Mem,
    rd: &mut PosReader<'_>,
    max_object_bytes: i64,
) -> Result<Buffered, Error> {
    let mut buf = ChargedBuf::new(mem)?;
    let (mut end_ind, mut stream_ind, mut stream_offset) = (-1i64, -1i64, 0i64);
    let mut grow_size = DEFAULT_BUF_SIZE;

    while end_ind < 0 && stream_ind < 0 {
        rcx.check()?;

        grow_object_buffer(&mut buf, grow_size, rd, max_object_bytes)?;
        grow_size = (grow_size * 2).min(MAX_BUF_SIZE);

        (end_ind, stream_ind) = detect_keywords(rcx.cancel, &buf.v)?;

        if end_ind > 0 && (stream_ind < 0 || stream_ind > end_ind) {
            // No stream marker in buf detected.
            break;
        }

        // For very rare cases where "stream" also occurs within obj dict we need to find the
        // last "stream" marker before a possible end marker.
        while stream_ind > 0 && !keyword_stream_right_after_end_of_dict(&buf.v, stream_ind) {
            last_stream_marker(&mut stream_ind, end_ind, &buf.v);
        }

        if stream_ind > 0 {
            // streamOffset ... the offset where the actual stream data begins. It is right
            // after the eol after "stream".
            stream_offset = stream_offset_for_buffer(&mut buf, stream_ind, rd, max_object_bytes)?;
        }
    }

    Ok(Buffered {
        buf,
        end_ind,
        stream_ind,
        stream_offset,
    })
}

/// `bufferedObject`: [`buffer`], charging what it read to the object read budget.
fn buffered_object(
    rcx: &mut ReadCx<'_>,
    mem: &Mem,
    limits: &Limits,
    rd: &mut PosReader<'_>,
) -> Result<Buffered, Error> {
    let start = rd.pos();
    match buffer(rcx, mem, rd, object_buffer_limit(limits)) {
        Err(e) => {
            // Failed reads can be retried in relaxed mode, so charge everything read.
            charge_object_read(rcx, to_i64(rd.pos() - start))?;
            Err(e)
        }
        Ok(b) => {
            // Ignore read-ahead beyond this object's end.
            let extent = object_extent(b.buf.v.len(), b.end_ind, b.stream_ind, b.stream_offset);
            charge_object_read(rcx, extent)?;
            Ok(b)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Objects

/// `parseObjectWithValidationPolicy`: parses the next object of `line`, charging what it
/// parses to `MaxParsedBytes`. The object number Go attributes errors to is not carried.
///
/// # Errors
/// The parse errors.
pub fn parse_object_with_validation_policy(
    rcx: &ReadCx<'_>,
    ctx: &mut Context,
    line: &mut &[u8],
) -> Result<Object, Error> {
    let max_depth = Some(ctx.xref.conf.limits.max_recursion_depth);
    let cancel = rcx.cancel;
    let mut counter = ctx.xref.parsed_charge();
    let mut cx = ParseCtx::with_charge(cancel, &mut counter);
    Ok(parse_object_with_policy(&mut cx, line, 0, max_depth)?.object)
}

/// What `object` returns: the parsed object (`Null` for Go's nil) and where the keywords are.
#[derive(Debug, Clone)]
pub struct ObjOut {
    pub obj: Object,
    pub end_ind: i64,
    pub stream_ind: i64,
    pub stream_offset: i64,
}

/// `object`: reads and parses the indirect object at `offset`, without its stream data.
///
/// # Errors
/// The errors of [`buffer`] (with `end_ind` 0), the object attribute errors, and the parse
/// errors, which come with the real `end_ind`.
pub fn object(rcx: &mut ReadCx<'_>, ctx: &mut Context, offset: i64) -> Result<ObjOut, ObjFail> {
    let mem = ctx.xref.mem().clone();
    let limits = ctx.xref.conf.limits;
    let mut rd = PosReader::at(rcx.data, offset)?;

    // process: # gen obj ... obj dict ... {stream ... data ... endstream} endobj
    //                                    streamInd                        endInd
    //                                  -1 if absent                    -1 if absent
    let b = buffered_object(rcx, &mem, &limits, &mut rd)?;
    let (end_ind, stream_ind, stream_offset) = (b.end_ind, b.stream_ind, b.stream_offset);
    let line: &[u8] = &b.buf.v;

    let mut l: &[u8] = if end_ind < 0 {
        // buf: # gen obj ... obj dict ... stream ... data
        // implies we detected no endobj and a stream starting at streamInd.
        // big stream, we parse object until "stream"
        &line[..ix(stream_ind)]
    } else if stream_ind < 0 {
        // buf: # gen obj ... obj dict ... endobj
        // small object w/o stream, parse until "endobj"
        &line[..ix(end_ind)]
    } else if stream_ind < end_ind {
        // buf: # gen obj ... obj dict ... stream ... data ... endstream endobj
        // small stream within buffer, parse until "stream"
        &line[..ix(stream_ind)]
    } else {
        // buf: # gen obj ... obj dict ... endobj # gen obj ... obj dict ... stream
        // small obj w/o stream, parse until "endobj"; the stream in buf belongs to a
        // subsequent object.
        &line[..ix(end_ind)]
    };

    // Parse object number and object generation. They may not match the xref entry's: this is
    // suspicious, but ok if two object numbers point to the same offset and only one of them
    // is used, like for cases where the PDF Writer is MS Word 2013.
    parse_object_attributes(&mut l)?;

    l = go_trim_space(l);
    if l.is_empty() {
        // 7.3.9
        // Specifying the null object as the value of a dictionary entry shall be equivalent to
        // omitting the entry entirely.
        return Ok(ObjOut {
            obj: Object::Null,
            end_ind,
            stream_ind,
            stream_offset,
        });
    }

    let obj = parse_object_with_validation_policy(rcx, ctx, &mut l)
        .map_err(|err| ObjFail { end_ind, err })?;
    Ok(ObjOut {
        obj,
        end_ind,
        stream_ind,
        stream_offset,
    })
}

/// `streamDictForObject`: the stream dict of a dict object that is followed by a stream.
///
/// # Errors
/// A missing stream offset, the filter pipeline errors.
pub fn stream_dict_for_object(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    mut d: Dict,
    obj_nr: i64,
    stream_ind: i64,
    stream_offset: i64,
    offset: i64,
) -> Result<StreamDict, Error> {
    let (stream_length, stream_length_ref) = d.length();

    if stream_ind <= 0 {
        return Err(Error::Corrupt("missing stream offset".to_string())
            .context(format!("stream obj#{obj_nr}")));
    }

    let filter_pipeline = stream::pdf_filter_pipeline(rcx, ctx, &mut d)
        .map_err(|e| e.context(format!("stream obj#{obj_nr} filter pipeline")))?;

    let stream_offset = stream_offset.wrapping_add(offset);

    // We have a stream object.
    Ok(StreamDict::new(
        d,
        stream_offset,
        stream_length,
        stream_length_ref,
        filter_pipeline,
    ))
}

/// `resolveObject`: decrypts the strings of a parsed object, and turns a dict that is followed
/// by a stream into a stream dict.
///
/// # Errors
/// The decryption errors, the errors of [`stream_dict_for_object`].
pub fn resolve_object(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj: Object,
    offset: i64,
    obj_nr: i64,
    gen_nr: i64,
    end_ind: i64,
    stream_ind: i64,
    stream_offset: i64,
) -> Result<Object, Error> {
    match obj {
        Object::Dict(_) | Object::Array(_) | Object::String(_) | Object::Hex(_) => {
            let mut o = obj;
            decrypt_object_strings(&mut ctx.xref, &mut o, obj_nr, gen_nr, rcx.cancel)?;
            let Object::Dict(d) = o else {
                return Ok(o);
            };
            if end_ind >= 0 && (stream_ind < 0 || stream_ind > end_ind) {
                // Dict
                return Ok(Object::Dict(d));
            }
            // StreamDict.
            let sd =
                stream_dict_for_object(rcx, ctx, d, obj_nr, stream_ind, stream_offset, offset)?;
            Ok(Object::Stream(Box::new(sd)))
        }
        other => Ok(other),
    }
}

/// `ParseObject`: parses the object at `offset`. `Null` is Go's nil object, which is also what
/// an object that runs into the end of the input is.
///
/// # Errors
/// A circular reference ([`Sentinel::CircularRef`]), the recursion limit, and the errors of
/// [`object`] and [`resolve_object`].
pub fn parse_object_at(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: i64,
    obj_nr: i64,
    gen_nr: i64,
) -> Result<Object, Error> {
    if rcx.in_parse.contains(&obj_nr) {
        return Err(Error::Sentinel(Sentinel::CircularRef).context(format!("obj#{obj_nr}")));
    }
    let depth = u64::try_from(rcx.in_parse.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    ctx.xref.check_recursion_depth("ParseObject", depth)?;
    rcx.in_parse.insert(obj_nr);
    let r = parse_object_guarded(rcx, ctx, offset, obj_nr, gen_nr);
    rcx.in_parse.remove(&obj_nr);
    r
}

fn parse_object_guarded(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: i64,
    obj_nr: i64,
    gen_nr: i64,
) -> Result<Object, Error> {
    let out = match object(rcx, ctx, offset) {
        Ok(o) => o,
        // Relaxed mode: an object that runs into the end of the input is nil.
        Err(f) if f.err.is(Sentinel::Eof) => return Ok(Object::Null),
        Err(f) => return Err(f.err),
    };
    resolve_object(
        rcx,
        ctx,
        out.obj,
        offset,
        obj_nr,
        gen_nr,
        out.end_ind,
        out.stream_ind,
        out.stream_offset,
    )
}

// ---------------------------------------------------------------------------------------------
// Dereferencing during the read (read.go:2991-3137)

/// `dereferencedObject`: the object of entry `obj_nr`, parsed from the input if it has not
/// been loaded. The result is a copy (it shares stream buffers).
///
/// # Errors
/// [`Sentinel::UnregisteredObject`], [`Sentinel::ReferenceDoesNotExist`] for a free entry,
/// [`Sentinel::NilDereferenced`], the parse errors, and [`Error::Corrupt`] for an entry that
/// holds an object stream or an xref stream.
pub fn dereferenced_object(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
) -> Result<Object, Error> {
    let unregistered =
        || Error::Sentinel(Sentinel::UnregisteredObject).context(format!("object {obj_nr}"));
    let Some(entry) = ctx.xref.find(obj_nr) else {
        return Err(unregistered());
    };

    if entry.compressed {
        objstm::decompress_xref_table_entry(&mut ctx.xref, obj_nr)?;
    }
    let Some(entry) = ctx.xref.find(obj_nr) else {
        return Err(unregistered());
    };

    enum State {
        Unloaded {
            free: bool,
            offset: Option<i64>,
            generation: Option<i64>,
        },
        Lazy,
        Loaded(Object),
        Container,
    }
    let state = match &entry.object {
        None => State::Unloaded {
            free: entry.free,
            offset: entry.offset,
            generation: entry.generation,
        },
        Some(EntryObject::Lazy(_)) => State::Lazy,
        Some(EntryObject::Obj(o)) => State::Loaded(o.clone()),
        Some(_) => State::Container,
    };

    match state {
        State::Unloaded {
            free,
            offset,
            generation,
        } => {
            if free {
                return Err(Error::Sentinel(Sentinel::ReferenceDoesNotExist));
            }
            let offset = offset.ok_or(Error::Panic("nil xref entry offset"))?;
            let generation = generation.ok_or(Error::Panic("nil xref entry generation"))?;
            let o = parse_object_at(rcx, ctx, offset, obj_nr, generation)
                .map_err(|e| e.context(format!("problem dereferencing object {obj_nr}")))?;
            if o.is_null() {
                return Err(
                    Error::Sentinel(Sentinel::NilDereferenced).context(format!("object {obj_nr}"))
                );
            }
            if let Some(e) = ctx.xref.entry_mut(obj_nr) {
                e.object = Some(EntryObject::Obj(o.clone()));
            }
            Ok(o)
        }
        State::Lazy => {
            ctx.xref
                .decode_lazy_entry(obj_nr, rcx.cancel)
                .map_err(|e| e.context(format!("problem dereferencing object {obj_nr}")))?;
            match ctx.xref.find(obj_nr).and_then(|e| e.object.as_ref()) {
                Some(EntryObject::Obj(o)) => Ok(o.clone()),
                _ => Err(Error::Corrupt(format!("object {obj_nr} is not loaded"))),
            }
        }
        State::Loaded(o) => Ok(o),
        State::Container => Err(Error::Corrupt(format!(
            "object {obj_nr} is an object stream or an xref stream"
        ))),
    }
}

/// `dereferencedInteger`.
///
/// # Errors
/// The errors of [`dereferenced_object`], and a corrupt integer object.
pub fn dereferenced_integer(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
) -> Result<i64, Error> {
    match dereferenced_object(rcx, ctx, obj_nr)? {
        Object::Integer(i) => Ok(i),
        _ => Err(Error::Corrupt("corrupt integer object".to_string())
            .context(format!("object {obj_nr}"))),
    }
}

/// `dereferencedDict`.
///
/// # Errors
/// The errors of [`dereferenced_object`], and a corrupt dict object.
pub fn dereferenced_dict(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
) -> Result<Dict, Error> {
    match dereferenced_object(rcx, ctx, obj_nr)? {
        Object::Dict(d) => Ok(d),
        _ => {
            Err(Error::Corrupt("corrupt dict object".to_string())
                .context(format!("object {obj_nr}")))
        }
    }
}

/// `bootstrapIntegerEntry`: the integer entry `key` of `d`, which may be an indirect reference
/// to an uncompressed integer object (object streams are not available yet), and whether the
/// key was found.
///
/// # Errors
/// An entry that is not an integer or a reference to one, a missing or compressed target.
pub fn bootstrap_integer_entry(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    d: &Dict,
    key: &[u8],
) -> Result<(Option<i64>, bool), Error> {
    let name = String::from_utf8_lossy(key).into_owned();
    let o = match d.find(key) {
        None => return Ok((None, false)),
        Some(Object::Null) => return Ok((None, true)),
        Some(o) => o,
    };

    let ind_ref = match o {
        Object::Integer(i) => return Ok((Some(*i), true)),
        Object::Ref(r) => *r,
        other => {
            return Err(Error::Corrupt(format!(
                "entry={name}: expected integer, got {}",
                crate::model::deref::type_name(other)
            )));
        }
    };

    let obj_nr = ind_ref.num;
    let attributed = |e: Error| e.context(format!("obj#{obj_nr}"));
    let Some(entry) = ctx.xref.find_table_entry_for_ind_ref(ind_ref) else {
        return Err(attributed(Error::Corrupt(format!(
            "entry={name}: missing indirect target"
        ))));
    };
    if entry.free {
        return Err(attributed(Error::Corrupt(format!(
            "entry={name}: missing indirect target"
        ))));
    }
    // Bootstrap values cannot come from object streams because those streams are not available
    // yet.
    if entry.compressed {
        return Err(attributed(Error::Corrupt(format!(
            "entry={name}: indirect target is compressed during bootstrap"
        ))));
    }
    if entry.object.is_none() && entry.offset.is_none() {
        return Ok((None, true));
    }

    match dereferenced_integer(rcx, ctx, obj_nr) {
        Ok(i) => Ok((Some(i), true)),
        Err(e) => Err(attributed(e.context(format!("entry={name}")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ctx_for(input: &[u8], limits: Limits) -> Context {
        let conf = Config {
            limits,
            ..Config::default()
        };
        match Context::new(input.len() as u64, conf) {
            Ok(c) => c,
            Err(e) => panic!("context: {e}"),
        }
    }

    fn mem() -> Mem {
        Mem::new(&Limits::default())
    }

    // Port of TestNextStreamOffset.
    #[test]
    fn next_stream_offset_cases() {
        let max_int = i64::MAX;
        let cases: [(&str, &[u8], i64, Result<usize, Option<Sentinel>>); 10] = [
            ("LF", b"stream\nabc", 0, Ok(7)),
            ("CRLF", b"stream   \r\nabc", 0, Ok(11)),
            ("CR", b"stream\r", 0, Ok(7)),
            ("Offset", b"xxstream\nabc", 2, Ok(9)),
            ("NegativeOffset", b"stream\n", -1, Err(None)),
            ("OverflowOffset", b"stream\n", max_int, Err(None)),
            ("WrongMarker", b"xxxxxx\n", 0, Err(None)),
            ("MissingMarker", b"stream", 1, Err(None)),
            (
                "Truncated",
                b"stream",
                0,
                Err(Some(Sentinel::TruncatedStreamMarker)),
            ),
            (
                "TrailingSpaces",
                b"stream   ",
                0,
                Err(Some(Sentinel::TruncatedStreamMarker)),
            ),
        ];
        for (name, line, ind, want) in cases {
            let got = next_stream_offset(line, ind);
            match want {
                Ok(off) => assert_eq!(got, Ok(off), "{name}"),
                Err(Some(s)) => assert!(got.as_ref().is_err_and(|e| e.is(s)), "{name}: {got:?}"),
                Err(None) => assert!(
                    matches!(&got, Err(Error::Corrupt(m)) if m == "corrupt stream marker"),
                    "{name}: {got:?}"
                ),
            }
        }
    }

    fn run_buffer(input: &[u8], limit: i64) -> Result<Buffered, Error> {
        let cancel = AtomicBool::new(false);
        let rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut rd = PosReader::at(input, 0)?;
        buffer(&rcx, &mem(), &mut rd, limit)
    }

    // Port of TestBufferRejectsObjectBeyondLimit.
    #[test]
    fn buffer_rejects_object_beyond_limit() {
        let err = run_buffer(&[b'x'; 32], 16).unwrap_err();
        assert!(err.is(Sentinel::ObjectBufferLimit), "{err}");
        assert!(err.is_limit_exceeded());
    }

    // Port of TestBufferAcceptsMarkersAtLimit.
    #[test]
    fn buffer_accepts_markers_at_limit() {
        for input in [&b"1 0 obj null endobj\n"[..], &b"1 0 obj <<>>stream\n"[..]] {
            let b = run_buffer(input, input.len() as i64).unwrap();
            assert_eq!(b.buf.v, input);
        }
    }

    // Port of TestBufferHonorsCancellation.
    #[test]
    fn buffer_honors_cancellation() {
        let input = [b'x'; 32];
        let cancel = AtomicBool::new(true);
        let rcx = ReadCx::new(&input, &cancel, i64::MAX);
        let mut rd = PosReader::at(&input, 0).unwrap();
        let err = buffer(&rcx, &mem(), &mut rd, 16).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    #[test]
    fn buffer_pads_a_short_read_with_zeros() {
        // Go appends the whole growth step and discards the byte count of the read.
        let input = b"1 0 obj null endobj\n";
        let b = run_buffer(input, 1 << 20).unwrap();
        assert_eq!(b.buf.v.len(), DEFAULT_BUF_SIZE);
        assert_eq!(&b.buf.v[..input.len()], input);
        assert!(b.buf.v[input.len()..].iter().all(|&c| c == 0));
        assert_eq!((b.end_ind, b.stream_ind), (13, -1));
    }

    #[test]
    fn buffer_reports_eof_for_an_empty_rest() {
        let err = run_buffer(b"", 1 << 20).unwrap_err();
        assert!(err.is(Sentinel::Eof));
    }

    // Port of TestObjectHonorsBufferLimit.
    #[test]
    fn object_honors_buffer_limit() {
        let input = b"1 0 obj (bounded object) endobj\n";
        let cancel = AtomicBool::new(false);
        for limit in [16u64, input.len() as u64, 0] {
            let mut ctx = ctx_for(
                input,
                Limits {
                    max_object_bytes: limit,
                    ..Limits::default()
                },
            );
            let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
            let r = object(&mut rcx, &mut ctx, 0);
            if limit == 16 {
                let f = r.unwrap_err();
                assert!(
                    f.err.is(Sentinel::ObjectBufferLimit),
                    "limit {limit}: {}",
                    f.err
                );
                assert!(f.err.is_limit_exceeded());
            } else {
                let o = r.unwrap();
                assert!(matches!(o.obj, Object::String(ref s) if s == b"bounded object"));
                assert_eq!((o.end_ind, o.stream_ind), (25, -1));
            }
        }
    }

    #[test]
    fn object_with_a_stream_reports_its_offset() {
        let input = b"4 0 obj\n<</Length 3>>\nstream\nabc\nendstream\nendobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(input, Limits::default());
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let o = object(&mut rcx, &mut ctx, 0).unwrap();
        assert!(matches!(o.obj, Object::Dict(_)));
        assert_eq!(o.stream_ind, 22);
        assert_eq!(o.stream_offset, 29);
        assert!(o.end_ind > o.stream_ind);
    }

    #[test]
    fn object_of_an_empty_body_is_null() {
        let input = b"2 0 obj\nendobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(input, Limits::default());
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let o = object(&mut rcx, &mut ctx, 0).unwrap();
        assert!(o.obj.is_null());
        assert_eq!(o.end_ind, 8);
    }

    #[test]
    fn object_past_the_end_is_eof() {
        let input = b"1 0 obj null endobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(input, Limits::default());
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let f = object(&mut rcx, &mut ctx, 1000).unwrap_err();
        assert!(f.err.is(Sentinel::Eof));
        // ParseObject turns it into a nil object.
        assert!(
            parse_object_at(&mut rcx, &mut ctx, 1000, 1, 0)
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn parse_object_at_rejects_a_circular_parse() {
        let input = b"1 0 obj null endobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(input, Limits::default());
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        rcx.in_parse.insert(1);
        let err = parse_object_at(&mut rcx, &mut ctx, 0, 1, 0).unwrap_err();
        assert!(err.is(Sentinel::CircularRef));
    }

    #[test]
    fn object_read_budget_is_charged_and_enforced() {
        let input = b"1 0 obj (bounded object) endobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(input, Limits::default());
        // The object is 32 bytes; a budget of 8 cannot take it.
        let mut rcx = ReadCx::new(input, &cancel, 8);
        let f = object(&mut rcx, &mut ctx, 0).unwrap_err();
        assert!(f.err.is(Sentinel::ObjectReadBudget), "{}", f.err);
        let mut rcx = ReadCx::new(input, &cancel, 1 << 20);
        object(&mut rcx, &mut ctx, 0).unwrap();
        assert_eq!(rcx.budget_used, 31);
    }

    #[test]
    fn object_extent_cases() {
        assert_eq!(object_extent(100, 20, -1, 0), 26);
        assert_eq!(object_extent(22, 20, -1, 0), 22);
        assert_eq!(object_extent(100, 50, 20, 30), 30);
        assert_eq!(object_extent(100, -1, 20, 130), 100);
        assert_eq!(object_extent(100, -1, -1, 0), 100);
    }

    #[test]
    fn object_read_budget_limit_cases() {
        let l = Limits::default();
        assert_eq!(object_read_budget_limit(&l, 10), (64 << 20) + 80);
        assert_eq!(object_read_budget_limit(&l, usize::MAX >> 1), i64::MAX);
    }

    #[test]
    fn stream_marker_search() {
        let line = b"<</A (stream) >>\nstream\n";
        let mut ind = 6;
        // The first marker is inside a string, not right after the end of the dict.
        assert!(!keyword_stream_right_after_end_of_dict(line, ind));
        last_stream_marker(&mut ind, -1, line);
        assert_eq!(ind, 17);
        assert!(keyword_stream_right_after_end_of_dict(line, ind));
        last_stream_marker(&mut ind, -1, line);
        assert_eq!(ind, -1);
        // A marker past the end marker belongs to another object.
        let mut ind = 0;
        last_stream_marker(&mut ind, 3, b"stream stream");
        assert_eq!(ind, -1);
    }

    #[test]
    fn post_script_header() {
        assert!(has_post_script_header(
            b"%!PS-Adobe-3.0\n%%Title: report.txt\n"
        ));
        assert!(has_post_script_header(b"\xef\xbb\xbf \r\n%!PS-Adobe-3.0\n"));
        assert!(!has_post_script_header(b"%PDF-1.7\n"));
        assert!(!has_post_script_header(b"%!PS\n"));
    }

    #[test]
    fn byte_helpers_follow_go() {
        assert_eq!(go_trim_space(b" \t\r\nabc \x0b"), b"abc");
        assert_eq!(go_trim_space("\u{a0}x\u{85}".as_bytes()), b"x");
        assert_eq!(go_trim_space(b"\xffx\xff"), b"\xffx\xff");
        let f = go_fields(b" 0000000016 00000 n \r").unwrap();
        assert_eq!(f, vec![&b"0000000016"[..], b"00000", b"n"]);
        assert!(go_fields(b"   ").unwrap().is_empty());
        assert_eq!(atoi(b"+12"), Ok(12));
        assert_eq!(atoi(b"-9223372036854775808"), Ok(i64::MIN));
        assert!(atoi(b"9223372036854775808").is_err());
        assert!(atoi(b"").is_err());
        assert!(atoi(b"1_0").is_err());
        assert_eq!(rfind(b"abcabc", b"bc"), Some(4));
        assert_eq!(find(b"abc", b"x"), None);
        assert_eq!(trim_left(b"  a ", b" "), b"a ");
        assert_eq!(trim_right(b"a%%", b"%"), b"a");
    }

    #[test]
    fn pos_reader_reads_like_fill_buffer() {
        let data = b"abcdef";
        let mut rd = PosReader::at(data, 2).unwrap();
        assert_eq!(rd.fill(3).unwrap(), b"cde");
        assert_eq!(rd.fill(3).unwrap(), b"f");
        assert!(rd.fill(3).unwrap_err().is(Sentinel::Eof));
        assert!(PosReader::at(data, -1).is_err());
        assert_eq!(PosReader::at(data, 100).unwrap().rest(), b"");
    }

    #[test]
    fn charged_buf_charges_and_releases() {
        let m = Mem::with_max(10);
        let mut b = ChargedBuf::new(&m).unwrap();
        b.extend(b"12345").unwrap();
        assert_eq!(m.used(), 5);
        assert!(b.extend(b"1234567").is_err());
        b.clear();
        assert_eq!(m.used(), 0);
    }

    #[test]
    fn read_of_nothing_is_empty_input() {
        let cancel = AtomicBool::new(false);
        let err = read(b"", &Config::default(), &cancel).unwrap_err();
        assert_eq!(err, err_empty_input());
        let cancelled = AtomicBool::new(true);
        let err = read(b"%PDF-1.7\n", &Config::default(), &cancelled).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }
}
