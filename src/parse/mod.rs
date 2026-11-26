//! The object parser (Go: pkg/pdfcpu/model/parse.go).
//!
//! The parser works on byte slices: every function takes the rest of the input as
//! `&mut &[u8]` and moves it forward, as Go's parser moves its `*string`. Go slices the string
//! rather than copying it; so does this. Strings and names are `Vec<u8>` and are not required to
//! be UTF-8.
//!
//! * [`parse_object`] and [`parse_object_with_policy`] parse one object. The first pass is
//!   strict about dictionary keys; if it fails, a second pass ("repair") skips junk keys and
//!   accepts a key with no value when the line ends. Both run in relaxed validation mode, the
//!   only mode byblos uses (docs/design.md section 6), so Go's `validationMode` parameter is
//!   gone: a duplicate key keeps the later non-null value and a keyword in the wrong case is
//!   accepted.
//! * [`parse_object_attributes`] reads `<num> <gen> obj`. [`detect_keywords`] finds the `endobj`
//!   and `stream` keywords of an object buffer.
//! * [`parse_xref_stream_dict_with_limits`], [`parse_xref_stream_dict_relaxed_with_limits`],
//!   [`object_stream_dict_with_limits`] and [`object_stream_dict_with_resolved_integers`] build
//!   the two stream variants from a [`StreamDict`].
//! * [`scan`] has the line splitters and the `bufio.Scanner` emulation.
//!
//! The helpers that the content-stream parser shares ([`trim_left_space`], [`parse_name`],
//! [`position_to_next_whitespace_or_char`], and so on) are public for it.
//!
//! Go's context becomes [`ParseCtx`]: the cancel flag (`c.Err()`) and, optionally, the counter
//! of the estimated heap of what is parsed (`WithParsedCharge`, `chargeParsed`).
//!
//! Differences from Go, all on paths Go never takes with its defaults:
//!
//! * Nesting is also capped at [`MAX_NESTING`] levels, whatever `max_depth` says, because the
//!   parser recurses on the native stack and Go's stacks grow. Over it the error is the same
//!   recursion-depth error.
//! * The messages of Go's `fmt.Errorf` errors keep their text, but carry no document bytes.
//! * Go's `Log`, `ShowDigestedSpecViolation` and validation notices are not ported.

pub mod content;
pub mod scan;

use crate::config::Limits;
use crate::error::{Error, LimitKind, Sentinel};
use crate::ledger::Held;
use crate::object::string::decode_name;
use crate::object::{Dict, ObjectStreamDict, StreamDict, XRefStreamDict};
use crate::object::{Object, Ref};
use std::sync::atomic::{AtomicBool, Ordering};

/// The most levels of nesting the parser recurses through, whatever the configured limit is.
pub const MAX_NESTING: u64 = 256;

/// `DefaultResourceLimits().MaxRecursionDepth`: the depth limit when none is given.
const DEFAULT_MAX_DEPTH: u64 = 100;

// The estimated heap per parsed array element, dictionary entry and dictionary
// (xreftable.go:2198-2202).
const PARSED_ARRAY_ELEM_BYTES: i64 = 64;
const PARSED_DICT_ENTRY_BYTES: i64 = 96;
const PARSED_DICT_BYTES: i64 = 256;

/// The ledger's estimate of a dictionary entry's heap, besides its key bytes: the `BTreeMap` slot
/// of its key and value twice over (keys in file order leave the nodes about half full), and 16
/// for the internal nodes and the key's allocation. Only the ledger gets the excess over Go's
/// [`PARSED_DICT_ENTRY_BYTES`], so `max_parsed_bytes` counts as Go does.
const DICT_ENTRY_HEAP: i64 = 2 * (size_of::<Vec<u8>>() + size_of::<Object>()) as i64 + 16;

const EMPTY: &[u8] = &[];

/// `posUnsearched`: a position that needs a new search, as opposed to -1 for "none".
const POS_UNSEARCHED: i64 = -2;

/// The text of `ErrDictionaryCorrupt`.
pub const ERR_DICTIONARY_CORRUPT: &str = "parse: corrupt dictionary";
/// The text of `errDictionaryNotTerminated`, which wraps `ErrDictionaryCorrupt`.
pub const ERR_DICTIONARY_NOT_TERMINATED: &str =
    "parse: unterminated dictionary: parse: corrupt dictionary";
/// The text of `errArrayNotTerminated`.
pub const ERR_ARRAY_NOT_TERMINATED: &str = "parse: unterminated array";
/// The text of `errStringLiteralCorrupt`.
pub const ERR_STRING_LITERAL_CORRUPT: &str =
    "parse: corrupt string literal, possibly unbalanced parenthesis";
/// The context message of `dictionaryKeyError`.
const KEY_ERROR_MESSAGE: &str = "parse: corrupt dictionary key";

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(msg.to_owned())
}

fn err_buf_not_available() -> Error {
    corrupt("parse: no buffer available")
}

fn err_array_corrupt() -> Error {
    corrupt("parse: corrupt array")
}

fn err_array_not_terminated() -> Error {
    corrupt(ERR_ARRAY_NOT_TERMINATED)
}

fn err_dictionary_corrupt() -> Error {
    corrupt(ERR_DICTIONARY_CORRUPT)
}

fn err_dictionary_not_terminated() -> Error {
    corrupt(ERR_DICTIONARY_NOT_TERMINATED)
}

fn err_hex_literal_corrupt() -> Error {
    corrupt("parse: corrupt hex literal")
}

fn err_hex_literal_not_terminated() -> Error {
    corrupt("parse: hex literal not terminated")
}

fn err_string_literal_corrupt() -> Error {
    corrupt(ERR_STRING_LITERAL_CORRUPT)
}

fn err_name_object_corrupt() -> Error {
    Error::Sentinel(Sentinel::NameObjectCorrupt)
}

fn err_xref_stream_corrupt_w() -> Error {
    corrupt("parse: xref stream dict corrupt entry W: expecting array of 3 int")
}

fn err_xref_stream_corrupt_index() -> Error {
    corrupt("parse: xref stream dict corrupt entry Index")
}

fn err_invalid_size() -> Error {
    corrupt("invalid \"Size\"")
}

/// `errors.Is(err, model.ErrDictionaryCorrupt)`.
pub fn is_dictionary_corrupt(e: &Error) -> bool {
    matches!(e.root(), Error::Corrupt(s)
        if s == ERR_DICTIONARY_CORRUPT || s == ERR_DICTIONARY_NOT_TERMINATED)
}

/// `errors.As(err, &dictionaryKeyError)`: a dictionary key that did not parse, in the first
/// (non-repair) pass.
pub fn is_dictionary_key_error(e: &Error) -> bool {
    let mut cur = e;
    while let Error::Context(msg, inner) = cur {
        if &**msg == KEY_ERROR_MESSAGE {
            return true;
        }
        cur = inner;
    }
    false
}

fn dictionary_key_error(e: Error) -> Error {
    Error::Context(KEY_ERROR_MESSAGE.into(), Box::new(e))
}

/// `CheckRecursionDepth("parse object", depth, limit)`.
fn check_recursion_depth(depth: u64, limit: u64) -> Result<(), Error> {
    if depth > limit {
        return Err(
            Error::Sentinel(Sentinel::MaxRecursionDepth).context(format!(
                "parse object: max recursion depth exceeded (depth {depth}, limit {limit})"
            )),
        );
    }
    Ok(())
}

/// The depth limit of a parse: `maxDepth` (0 counts as the default), capped at [`MAX_NESTING`].
fn effective_depth_limit(max_depth: Option<u64>) -> u64 {
    let d = match max_depth {
        Some(d) if d > 0 => d,
        _ => DEFAULT_MAX_DEPTH,
    };
    d.min(MAX_NESTING)
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A position known to be non-negative, as an index.
fn ix(p: i64) -> usize {
    usize::try_from(p).unwrap_or(0)
}

/// A limit as Go's `int`.
fn lim(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn try_vec(b: &[u8]) -> Result<Vec<u8>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(b.len())?;
    v.extend_from_slice(b);
    Ok(v)
}

/// `strings.Index` for a non-empty needle.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let n = needle.len();
    if hay.len() < n {
        return None;
    }
    let last = hay.len() - n;
    let mut i = 0;
    while i <= last {
        let p = hay[i..=last].iter().position(|&b| b == first)?;
        i += p;
        if &hay[i..i + n] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `strings.IndexByte`, -1 when there is none.
fn index_byte(s: &[u8], c: u8) -> i64 {
    s.iter().position(|&b| b == c).map_or(-1, to_i64)
}

// ---------------------------------------------------------------------------------------------
// Context

/// What the parser reports the estimated heap of its objects to (`chargeParsed`). The table's
/// `MaxParsedBytes` counter implements it.
pub trait ParsedCharge {
    /// Adds `n` bytes to the running total.
    ///
    /// # Errors
    /// [`LimitKind::Parsed`] once the total is over the limit.
    fn charge_parsed(&mut self, n: i64) -> Result<(), Error>;

    /// Charges `n` more bytes to the ledger only, never to the `MaxParsedBytes` total.
    ///
    /// # Errors
    /// [`Error::Alloc`].
    fn charge_ledger(&mut self, _n: i64) -> Result<(), Error> {
        Ok(())
    }
}

/// A stand-alone `MaxParsedBytes` counter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParsedBudget {
    /// The estimated bytes parsed so far.
    pub parsed: i64,
    /// The limit.
    pub max: i64,
}

impl ParsedBudget {
    /// A counter at 0 with the limit `max`.
    pub fn new(max: u64) -> ParsedBudget {
        ParsedBudget {
            parsed: 0,
            max: lim(max),
        }
    }
}

impl ParsedCharge for ParsedBudget {
    fn charge_parsed(&mut self, n: i64) -> Result<(), Error> {
        self.parsed = self.parsed.saturating_add(n);
        if self.parsed > self.max {
            return Err(Error::Limit(LimitKind::Parsed));
        }
        Ok(())
    }
}

/// Go's `context.Context` for the parser: cancellation and the parsed-bytes charge.
pub struct ParseCtx<'a> {
    cancel: &'a AtomicBool,
    parsed: Option<&'a mut dyn ParsedCharge>,
}

impl<'a> ParseCtx<'a> {
    /// A context that only checks `cancel` (no `WithParsedCharge`).
    pub fn new(cancel: &'a AtomicBool) -> ParseCtx<'a> {
        ParseCtx {
            cancel,
            parsed: None,
        }
    }

    /// `WithParsedCharge`: a context that also charges what it parses to `charge`. Go attaches
    /// the charge only when `MaxParsedBytes > 0`; the caller does the same.
    pub fn with_charge(cancel: &'a AtomicBool, charge: &'a mut dyn ParsedCharge) -> ParseCtx<'a> {
        ParseCtx {
            cancel,
            parsed: Some(charge),
        }
    }

    /// `c.Err() != nil`.
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// `contextutil.Check(c)`.
    fn check(&self) -> Result<(), Error> {
        if self.cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// `chargeParsed(c, n)`.
    fn charge(&mut self, n: i64) -> Result<(), Error> {
        match self.parsed.as_mut() {
            Some(c) => c.charge_parsed(n),
            None => Ok(()),
        }
    }

    /// [`ParsedCharge::charge_ledger`].
    fn charge_ledger(&mut self, n: i64) -> Result<(), Error> {
        match self.parsed.as_mut() {
            Some(c) => c.charge_ledger(n),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Lexical helpers

/// `pdfWhitespace`: one of the six PDF whitespace bytes (7.2.3).
pub fn pdf_whitespace(b: u8) -> bool {
    matches!(b, 0x00 | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

/// `delimiter`: one of `<>[]()/`.
pub fn delimiter(b: u8) -> bool {
    matches!(b, b'<' | b'>' | b'[' | b']' | b'(' | b')' | b'/')
}

/// `positionToNextWhitespaceOrChar`: the index of the first PDF whitespace byte or byte of
/// `chars` in `s`, `None` if there is none. With no `chars` it is `positionToNextWhitespace`,
/// which gives 0 for "none".
pub fn position_to_next_whitespace_or_char(s: &[u8], chars: &[u8]) -> Option<usize> {
    if chars.is_empty() {
        return Some(s.iter().position(|&b| pdf_whitespace(b)).unwrap_or(0));
    }
    s.iter()
        .position(|&b| pdf_whitespace(b) || chars.contains(&b))
}

/// `positionToNextEOL`: the index of the first LF or CR in `s`.
pub fn position_to_next_eol(s: &[u8]) -> Option<usize> {
    s.iter().position(|&b| b == b'\n' || b == b'\r')
}

/// `trimLeftSpace`: `s` without its leading whitespace and comments. With `repair`, also
/// whether the whitespace held a line ending. A lone `%` in the last byte is not a comment.
pub fn trim_left_space(s: &[u8], repair: bool) -> (&[u8], bool) {
    let mut s = s;
    let mut eol = false;
    loop {
        let mut i = 0;
        while i < s.len() && pdf_whitespace(s[i]) {
            if repair && (s[i] == b'\n' || s[i] == b'\r') {
                eol = true;
            }
            i += 1;
        }
        s = &s[i..];
        if s.len() <= 1 || s[0] != b'%' {
            break;
        }
        // Trim a PDF comment (`%` up to the end of the line).
        s = match position_to_next_eol(s) {
            Some(i) => &s[i..],
            None => EMPTY,
        };
    }
    (s, eol)
}

/// `balancedParenthesesPrefix`: the index of the `)` that closes the `(` at the start of `s`,
/// `None` if the parentheses are not balanced. A backslash escapes the next byte.
pub fn balanced_parentheses_prefix(s: &[u8]) -> Option<usize> {
    let mut j: i64 = 0;
    let mut escaped = false;
    for (i, &c) in s.iter().enumerate() {
        if !escaped && c == b'\\' {
            escaped = true;
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if c == b'(' {
            j += 1;
        }
        if c == b')' {
            j -= 1;
        }
        if j == 0 {
            return Some(i);
        }
    }
    None
}

/// `forwardParseBuf`: `buf` from `pos` on, empty if `pos` is past the end.
pub fn forward_parse_buf(buf: &[u8], pos: usize) -> &[u8] {
    if pos < buf.len() { &buf[pos..] } else { EMPTY }
}

/// `strings.TrimSpace`: Unicode white space at both ends; bytes that are not valid UTF-8 are
/// not space.
fn go_trim_space(s: &[u8]) -> &[u8] {
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

/// The first char of `s` and its length in bytes, if `s` starts with a valid UTF-8 sequence.
fn first_char(s: &[u8]) -> Option<(char, usize)> {
    let w = match *s.first()? {
        0..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let text = std::str::from_utf8(s.get(..w)?).ok()?;
    text.chars().next().map(|c| (c, w))
}

/// The last char of `s` and its length in bytes, if `s` ends with a valid UTF-8 sequence.
fn last_char(s: &[u8]) -> Option<(char, usize)> {
    let n = s.len();
    let floor = n.saturating_sub(4);
    let mut i = n;
    while i > floor {
        i -= 1;
        if s[i] & 0xC0 != 0x80 {
            let (c, w) = first_char(&s[i..])?;
            return if i + w == n { Some((c, w)) } else { None };
        }
    }
    None
}

/// `hexString`: the hex digits of `s` in upper case, without whitespace and padded with a final
/// 0 to an even length; `None` if `s` holds anything but PDF whitespace and hex digits.
fn hex_string(s: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve(s.len().saturating_add(1))?;
    for &c in s {
        if matches!(c, 0x00 | 0x09 | 0x0A | 0x0C | 0x0D | b' ') {
            // Ignore whitespace within digit pairs (PDF 2.0, 7.3.4.3).
            continue;
        }
        if c.is_ascii_hexdigit() {
            out.push(c.to_ascii_uppercase());
        } else {
            return Ok(None);
        }
    }
    // If the final digit of a hexadecimal string is missing - that is, if there is an odd
    // number of digits - the final digit shall be assumed to be 0.
    if !out.len().is_multiple_of(2) {
        out.push(b'0');
    }
    Ok(Some(out))
}

/// `strconv.Atoi`'s two failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtoiError {
    Syntax,
    Range,
}

/// `strconv.Atoi` on 64-bit `int`: an optional sign and decimal digits. An overflow is reported
/// as soon as the digits so far overflow 64 bits, before a later bad byte is looked at.
fn atoi(s: &[u8]) -> Result<i64, AtoiError> {
    let (neg, digits) = match s.first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return Err(AtoiError::Syntax);
    }
    let mut n: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return Err(AtoiError::Syntax);
        }
        n = n
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(c - b'0')))
            .ok_or(AtoiError::Range)?;
    }
    const CUTOFF: u64 = 1 << 63;
    if !neg {
        if n >= CUTOFF {
            return Err(AtoiError::Range);
        }
        return i64::try_from(n).map_err(|_| AtoiError::Range);
    }
    if n > CUTOFF {
        return Err(AtoiError::Range);
    }
    Ok((n as i64).wrapping_neg())
}

fn atoi_error(e: AtoiError) -> Error {
    match e {
        AtoiError::Syntax => corrupt("strconv.Atoi: invalid syntax"),
        AtoiError::Range => corrupt("strconv.Atoi: value out of range"),
    }
}

// ---------------------------------------------------------------------------------------------
// Object attributes

/// `detectObj`: the text before `obj` (or `bj`) and the text after it. The keyword must not be
/// at the very start.
fn detect_obj(s: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    if let Some(i) = find(s, b"obj").filter(|&i| i > 0) {
        return Ok((&s[..i], &s[i + 3..]));
    }
    if let Some(i) = find(s, b"bj").filter(|&i| i > 0) {
        return Ok((&s[..i], &s[i + 2..]));
    }
    Err(corrupt("parseObjectAttributes: can't find \"obj\""))
}

/// `cleanObjProlog`: the digits and spaces of `s`, with PDF whitespace as a space.
fn clean_obj_prolog(s: &[u8]) -> Result<Vec<u8>, Error> {
    let (s, _) = trim_left_space(s, false);
    if s.is_empty() {
        return Err(corrupt("parseObjectAttributes: can't find object number"));
    }
    let mut b = Vec::new();
    b.try_reserve(s.len())?;
    for &c in s {
        match c {
            b'0'..=b'9' | b' ' => b.push(c),
            // Preserve PDF whitespace as a separator between numbers (7.2.3).
            0x00 | b'\t' | b'\n' | 0x0C | b'\r' => b.push(b' '),
            _ => {}
        }
    }
    Ok(b)
}

/// `ParseObjectAttributes`: the object and generation numbers of the next `<num> <gen> obj`;
/// `line` is moved behind `obj`. On an error `line` is left as it was.
///
/// # Errors
/// No `obj` (an [`Error::Corrupt`]), no number ([`Sentinel::CorruptObjectOffset`] for the
/// object number), or no generation number.
pub fn parse_object_attributes<'s>(line: &mut &'s [u8]) -> Result<(i64, i64), Error> {
    let s: &'s [u8] = line;
    if s.is_empty() {
        return Err(corrupt("buf not available"));
    }
    let (l, remainder) = detect_obj(s)?;
    let l = clean_obj_prolog(l)?;

    // Object number.
    let i = position_to_next_whitespace_or_char(&l, b"%").filter(|&n| n > 0);
    let num_text: &[u8] = match i {
        Some(n) => &l[..n],
        None => &l,
    };
    let obj_nr =
        atoi(num_text.trim_ascii()).map_err(|_| Error::Sentinel(Sentinel::CorruptObjectOffset))?;

    // Generation number.
    let mut gen_nr = 0;
    if let Some(n) = i {
        let (rest, _) = trim_left_space(&l[n..], false);
        if rest.is_empty() {
            return Err(corrupt("can't find generation number"));
        }
        let Some(j) = position_to_next_whitespace_or_char(rest, b"%").filter(|&n| n > 0) else {
            return Err(corrupt("can't find end of generation number"));
        };
        gen_nr = atoi(&rest[..j]).map_err(atoi_error)?;
    }

    *line = remainder;
    Ok((obj_nr, gen_nr))
}

// ---------------------------------------------------------------------------------------------
// Objects

/// `parseObjectValue`'s `parseBooleanOrNull`: the object and the number of bytes of the keyword.
/// The keywords are matched in any case.
fn parse_boolean_or_null(l: &[u8]) -> Option<(Object, usize)> {
    if l.len() < 4 {
        return None;
    }
    let p = &l[..4];
    // null, absent object
    if p.eq_ignore_ascii_case(b"null") {
        return Some((Object::Null, 4));
    }
    // boolean true
    if p.eq_ignore_ascii_case(b"true") {
        return Some((Object::Bool(true), 4));
    }
    if l.len() < 5 {
        return None;
    }
    // boolean false
    if l[..5].eq_ignore_ascii_case(b"false") {
        return Some((Object::Bool(false), 5));
    }
    None
}

/// `parsedScalarBytes`: the heap held by a string, hex string or name.
fn parsed_scalar_bytes(o: &Object) -> i64 {
    match o {
        Object::String(v) | Object::Hex(v) | Object::Name(v) => to_i64(v.len()),
        _ => 0,
    }
}

/// `decodeNameHexSequence`.
fn decode_name_hex_sequence(s: &[u8]) -> Result<Vec<u8>, Error> {
    match decode_name(s) {
        Ok(d) => try_vec(&d),
        Err(_) => Err(err_name_object_corrupt()),
    }
}

/// `parseName`: a name object (7.3.5) with its `#xx` sequences decoded. `line` is moved past the
/// name even when the name does not decode.
///
/// # Errors
/// [`Sentinel::NameObjectCorrupt`] if `line` does not start with `/` or the name does not
/// decode.
pub fn parse_name<'s>(line: &mut &'s [u8]) -> Result<Vec<u8>, Error> {
    let l0: &'s [u8] = line;
    let Some(&first) = l0.first() else {
        return Err(err_buf_not_available());
    };
    if first != b'/' {
        return Err(err_name_object_corrupt());
    }
    // Position behind '/'.
    let l = forward_parse_buf(l0, 1);
    // Cut off on whitespace or delimiter.
    let name = match position_to_next_whitespace_or_char(l, b"/<>()[]%") {
        None => {
            // Name terminated by eol.
            *line = EMPTY;
            l
        }
        Some(eok) => {
            *line = &l[eok..];
            &l[..eok]
        }
    };
    decode_name_hex_sequence(name)
}

/// `parseStringLiteral`: the escaped body of a string literal, without the parentheses.
fn parse_string_literal<'s>(line: &mut &'s [u8]) -> Result<Object, Error> {
    // Balanced pairs of parentheses are allowed. Empty literals are allowed. The body keeps its
    // escape sequences.
    let l: &'s [u8] = line;
    if l.len() < 2 || l[0] != b'(' {
        return Err(err_string_literal_corrupt());
    }
    // The index of the enclosing ')'.
    let Some(i) = balanced_parentheses_prefix(l) else {
        return Err(err_string_literal_corrupt());
    };
    let body = try_vec(&l[1..i])?;
    // Position behind ')'.
    *line = forward_parse_buf(&l[i..], 1);
    Ok(Object::String(body))
}

/// `parseHexLiteral`. A literal with a byte that is not a hex digit is skipped: the result is
/// null.
fn parse_hex_literal<'s>(line: &mut &'s [u8]) -> Result<Object, Error> {
    let l0: &'s [u8] = line;
    if l0.len() < 2 || l0[0] != b'<' {
        return Err(err_hex_literal_corrupt());
    }
    // Position behind '<'.
    let l = forward_parse_buf(l0, 1);
    // End of the hex literal.
    let Some(eov) = l.iter().position(|&b| b == b'>') else {
        return Err(err_hex_literal_not_terminated());
    };
    let hex = hex_string(go_trim_space(&l[..eov]))?;
    // Position behind '>'.
    *line = forward_parse_buf(&l[eov..], 1);
    match hex {
        Some(h) => Ok(Object::Hex(h)),
        // Skip junk.
        None => Ok(Object::Null),
    }
}

/// `parseHexLiteralOrDict`.
fn parse_hex_literal_or_dict<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Object, Error> {
    let l: &'s [u8] = line;
    if l.len() < 2 {
        return Err(err_buf_not_available());
    }
    // If the next char is '<', it is a dictionary.
    if l[1] == b'<' {
        check_recursion_depth(level, depth_limit)?;
        let d = parse_dict(cx, line, level, depth_limit, repair)?;
        return Ok(Object::Dict(d));
    }
    parse_hex_literal(line)
}

/// `parseArray`.
fn parse_array<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Vec<Object>, Error> {
    check_recursion_depth(level, depth_limit)?;
    let l0: &'s [u8] = line;
    if !l0.starts_with(b"[") {
        return Err(err_array_corrupt());
    }
    if l0.len() == 1 {
        return Err(err_array_not_terminated());
    }
    // Position behind '[', then to the first non whitespace char.
    let (mut l, _) = trim_left_space(forward_parse_buf(l0, 1), false);
    if l.is_empty() {
        // Only whitespace after '['.
        return Err(err_array_not_terminated());
    }

    let mut a: Vec<Object> = Vec::new();
    while !l.starts_with(b"]") {
        let obj = parse_object_inner(cx, &mut l, level.saturating_add(1), depth_limit, repair)?;
        cx.charge(PARSED_ARRAY_ELEM_BYTES)?;
        a.try_reserve(1)?;
        a.push(obj);

        // We are positioned on the char behind the last parsed array entry.
        if l.is_empty() {
            return Err(err_array_not_terminated());
        }
        // Position to the next non whitespace char.
        l = trim_left_space(l, false).0;
        if l.is_empty() {
            return Err(err_array_not_terminated());
        }
    }
    // Position behind ']'.
    *line = forward_parse_buf(l, 1);
    Ok(a)
}

/// `insertKey` in relaxed mode: the first value of a key stays unless a later, non-null one
/// replaces it. A null is kept until the dictionary is complete, so that it counts as a key.
fn insert_key(d: &mut Dict, key: Vec<u8>, val: Object) {
    match d.0.get_mut(&key) {
        None => {
            d.0.insert(key, val);
        }
        Some(slot) => {
            if !val.is_null() {
                *slot = val;
            }
        }
    }
}

/// `dictString`: the dictionary goes on.
fn dict_string(l: &[u8]) -> bool {
    !l.is_empty() && !l.starts_with(b">>")
}

/// `processDictKeys`.
fn process_dict_keys<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Dict, Error> {
    let mut l: &'s [u8] = line;
    cx.charge(PARSED_DICT_BYTES)?;
    let mut d = Dict::default();

    while dict_string(l) {
        cx.check()?;

        let key = match parse_name(&mut l) {
            Ok(k) => Some(k),
            Err(e) => {
                if !repair {
                    return Err(dictionary_key_error(e));
                }
                // Skip junk.
                l = forward_parse_buf(l, 1);
                None
            }
        };

        // Position to the first non whitespace after the key.
        let (rest, eol) = trim_left_space(l, repair);
        l = rest;

        let Some(key) = key else {
            // Skip junk.
            continue;
        };

        if l.is_empty() {
            // Only whitespace after the key.
            return Err(err_dictionary_not_terminated());
        }

        let val = if eol {
            // #252: For dicts with kv pairs terminated by eol we accept a missing value as an
            // empty string.
            Object::String(Vec::new())
        } else {
            parse_object_inner(cx, &mut l, level.saturating_add(1), depth_limit, repair)?
        };

        // Specifying the null object as the value of a dictionary entry (7.3.7) shall be
        // equivalent to omitting the entry entirely. Null entries are retained until duplicate
        // keys have been checked.
        cx.charge(PARSED_DICT_ENTRY_BYTES.saturating_add(to_i64(key.len())))?;
        cx.charge_ledger(DICT_ENTRY_HEAP - PARSED_DICT_ENTRY_BYTES)?;
        insert_key(&mut d, key, val);

        // We are positioned on the char behind the last parsed dict value.
        if l.len() < 2 {
            return Err(err_dictionary_not_terminated());
        }
        // Position to the next non whitespace char.
        l = trim_left_space(l, false).0;
        if l.is_empty() {
            return Err(err_dictionary_not_terminated());
        }
    }
    *line = l;
    Ok(d)
}

/// `parseDict`.
fn parse_dict<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Dict, Error> {
    let l0: &'s [u8] = line;
    if l0.len() < 4 || !l0.starts_with(b"<<") {
        return Err(err_dictionary_corrupt());
    }
    // Position behind '<<', then to the first non whitespace char.
    let (mut l, _) = trim_left_space(forward_parse_buf(l0, 2), false);
    if l.is_empty() {
        return Err(err_dictionary_not_terminated());
    }

    let mut d = process_dict_keys(cx, &mut l, level, depth_limit, repair)?;
    d.0.retain(|_, v| !v.is_null());

    // Position behind '>>'.
    *line = forward_parse_buf(l, 2);
    Ok(d)
}

/// `startParseNumericOrIndRef`: the token (with a prefix of zeros before a sign dropped), the
/// text behind it, and the token's length (0 if there is no delimiter after it, or it is empty).
fn start_parse_numeric_or_ind_ref(l: &[u8]) -> (&[u8], &[u8], usize) {
    let i1 = position_to_next_whitespace_or_char(l, b"/<([]>%").filter(|&n| n > 0);
    let (mut s, l1, i1) = match i1 {
        Some(n) => (&l[..n], &l[n..], n),
        None => (l, &l[l.len()..], 0),
    };

    // Integers are sometimes prefixed with any form of 0. Following is a list of valid prefixes
    // that can be safely ignored: 0, 0.000000000
    if s.len() > 1 && s[0] == b'0' {
        match s[1] {
            b'+' | b'-' => s = &s[1..],
            b'.' => {
                let mut i = 2;
                while s.len() > i && s[i] == b'0' {
                    i += 1;
                }
                if s.len() > i && (s[i] == b'+' || s[i] == b'-') {
                    s = &s[i..];
                }
            }
            _ => {}
        }
    }
    (s, l1, i1)
}

/// `isPDFReal`: decimal digits, an optional sign and at most one period (7.3.3).
fn is_pdf_real(s: &[u8]) -> bool {
    let (mut digits, mut period) = (false, false);
    for (i, &c) in s.iter().enumerate() {
        match c {
            b'0'..=b'9' => digits = true,
            b'.' if !period => period = true,
            b'+' | b'-' if i == 0 => {}
            _ => return false,
        }
    }
    digits
}

/// `parseFloat`: a real number, with `,` accepted for the decimal point and `.-` for `.`; `None`
/// (Go's nil object) if the token is not a PDF real or does not fit a float.
fn parse_float(s: &[u8]) -> Result<Option<Object>, Error> {
    // Only these bytes can survive the checks below, so anything else is rejected before the
    // token (which may be long) is copied.
    if !s
        .iter()
        .all(|&c| c.is_ascii_digit() || matches!(c, b'.' | b',' | b'+' | b'-'))
    {
        return Ok(None);
    }
    let mut v = try_vec(s)?;
    // Replace ',' with '.' to accept comma as decimal separator.
    if let Some(p) = v.iter().position(|&c| c == b',') {
        v[p] = b'.';
    }
    // Preserve the existing tolerance for ".-" (e.g., ".-5").
    if let Some(p) = v.windows(2).position(|w| w == b".-") {
        v.remove(p + 1);
    }
    if !is_pdf_real(&v) {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(&v) else {
        return Ok(None);
    };
    // Go reports an overflow as a range error; Rust returns infinity.
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(Some(Object::Float(f))),
        _ => Ok(None),
    }
}

/// `parseIndRef`: `i` is followed by the token `s` (of length `i2` in `l`); with an `R` behind
/// it, this is a reference.
fn parse_ind_ref<'s>(
    s: &[u8],
    l: &'s [u8],
    l1: &'s [u8],
    line: &mut &'s [u8],
    i: i64,
    i2: usize,
) -> Object {
    let Ok(g) = atoi(s) else {
        // 2nd int (generation number) not available. Can't be an indirect reference.
        *line = l1;
        return Object::Integer(i);
    };

    let (l, _) = trim_left_space(&l[i2..], false);
    if l.is_empty() {
        // Only whitespace.
        *line = l1;
        return Object::Integer(i);
    }

    if l[0] == b'R' {
        *line = forward_parse_buf(l, 1);
        // We have all 3 components to create an indirect reference.
        return Object::Ref(Ref {
            num: i,
            generation: g,
        });
    }

    // 'R' not available. Can't be an indirect reference.
    *line = l1;
    Object::Integer(i)
}

/// `parseNumericOrIndRef`: an integer, a real, a reference, or null for a token that is none of
/// them (the token is skipped).
fn parse_numeric_or_ind_ref<'s>(line: &mut &'s [u8]) -> Result<Object, Error> {
    let l: &'s [u8] = line;
    if l.is_empty() {
        return Err(err_buf_not_available());
    }

    // If this object is an integer we need to check for an indirect reference, eg. 1 0 R,
    // otherwise it has to be a float. We have to check first for integer.
    let (s, l1, i1) = start_parse_numeric_or_ind_ref(l);

    // Try int.
    let i = match atoi(s) {
        Ok(i) => i,
        Err(AtoiError::Range) => {
            // #407
            *line = l1;
            return Ok(Object::Integer(0));
        }
        Err(AtoiError::Syntax) => {
            *line = l1;
            return Ok(parse_float(s)?.unwrap_or(Object::Null));
        }
    };

    // We have an Int! If not followed by whitespace, return the sole integer value.
    if i1 == 0 || delimiter(l[i1]) {
        *line = l1;
        return Ok(Object::Integer(i));
    }

    // Must be an indirect reference (123 0 R). Missing is the 2nd int and "R".
    let (l2, _) = trim_left_space(&l[i1..], false);
    if l2.is_empty() {
        // Only whitespace.
        *line = l1;
        return Ok(Object::Integer(i));
    }

    // If only 2 tokens, can't be an indirect reference. If not followed by whitespace, return
    // the sole integer value.
    let i2 = position_to_next_whitespace_or_char(l2, b"/<([]>").filter(|&n| n > 0);
    let Some(i2) = i2 else {
        *line = l1;
        return Ok(Object::Integer(i));
    };
    if delimiter(l2[i2]) {
        *line = l1;
        return Ok(Object::Integer(i));
    }

    Ok(parse_ind_ref(&l2[..i2], l2, l1, line, i, i2))
}

/// `parseObjectValue`.
fn parse_object_value(
    cx: &mut ParseCtx<'_>,
    l: &mut &[u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Object, Error> {
    let Some(&first) = l.first() else {
        return Err(err_buf_not_available());
    };
    match first {
        // Array.
        b'[' => Ok(Object::Array(parse_array(
            cx,
            l,
            level,
            depth_limit,
            repair,
        )?)),
        // Name.
        b'/' => Ok(Object::Name(parse_name(l)?)),
        // Hex literal or dict.
        b'<' => parse_hex_literal_or_dict(cx, l, level, depth_limit, repair),
        // String literal.
        b'(' => parse_string_literal(l),
        _ => {
            if let Some((value, keyword_len)) = parse_boolean_or_null(l) {
                *l = forward_parse_buf(l, keyword_len);
                return Ok(value);
            }
            // Must be numeric or indirect reference: int 0 r, int, float.
            parse_numeric_or_ind_ref(l)
        }
    }
}

/// `parseObject`: `repair` enables the missing-value and junk recovery in dictionaries.
fn parse_object_inner(
    cx: &mut ParseCtx<'_>,
    line: &mut &[u8],
    level: u64,
    depth_limit: u64,
    repair: bool,
) -> Result<Object, Error> {
    if line.is_empty() {
        return Err(err_buf_not_available());
    }
    check_recursion_depth(level, depth_limit)?;
    cx.check()?;

    // Position to the first non whitespace char.
    let (mut l, _) = trim_left_space(line, false);
    if l.is_empty() {
        // Only whitespace.
        return Err(err_buf_not_available());
    }

    let value = parse_object_value(cx, &mut l, level, depth_limit, repair)?;
    cx.charge(parsed_scalar_bytes(&value))?;
    cx.check()?;

    *line = l;
    Ok(value)
}

/// `ParseObject`: the next object of `line`, which is moved behind it. A null (also a skipped
/// token such as `NaN`) is [`Object::Null`]. If the first pass fails, a second one repairs
/// dictionaries.
///
/// `max_depth` is the recursion limit (`None` or 0: 100); `level` is where the caller already
/// is (0).
///
/// # Errors
/// A syntax error ([`Error::Corrupt`], [`Sentinel::NameObjectCorrupt`]), the recursion limit
/// ([`Sentinel::MaxRecursionDepth`]), [`LimitKind::Parsed`], [`Error::Cancelled`].
pub(crate) fn parse_object<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    max_depth: Option<u64>,
) -> Result<Object, Error> {
    if line.is_empty() {
        return Err(err_buf_not_available());
    }
    let depth_limit = effective_depth_limit(max_depth);
    let original: &'s [u8] = line;
    match parse_object_inner(cx, line, level, depth_limit, false) {
        Ok(v) => Ok(v),
        Err(e) => {
            if e.is(Sentinel::MaxRecursionDepth) || cx.cancelled() {
                return Err(e);
            }
            *line = original;
            parse_object_inner(cx, line, level, depth_limit, true)
        }
    }
}

/// `ParseObjectResult`: an object and the strict failure that the relaxed fallback accepted.
#[derive(Debug, Clone)]
pub struct ParseObjectResult {
    pub object: Object,
    /// Set when the first pass failed on a dictionary key and the repair pass succeeded. Go
    /// reports it as a validation notice.
    pub strict_failure: Option<Error>,
}

/// `ParseObjectWithPolicy` in relaxed validation mode. Like [`parse_object`], but it keeps the
/// failure of the first pass when it was a dictionary key that did not parse.
///
/// # Errors
/// As [`parse_object`].
pub fn parse_object_with_policy<'s>(
    cx: &mut ParseCtx<'_>,
    line: &mut &'s [u8],
    level: u64,
    max_depth: Option<u64>,
) -> Result<ParseObjectResult, Error> {
    if line.is_empty() {
        return Err(err_buf_not_available());
    }
    let depth_limit = effective_depth_limit(max_depth);
    let original: &'s [u8] = line;
    let first = match parse_object_inner(cx, line, level, depth_limit, false) {
        Ok(object) => {
            return Ok(ParseObjectResult {
                object,
                strict_failure: None,
            });
        }
        Err(e) => e,
    };
    if first.is(Sentinel::MaxRecursionDepth) || cx.cancelled() {
        return Err(first);
    }
    let policy_managed = is_dictionary_key_error(&first);
    *line = original;
    let object = parse_object_inner(cx, line, level, depth_limit, true)?;
    Ok(ParseObjectResult {
        object,
        strict_failure: if policy_managed { Some(first) } else { None },
    })
}

// ---------------------------------------------------------------------------------------------
// Cross-reference and object streams

/// `createXRefStreamDict`: reads the field widths `/W`.
fn create_xref_stream_dict(
    sd: &StreamDict,
    objs: Vec<i64>,
    size: i64,
) -> Result<XRefStreamDict, Error> {
    // An array of integers representing the size of the fields in a single cross-reference
    // entry.
    let Some(a) = sd.w() else {
        return Err(corrupt("parse: xref stream dict missing entry W"));
    };
    // Validate an array of 3 non-negative integers.
    if a.len() != 3 {
        return Err(err_xref_stream_corrupt_w());
    }
    let mut w = [0i64; 3];
    for (k, item) in a.iter().enumerate() {
        match item {
            Object::Integer(v) if *v >= 0 => w[k] = *v,
            _ => return Err(err_xref_stream_corrupt_w()),
        }
    }
    Ok(XRefStreamDict {
        stream: sd.clone(),
        size,
        objects: objs,
        w,
        previous_offset: sd.prev(),
    })
}

/// `xRefStreamSize`: the direct `/Size`.
fn xref_stream_size(sd: &StreamDict, limits: &Limits, relaxed: bool) -> Result<i64, Error> {
    let Some(size) = sd.size() else {
        return Err(corrupt("\"Size\" not available"));
    };
    if size <= 0 {
        if relaxed && sd.index().is_some() {
            return Ok(0);
        }
        return Err(err_invalid_size());
    }
    let max = lim(limits.max_object_count);
    if size > max {
        return Err(Error::Corrupt(format!(
            "\"Size\" {size} exceeds limit {max}"
        )));
    }
    Ok(size)
}

/// `xRefStreamObjectsFromIndex`: the object numbers of `/Index`, and the size, which a relaxed
/// parse raises to the end of the last range.
fn xref_stream_objects_from_index(
    ind_arr: &[Object],
    size: i64,
    limits: &Limits,
    relaxed: bool,
    held: Option<&mut Held>,
) -> Result<(Vec<i64>, i64), Error> {
    if !ind_arr.len().is_multiple_of(2) {
        return Err(err_xref_stream_corrupt_index());
    }
    let max_obj = lim(limits.max_object_count);
    let max_entries = lim(limits.max_xref_entries);
    let mut size = size;
    let mut total: i64 = 0;

    for pair in ind_arr.as_chunks::<2>().0 {
        let (Object::Integer(start), Object::Integer(n)) = (&pair[0], &pair[1]) else {
            return Err(err_xref_stream_corrupt_index());
        };
        let (start, n) = (*start, *n);
        if start < 0 || n < 0 || start > max_obj - n {
            return Err(err_xref_stream_corrupt_index());
        }
        let end = start + n;
        if end > size {
            if !relaxed {
                return Err(Error::Sentinel(Sentinel::XRefStreamIndexSizeMismatch));
            }
            size = end;
        }
        if n > max_entries - total {
            return Err(Error::Corrupt(format!(
                "xref entry count exceeds limit {max_entries}"
            )));
        }
        total += n;
    }

    charge_objs(held, total)?;
    let mut objs: Vec<i64> = Vec::new();
    objs.try_reserve_exact(usize::try_from(total).map_err(|_| Error::OutOfMemory)?)?;
    for pair in ind_arr.as_chunks::<2>().0 {
        if let (Object::Integer(start), Object::Integer(n)) = (&pair[0], &pair[1]) {
            for j in 0..*n {
                objs.push(*start + j);
            }
        }
    }
    Ok((objs, size))
}

/// `xRefStreamObjectsFromSize`: the object numbers `0..size`.
fn xref_stream_objects_from_size(
    size: i64,
    limits: &Limits,
    held: Option<&mut Held>,
) -> Result<(Vec<i64>, i64), Error> {
    let max_entries = lim(limits.max_xref_entries);
    if size > max_entries {
        return Err(Error::Corrupt(format!(
            "xref entry count {size} exceeds limit {max_entries}"
        )));
    }
    charge_objs(held, size)?;
    let mut objs: Vec<i64> = Vec::new();
    objs.try_reserve_exact(usize::try_from(size).map_err(|_| Error::OutOfMemory)?)?;
    objs.extend(0..size);
    Ok((objs, size))
}

/// Charges the object list of `n` numbers of a cross-reference stream to `held`, if there is one.
fn charge_objs(held: Option<&mut Held>, n: i64) -> Result<(), Error> {
    match held {
        Some(h) => h.grow_by(u64::try_from(n).unwrap_or(0).saturating_mul(8)),
        None => Ok(()),
    }
}

fn parse_xref_stream_dict(
    sd: &StreamDict,
    limits: &Limits,
    relaxed: bool,
    held: Option<&mut Held>,
) -> Result<XRefStreamDict, Error> {
    let size = xref_stream_size(sd, limits, relaxed)?;

    // Read the optional parameter Index.
    let (objs, size) = match sd.index() {
        Some(ind_arr) => xref_stream_objects_from_index(ind_arr, size, limits, relaxed, held)?,
        None => xref_stream_objects_from_size(size, limits, held)?,
    };
    if size <= 0 {
        return Err(err_invalid_size());
    }
    if let Some(declared) = sd.size()
        && declared <= 0
        && objs.is_empty()
    {
        return Err(err_invalid_size());
    }

    create_xref_stream_dict(sd, objs, size)
}

/// `ParseXRefStreamDictWithLimits`: an [`XRefStreamDict`] out of a [`StreamDict`], with the
/// object numbers of its `/Index` (or `0..Size`) and the field widths of `/W`.
///
/// # Errors
/// A missing or indirect `/Size`, a size over `max_object_count`, entries over
/// `max_xref_entries`, a corrupt `/Index` or `/W`, an `/Index` that goes past `/Size`
/// ([`Sentinel::XRefStreamIndexSizeMismatch`]).
pub fn parse_xref_stream_dict_with_limits(
    sd: &StreamDict,
    limits: &Limits,
) -> Result<XRefStreamDict, Error> {
    parse_xref_stream_dict(sd, limits, false, None)
}

/// `ParseXRefStreamDictRelaxedWithLimits`: as [`parse_xref_stream_dict_with_limits`], but an
/// `/Index` that goes past `/Size` raises the size (within the limits).
///
/// # Errors
/// As [`parse_xref_stream_dict_with_limits`], except the size mismatch.
pub fn parse_xref_stream_dict_relaxed_with_limits(
    sd: &StreamDict,
    limits: &Limits,
) -> Result<XRefStreamDict, Error> {
    parse_xref_stream_dict(sd, limits, true, None)
}

/// [`parse_xref_stream_dict_relaxed_with_limits`] that charges the object list to `held` (the
/// table's ledger charge) before it is allocated.
///
/// # Errors
/// As [`parse_xref_stream_dict_relaxed_with_limits`]; [`Error::Alloc`].
pub(crate) fn parse_xref_stream_dict_charged(
    sd: &StreamDict,
    limits: &Limits,
    held: &mut Held,
) -> Result<XRefStreamDict, Error> {
    parse_xref_stream_dict(sd, limits, true, Some(held))
}

/// `objectStreamDict`.
fn object_stream_dict(
    sd: &StreamDict,
    limits: &Limits,
    n: Option<i64>,
    first: Option<i64>,
) -> Result<ObjectStreamDict, Error> {
    let Some(first) = first else {
        return Err(corrupt("parse: obj stream dict missing entry First"));
    };
    let Some(n) = n else {
        // Go's message says W.
        return Err(corrupt("parse: obj stream dict missing entry W"));
    };
    if n <= 0 || n > lim(limits.max_object_stream_count) {
        return Err(Error::Corrupt(format!(
            "object stream N {n} exceeds limit {}",
            lim(limits.max_object_stream_count)
        )));
    }
    if first < 0 || first > lim(limits.max_object_stream_first) {
        return Err(Error::Corrupt(format!(
            "object stream First {first} exceeds limit {}",
            lim(limits.max_object_stream_first)
        )));
    }

    Ok(ObjectStreamDict {
        stream: sd.clone(),
        prolog: Vec::new(),
        obj_count: n,
        first_obj_offset: first,
        max_decode_bytes: lim(limits.max_decode_bytes),
        obj_array: Vec::new(),
        body: Vec::new(),
    })
}

/// `ObjectStreamDictWithLimits`: an [`ObjectStreamDict`] out of a [`StreamDict`], with the direct
/// `/N` and `/First`.
///
/// # Errors
/// A missing `/First` or `/N`, `/N` over `max_object_stream_count` or not positive, `/First`
/// negative or over `max_object_stream_first`.
pub fn object_stream_dict_with_limits(
    sd: &StreamDict,
    limits: &Limits,
) -> Result<ObjectStreamDict, Error> {
    object_stream_dict(sd, limits, sd.n(), sd.first())
}

/// `ObjectStreamDictWithResolvedIntegers`: as [`object_stream_dict_with_limits`], with `/N` and
/// `/First` the caller has already resolved (they may be indirect in the dictionary, which stays
/// as it is).
///
/// # Errors
/// As [`object_stream_dict_with_limits`].
pub fn object_stream_dict_with_resolved_integers(
    sd: &StreamDict,
    limits: &Limits,
    n: Option<i64>,
    first: Option<i64>,
) -> Result<ObjectStreamDict, Error> {
    object_stream_dict(sd, limits, n, first)
}

// ---------------------------------------------------------------------------------------------
// Keyword detection

/// `isMarkerTerminated`: PDF whitespace after a recovery marker (7.2.3).
fn is_marker_terminated(b: u8) -> bool {
    pdf_whitespace(b)
}

/// `detectMarker`: the position of `marker` (`endobj` or `stream`) in `line` when it is followed
/// by whitespace, -1 if there is none. Go's arithmetic for a marker that is followed by other
/// bytes is kept as it is.
fn detect_marker(line: &[u8], marker: &[u8]) -> i64 {
    let mut line = line;
    let Some(i) = find(line, marker) else {
        return -1;
    };
    if i + marker.len() >= line.len() {
        return -1;
    }
    let mut off = i + marker.len();
    let mut ind = i;
    while !is_marker_terminated(line[off]) {
        line = &line[off..];
        if marker == b"endobj"
            && let Some(j) = find(line, b"xref")
            && j + 4 < line.len()
            && is_marker_terminated(line[j + 4])
        {
            return to_i64(ind);
        }
        let Some(i) = find(line, marker) else {
            return -1;
        };
        if i + marker.len() >= line.len() {
            return -1;
        }
        off = i + marker.len();
        ind += off;
    }
    to_i64(ind)
}

/// `detectMarkers`: searches for the markers that still need a search.
fn detect_markers(line: &[u8], end_ind: &mut i64, stream_ind: &mut i64) {
    if *end_ind == POS_UNSEARCHED {
        *end_ind = detect_marker(line, b"endobj");
    }
    if *stream_ind == POS_UNSEARCHED {
        *stream_ind = detect_marker(line, b"stream");
    }
}

/// `posFloor`: the smaller of two positions, ignoring a negative one.
fn pos_floor(pos1: i64, pos2: i64) -> i64 {
    if pos1 < 0 {
        return pos2;
    }
    if pos1 < pos2 {
        return pos1;
    }
    if pos2 < 0 {
        return pos1;
    }
    pos2
}

fn apply_off_both(end_ind: i64, stream_ind: i64, off: i64) -> (i64, i64) {
    let end = if end_ind >= 0 { end_ind + off } else { end_ind };
    let stream = if stream_ind >= 0 {
        stream_ind + off
    } else {
        stream_ind
    };
    (end, stream)
}

fn apply_off_end_ind_first(end_ind: i64, stream_ind: i64, off: i64, floor: i64) -> (i64, i64) {
    let end = end_ind + off;
    let stream = if stream_ind >= 0 {
        if stream_ind > floor {
            // stream after any ( or % to skip
            -1
        } else {
            stream_ind + off
        }
    } else {
        stream_ind
    };
    (end, stream)
}

fn apply_off_stream_ind_first(end_ind: i64, stream_ind: i64, off: i64, floor: i64) -> (i64, i64) {
    let stream = stream_ind + off;
    let end = if end_ind >= 0 {
        if end_ind > floor {
            // endobj after any ( or % to skip
            -1
        } else {
            end_ind + off
        }
    } else {
        end_ind
    };
    (end, stream)
}

fn is_comment(comment_pos: i64, str_lit_pos: i64) -> bool {
    comment_pos >= 0 && (str_lit_pos < 0 || comment_pos < str_lit_pos)
}

/// Moves a found position behind `delta` skipped bytes.
fn adjust_found(pos: &mut i64, delta: i64) {
    if *pos >= delta {
        *pos -= delta;
    } else if *pos != -1 {
        *pos = POS_UNSEARCHED;
    }
}

/// `skipComment`: `line` from the end of the comment at `comment_pos`.
fn skip_comment<'a>(
    line: &'a [u8],
    comment_pos: i64,
    off: &mut i64,
    end_ind: &mut i64,
    stream_ind: &mut i64,
) -> &'a [u8] {
    let tail = line.get(ix(comment_pos)..).unwrap_or(EMPTY);
    let Some(i) = position_to_next_eol(tail) else {
        if *end_ind >= comment_pos {
            *end_ind = -1;
        }
        if *stream_ind >= comment_pos {
            *stream_ind = -1;
        }
        return EMPTY;
    };
    let delta = comment_pos + to_i64(i);
    *off += delta;
    // Adjust found positions for the changed line.
    adjust_found(end_ind, delta);
    adjust_found(stream_ind, delta);
    &tail[i..]
}

/// `skipStringLit`: `line` from the end of the string literal at `str_lit_pos`.
fn skip_string_lit<'a>(
    line: &'a [u8],
    str_lit_pos: i64,
    off: &mut i64,
    end_ind: &mut i64,
    stream_ind: &mut i64,
) -> Result<&'a [u8], Error> {
    let tail = line.get(ix(str_lit_pos)..).unwrap_or(EMPTY);
    let Some(i) = balanced_parentheses_prefix(tail) else {
        return Err(err_string_literal_corrupt());
    };
    let rest = forward_parse_buf(&tail[i..], 1);
    let delta = str_lit_pos + to_i64(i) + 1;
    *off += delta;
    // Adjust found positions for the changed line.
    adjust_found(end_ind, delta);
    adjust_found(stream_ind, delta);
    Ok(rest)
}

/// `rebaseDelimiterPos`: a delimiter position after `delta` bytes were skipped.
fn rebase_delimiter_pos(pos: i64, delta: i64, empty: bool) -> i64 {
    if empty {
        return -1;
    }
    if pos < 0 {
        return pos;
    }
    if pos >= delta {
        return pos - delta;
    }
    POS_UNSEARCHED
}

/// `DetectKeywords`: where the `endobj` and the `stream` keywords of an object buffer are, -1
/// for none. A keyword inside a literal string or a comment does not count.
///
/// # Errors
/// [`Error::Cancelled`], or a literal string that is not closed before the buffer ends.
pub fn detect_keywords(cancel: &AtomicBool, line: &[u8]) -> Result<(i64, i64), Error> {
    // Return the endobj or stream index, whichever is encountered first.
    let mut line = line;
    let mut off: i64 = 0;
    let mut str_lit_pos = POS_UNSEARCHED;
    let mut comment_pos = POS_UNSEARCHED;
    let mut end_ind = POS_UNSEARCHED;
    let mut stream_ind = POS_UNSEARCHED;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }

        detect_markers(line, &mut end_ind, &mut stream_ind);

        if off == 0 && end_ind < 0 && stream_ind < 0 {
            return Ok((-1, -1));
        }

        // Outside literal strings, backslashes do not escape delimiters.
        if str_lit_pos == POS_UNSEARCHED {
            str_lit_pos = index_byte(line, b'(');
        }
        if comment_pos == POS_UNSEARCHED {
            comment_pos = index_byte(line, b'%');
        }

        if str_lit_pos < 0 && comment_pos < 0 {
            // Neither ( nor % to skip.
            return Ok(apply_off_both(end_ind, stream_ind, off));
        }

        let floor = pos_floor(str_lit_pos, comment_pos);

        if end_ind >= 0 && end_ind < floor {
            // endobj before any ( or % to skip.
            return Ok(apply_off_end_ind_first(end_ind, stream_ind, off, floor));
        }

        if stream_ind >= 0 && stream_ind < floor {
            // stream before any ( or % to skip.
            return Ok(apply_off_stream_ind_first(end_ind, stream_ind, off, floor));
        }

        let prev_off = off;
        line = if is_comment(comment_pos, str_lit_pos) {
            // Skip a comment if % is before any (.
            skip_comment(line, comment_pos, &mut off, &mut end_ind, &mut stream_ind)
        } else {
            skip_string_lit(line, str_lit_pos, &mut off, &mut end_ind, &mut stream_ind)?
        };
        let delta = off - prev_off;
        str_lit_pos = rebase_delimiter_pos(str_lit_pos, delta, line.is_empty());
        comment_pos = rebase_delimiter_pos(comment_pos, delta, line.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::string::hex_decode;
    use std::collections::BTreeMap;

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn run(input: &[u8]) -> (Result<Object, Error>, Vec<u8>) {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = input;
        let r = parse_object(&mut cx, &mut line, 0, None);
        (r, line.to_vec())
    }

    fn ok(input: &[u8]) -> Object {
        let (r, _) = run(input);
        match r {
            Ok(o) => o,
            Err(e) => panic!("{:?} failed: {e}", String::from_utf8_lossy(input)),
        }
    }

    fn fails(input: &[u8]) -> Error {
        let (r, _) = run(input);
        match r {
            Ok(o) => panic!("{:?} parsed as {o:?}", String::from_utf8_lossy(input)),
            Err(e) => e,
        }
    }

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn w111() -> Object {
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(1),
            Object::Integer(1),
        ])
    }

    /// Go: TestObjectParserRegularSpaceBytes.
    #[test]
    fn regular_space_bytes_are_not_whitespace() {
        let regulars: [&[u8]; 9] = [
            b"\x0b",
            b"\x85",
            b"\xa0",
            b"\xc2\x85",
            b"\xc2\xa0",
            "\u{2003}".as_bytes(),
            "\u{2028}".as_bytes(),
            "\u{3000}".as_bytes(),
            b"\xff",
        ];
        for regular in regulars {
            let name = cat(&[b"A", regular, b"B"]);
            let input = cat(&[b"/", &name, b" /Tail"]);
            let (r, rest) = run(&input);
            assert!(matches!(r, Ok(Object::Name(ref n)) if *n == name));
            assert_eq!(rest, b" /Tail");
            for relaxed in [false, true] {
                let want = cat(&[regular, b"\n/Name"]);
                let padded = cat(&[b" \t", &want]);
                let (got, eol) = trim_left_space(&padded, relaxed);
                assert_eq!(got, &want[..]);
                assert!(!eol);
            }
            let prefix = cat(&[b"12", regular, b"34"]);
            assert_eq!(
                position_to_next_whitespace_or_char(&cat(&[&prefix, b" 5"]), b""),
                Some(prefix.len())
            );
            assert_eq!(
                position_to_next_whitespace_or_char(&cat(&[&prefix, b"/Tail"]), b"/"),
                Some(prefix.len())
            );
        }
    }

    /// Go: TestObjectParserPDFWhitespace.
    #[test]
    fn pdf_whitespace_separates_tokens() {
        let all: [&[u8]; 6] = [b"\x00", b"\t", b"\n", b"\x0c", b"\r", b" "];
        for ws in all {
            let input = cat(&[ws, b"/Name", ws, b"17"]);
            let cancel = AtomicBool::new(false);
            let mut cx = ParseCtx::new(&cancel);
            let mut line: &[u8] = &input;
            let o = parse_object(&mut cx, &mut line, 0, None);
            assert!(matches!(o, Ok(Object::Name(ref n)) if n == b"Name"));
            assert_eq!(line, cat(&[ws, b"17"]));
            let o = parse_object(&mut cx, &mut line, 0, None);
            assert!(matches!(o, Ok(Object::Integer(17))));
            assert!(line.is_empty());
        }
    }

    /// Go: TestTrimLeftPDFWhitespace.
    #[test]
    fn trim_left_pdf_whitespace() {
        let cases: [(&[u8], &[u8], bool); 6] = [
            (b"", b"", false),
            (b"\x00\t\x0c /Name", b"/Name", false),
            (b"\x00\t\n\x0c\r /Name", b"/Name", true),
            (b"%comment\r\n/Name", b"/Name", true),
            (b" %first\n\t%second\r/Name", b"/Name", true),
            (b"%comment without line ending", b"", false),
        ];
        for (input, want, eol) in cases {
            for relaxed in [false, true] {
                let (got, got_eol) = trim_left_space(input, relaxed);
                assert_eq!(got, want, "{input:?} relaxed={relaxed}");
                assert_eq!(got_eol, relaxed && eol, "{input:?} relaxed={relaxed}");
            }
        }
        // A lone % in the last byte is not a comment.
        assert_eq!(trim_left_space(b" %", false).0, b"%");
    }

    /// Go: TestParseObjectAttributesWhitespace.
    #[test]
    fn object_attributes_whitespace() {
        let whitespace: [&[u8]; 8] = [
            b"\x00",
            b"\t",
            b"\n",
            b"\x0c",
            b"\r",
            b" ",
            b"\r\n",
            b"\x00\t\n\x0c\r ",
        ];
        const REMAINDER: &[u8] = b" << /Length 0 >>\nendobj\n13 0 obj";
        for before_generation in whitespace {
            for before_obj in whitespace {
                for generation in [0i64, 34] {
                    let g = generation.to_string();
                    let input = cat(&[
                        before_generation,
                        b"12",
                        before_generation,
                        g.as_bytes(),
                        before_obj,
                        b"obj",
                        REMAINDER,
                    ]);
                    let mut line: &[u8] = &input;
                    let got = parse_object_attributes(&mut line);
                    assert_eq!(got, Ok((12, generation)), "{input:?}");
                    assert_eq!(line, REMAINDER, "{input:?}");
                }
            }
        }
    }

    #[test]
    fn object_attributes_errors_leave_the_line() {
        let mut line: &[u8] = b"12 0 xyz";
        assert!(parse_object_attributes(&mut line).is_err());
        assert_eq!(line, b"12 0 xyz");
        let mut line: &[u8] = b"x obj";
        assert_eq!(
            parse_object_attributes(&mut line),
            Err(Error::Sentinel(Sentinel::CorruptObjectOffset))
        );
        // No generation number.
        let mut line: &[u8] = b"12 obj";
        assert!(parse_object_attributes(&mut line).is_err());
        // The generation is not followed by whitespace.
        let mut line: &[u8] = b"12 0obj";
        assert!(parse_object_attributes(&mut line).is_err());
        // The object number alone.
        let mut line: &[u8] = b"12obj rest";
        assert_eq!(parse_object_attributes(&mut line), Ok((12, 0)));
        assert_eq!(line, b" rest");
        // `obj` is also found as `bj`.
        let mut line: &[u8] = b"12 0 bj<<>>";
        assert_eq!(parse_object_attributes(&mut line), Ok((12, 0)));
        assert_eq!(line, b"<<>>");
    }

    #[test]
    fn atoi_matches_go() {
        assert_eq!(atoi(b"0"), Ok(0));
        assert_eq!(atoi(b"+17"), Ok(17));
        assert_eq!(atoi(b"-98"), Ok(-98));
        assert_eq!(atoi(b"-0"), Ok(0));
        assert_eq!(atoi(b"9223372036854775807"), Ok(i64::MAX));
        assert_eq!(atoi(b"-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(atoi(b"9223372036854775808"), Err(AtoiError::Range));
        assert_eq!(atoi(b"-9223372036854775809"), Err(AtoiError::Range));
        assert_eq!(atoi(b"18446744071963345064"), Err(AtoiError::Range));
        // The range error comes first when the digits overflow before the bad byte.
        assert_eq!(atoi(b"99999999999999999999x"), Err(AtoiError::Range));
        assert_eq!(atoi(b"9223372036854775808x"), Err(AtoiError::Syntax));
        let bads: [&[u8]; 8] = [b"", b"+", b"-", b"1a", b" 1", b"1_0", b"0x10", b"1.0"];
        for bad in bads {
            assert_eq!(atoi(bad), Err(AtoiError::Syntax), "{bad:?}");
        }
    }

    /// Go: TestParseFloatRejectsNonPDFNumbers.
    #[test]
    fn parse_float_rejects_non_pdf_numbers() {
        let long = format!("{}.0", "9".repeat(400));
        let inputs: Vec<&str> = vec![
            "NaN",
            "nan",
            "NAN",
            "Inf",
            "+Inf",
            "-Inf",
            "Infinity",
            "-Infinity",
            "iNfInItY",
            "1e5",
            "-1E-5",
            "0x1p2",
            "0X1.FP+3",
            "1_000.0",
            "1.2.3",
            "1,2,3",
            "1.-2.-3",
            "",
            ".",
            "+.",
            "-",
            "+",
            "1a",
            "\u{661}.\u{665}",
            long.as_str(),
        ];
        for input in inputs {
            assert!(
                matches!(parse_float(input.as_bytes()), Ok(None)),
                "{input:?} is not a PDF real"
            );
        }
    }

    /// Go: TestParseFloatDecimals.
    #[test]
    fn parse_float_decimals() {
        let long = format!("0.{}1", "0".repeat(400));
        let cases: Vec<(&str, f64)> = vec![
            ("0", 0.0),
            ("+17", 17.0),
            ("-98", -98.0),
            ("34.5", 34.5),
            ("-3.62", -3.62),
            ("+123.6", 123.6),
            ("4.", 4.0),
            ("-.002", -0.002),
            (".5", 0.5),
            ("00.50", 0.5),
            ("1,5", 1.5),
            ("-2,75", -2.75),
            (".-5", 0.5),
            ("1.-5", 1.5),
            ("1,-5", 1.5),
            (long.as_str(), 0.0),
        ];
        for (input, want) in cases {
            match parse_float(input.as_bytes()) {
                Ok(Some(Object::Float(f))) => assert_eq!(f, want, "{input}"),
                other => panic!("{input}: {other:?}"),
            }
        }
        match parse_float(b"-0.0") {
            Ok(Some(Object::Float(f))) => assert!(f.is_sign_negative()),
            other => panic!("negative zero lost: {other:?}"),
        }
        // The vectors Go's strconv gives: correctly rounded decimal to binary.
        match parse_float(b"0.1") {
            Ok(Some(Object::Float(f))) => assert_eq!(f.to_bits(), 0x3FB9_9999_9999_999A),
            other => panic!("{other:?}"),
        }
    }

    /// Go: TestParseObjectSkipsNonPDFNumbers.
    #[test]
    fn non_pdf_numbers_are_skipped_without_eating_the_next_object() {
        for input in ["NaN", "+Inf", "-Infinity", "1e5", "0x1p2", "1_000.0"] {
            let text = format!("{input} 17");
            let cancel = AtomicBool::new(false);
            let mut cx = ParseCtx::new(&cancel);
            let mut line: &[u8] = text.as_bytes();
            let o = parse_object(&mut cx, &mut line, 0, None);
            assert!(matches!(o, Ok(Object::Null)), "{input}");
            assert_eq!(line, b" 17", "{input}");
            let o = parse_object(&mut cx, &mut line, 0, None);
            assert!(matches!(o, Ok(Object::Integer(17))), "{input}");
            assert!(line.is_empty(), "{input}");
        }
    }

    fn check_hex(input: &[u8], want: &[u8]) {
        match ok(input) {
            Object::Hex(h) => {
                assert_eq!(hex_decode(&h).unwrap(), want, "{input:?}");
            }
            o => panic!("{input:?}: got {o:?}, want a hex string"),
        }
    }

    /// Go: TestParseHexStringWhitespace.
    #[test]
    fn hex_string_whitespace() {
        let all: [&[u8]; 7] = [
            b"\x00",
            b"\t",
            b"\n",
            b"\x0c",
            b"\r",
            b" ",
            b"\x00\t\n\x0c\r ",
        ];
        let cases: [(&[u8], &[u8]); 5] = [
            (b"41", b"A"),
            (b"4142", b"AB"),
            (b"414", b"A@"),
            (b"9aB", b"\x9a\xb0"),
            (b"", b""),
        ];
        for ws in all {
            for (digits, want) in cases {
                let mut input = cat(&[b"<", ws]);
                for (k, d) in digits.iter().enumerate() {
                    if k > 0 {
                        input.extend_from_slice(ws);
                    }
                    input.push(*d);
                }
                input.extend_from_slice(ws);
                input.push(b'>');
                check_hex(&input, want);
            }
        }
        check_hex(b"<>", b"");
        check_hex(b"<901FA>", b"\x90\x1f\xa0");
        // Unicode white space around the digits is trimmed (strings.TrimSpace).
        check_hex(b"<\xc2\xa041\xe3\x80\x80>", b"A");
    }

    /// Go: TestHexStringRejectsInvalidCharacters.
    #[test]
    fn hex_string_rejects_invalid_characters() {
        let bad: [&[u8]; 5] = [b"4\x0b1", b"4\xc2\x851", b"4\xc2\xa01", b"4Z1", b"4%1"];
        for s in bad {
            assert_eq!(hex_string(s), Ok(None), "accepted {s:?}");
        }
        assert_eq!(hex_string(b""), Ok(Some(Vec::new())));
        assert_eq!(hex_string(b"4 a\n1"), Ok(Some(b"4A10".to_vec())));
    }

    /// Go: TestDecodeNameHexInvalid.
    #[test]
    fn decode_name_hex_invalid() {
        let bad: [&[u8]; 6] = [b"#", b"#A", b"#a", b"#G0", b"#00", b"Fo\x00"];
        for tc in bad {
            assert_eq!(
                decode_name_hex_sequence(tc),
                Err(Error::Sentinel(Sentinel::NameObjectCorrupt)),
                "{tc:?}"
            );
        }
    }

    /// Go: TestDecodeNameHexValid.
    #[test]
    fn decode_name_hex_valid() {
        let cases: [(&str, &str); 14] = [
            ("", ""),
            ("Foo", "Foo"),
            ("A#23", "A#"),
            ("Name1", "Name1"),
            ("ASomewhatLongerName", "ASomewhatLongerName"),
            (
                "A;Name_With-Various***Characters?",
                "A;Name_With-Various***Characters?",
            ),
            ("1.2", "1.2"),
            ("$$", "$$"),
            ("@pattern", "@pattern"),
            (".notdef", ".notdef"),
            ("Lime#20Green", "Lime Green"),
            ("paired#28#29parentheses", "paired()parentheses"),
            ("The_Key_of_F#23_Minor", "The_Key_of_F#_Minor"),
            ("A#42", "AB"),
        ];
        for (input, want) in cases {
            assert_eq!(
                decode_name_hex_sequence(input.as_bytes()),
                Ok(want.as_bytes().to_vec()),
                "{input}"
            );
        }
    }

    /// Go: TestParseObjectRejectsRecursionDepth.
    #[test]
    fn recursion_depth_is_limited() {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = b"[[[1]]]";
        let e = parse_object(&mut cx, &mut line, 0, Some(1)).unwrap_err();
        assert!(e.is(Sentinel::MaxRecursionDepth), "{e}");

        // Deeper than any limit: the native stack is protected by MAX_NESTING.
        let deep = "[".repeat(100_000);
        let mut line: &[u8] = deep.as_bytes();
        let e = parse_object(&mut cx, &mut line, 0, Some(u64::MAX)).unwrap_err();
        assert!(e.is(Sentinel::MaxRecursionDepth), "{e}");
        let mut line: &[u8] = deep.as_bytes();
        let e = parse_object(&mut cx, &mut line, 0, None).unwrap_err();
        assert!(e.is(Sentinel::MaxRecursionDepth), "{e}");
    }

    /// Go: TestParseObjectBoundsRelaxedFallback.
    #[test]
    fn relaxed_fallback_is_bounded() {
        let s = "<</Differences[24/breve/quotesingle0 obj\r".repeat(34);
        let e = fails(s.as_bytes());
        assert_eq!(e, err_array_not_terminated());
    }

    #[test]
    fn cancel_and_parsed_budget() {
        let cancel = AtomicBool::new(true);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = b"[1 2 3]";
        assert_eq!(
            parse_object(&mut cx, &mut line, 0, None).unwrap_err(),
            Error::Cancelled
        );

        let cancel = AtomicBool::new(false);
        // 3 elements at 64 bytes each are over a budget of 100.
        let mut budget = ParsedBudget::new(100);
        let mut cx = ParseCtx::with_charge(&cancel, &mut budget);
        let mut line: &[u8] = b"[1 2 3]";
        let e = parse_object(&mut cx, &mut line, 0, None).unwrap_err();
        assert_eq!(e, Error::Limit(LimitKind::Parsed));
        assert!(e.is_limit_exceeded());

        // The estimate: an array of 2 elements, a dictionary of 1 entry with a 3 byte key and a
        // 4 byte name value, and a 5 byte string.
        let mut budget = ParsedBudget::new(1 << 20);
        let mut cx = ParseCtx::with_charge(&cancel, &mut budget);
        let mut line: &[u8] = b"[<</Key /Name>> (hello)]";
        assert!(parse_object(&mut cx, &mut line, 0, None).is_ok());
        assert_eq!(budget.parsed, 2 * 64 + 256 + (96 + 3) + 4 + 5);
    }

    #[test]
    fn object_values() {
        assert!(matches!(ok(b"null "), Object::Null));
        assert!(matches!(ok(b"TRUE"), Object::Bool(true)));
        assert!(matches!(ok(b"FAlse"), Object::Bool(false)));
        // A keyword ends where it ends: "nullx" is null, then x.
        let (r, rest) = run(b"nullx");
        assert!(matches!(r, Ok(Object::Null)));
        assert_eq!(rest, b"x");
        assert!(matches!(ok(b"12"), Object::Integer(12)));
        assert!(matches!(ok(b"-12"), Object::Integer(-12)));
        assert!(matches!(ok(b"+12"), Object::Integer(12)));
        assert!(matches!(ok(b"3.43"), Object::Float(f) if f == 3.43));
        assert!(matches!(
            ok(b"12 7 R"),
            Object::Ref(Ref {
                num: 12,
                generation: 7
            })
        ));
        // Not a reference: no R, or a second token that is no integer.
        let (r, rest) = run(b"12 7 S");
        assert!(matches!(r, Ok(Object::Integer(12))));
        assert_eq!(rest, b" 7 S");
        let (r, rest) = run(b"12 7");
        assert!(matches!(r, Ok(Object::Integer(12))));
        assert_eq!(rest, b" 7");
        let (r, rest) = run(b"12 /N");
        assert!(matches!(r, Ok(Object::Integer(12))));
        assert_eq!(rest, b" /N");
        // An integer that does not fit is 0 (#407).
        let (r, rest) = run(b"18446744071963345064 0 R");
        assert!(matches!(r, Ok(Object::Integer(0))));
        assert_eq!(rest, b" 0 R");
        // A prefix of zeros before a sign.
        assert!(matches!(
            ok(b"0.000000-16763662]"),
            Object::Integer(-16763662)
        ));
        assert!(matches!(ok(b"0+5 "), Object::Integer(5)));

        assert!(matches!(ok(b"/Na#20me "), Object::Name(ref n) if n == b"Na me"));
        assert!(matches!(ok(b"/"), Object::Name(ref n) if n.is_empty()));
        // A string keeps its escapes and balanced parentheses.
        assert!(matches!(ok(b"(a\\)b(c)d\\\\)"), Object::String(ref s) if s == b"a\\)b(c)d\\\\"));
        assert!(matches!(ok(b"()"), Object::String(ref s) if s.is_empty()));
        // A hex string is upper case and padded.
        assert!(matches!(ok(b"<0ab>"), Object::Hex(ref h) if h == b"0AB0"));
        // Junk in a hex string: skipped, null.
        let (r, rest) = run(b"<ABG> 5");
        assert!(matches!(r, Ok(Object::Null)));
        assert_eq!(rest, b" 5");

        let Object::Array(a) = ok(b"[1 true [2 0 R] <</A 1>> null]") else {
            panic!("not an array");
        };
        assert_eq!(a.len(), 5);
        assert!(matches!(a[0], Object::Integer(1)));
        assert!(matches!(a[1], Object::Bool(true)));
        assert!(matches!(&a[2], Object::Array(x) if x.len() == 1));
        assert!(matches!(&a[3], Object::Dict(d) if d.len() == 1));
        assert!(matches!(a[4], Object::Null));
    }

    #[test]
    fn dictionary_values() {
        // A null entry is the same as no entry; a duplicate keeps the later non-null value.
        let Object::Dict(d) = ok(b"<</A null /B 1 /B 2 /C 3 /C null /D /N>>") else {
            panic!("not a dictionary");
        };
        assert!(d.find(b"A").is_none());
        assert_eq!(d.int_entry(b"B"), Some(2));
        assert_eq!(d.int_entry(b"C"), Some(3));
        assert_eq!(d.name_entry(b"D"), Some(&b"N"[..]));
        assert_eq!(d.len(), 3);
        // A null after a value does not remove it, and a value replaces a null.
        let Object::Dict(d) = ok(b"<</A null /A 7>>") else {
            panic!("not a dictionary");
        };
        assert_eq!(d.int_entry(b"A"), Some(7));
        // Keys are decoded names.
        let Object::Dict(d) = ok(b"<</Key#28#29 (v)>>") else {
            panic!("not a dictionary");
        };
        assert!(d.0.contains_key(&b"Key()"[..]));
        // The repair pass: an entry with no value at the end of a line is an empty string.
        let Object::Dict(d) = ok(b"<</Title \n/Type /Outline\n/Key /Value>>") else {
            panic!("not a dictionary");
        };
        assert_eq!(d.string_entry(b"Title"), Some(&b""[..]));
        assert_eq!(d.name_entry(b"Type"), Some(&b"Outline"[..]));
        assert_eq!(d.name_entry(b"Key"), Some(&b"Value"[..]));
        // Junk keys are skipped in the repair pass.
        let Object::Dict(d) = ok(b"<<1 2 /Type /Catalog>>") else {
            panic!("not a dictionary");
        };
        assert_eq!(d.name_entry(b"Type"), Some(&b"Catalog"[..]));
    }

    /// Go: TestParseObject.
    #[test]
    fn parse_object_cases() {
        let good: Vec<&[u8]> = vec![
            b"null      ",
            b"true     ",
            b"[true%comment\x0Anull]",
            b"[[%comment\x0dnull][%empty\x0A\x0Dtrue]false%comment\x0A]",
            b"<<>>",
            b"<</Key %comment\x0a true%comment       \x0a\x0d>>",
            b"<</Key/Value>>",
            b"<</Key[/Val1/Val2\x0d%gopher\x0atrue]>>",
            b"[<</k1[/name1]>><</k1[false true null]>>]",
            b"/Name ",
            b"/Na#20me",
            b"[null]abc",
            b"/",
            b"/(",
            b"//",
            b"/abc/",
            b"/abc",
            b"/abc/def",
            b"%comment\x0D<c0c>%\x0a",
            b"[<0ab>%comment\x0a]",
            b"<</Key1<abc>/Key2<def>>>",
            b"<< /Key1 <abc> /Key2 <def> >>",
            b"<</Key1<AB>>>",
            b"<</Key1<ABC>>>",
            b"<</Key1<0ab>>>",
            b"<</Key<>>>",
            b"<>",
            b"()",
            b"(gopher\\\x28go)",
            b"(gop\x0aher\\(go)",
            b"(go\\pher\\)go)",
            b"[%comment\x0d(gopher\\ago)%comment\x0a]",
            b"<</K(gopher)>>",
            b"[(abc)true/n1<20f>]..",
            b"[(abc)()]..",
            b"[<743EEC2AFD93A438D87F5ED3D51166A8><B7FFF0ADB814244ABD8576D07849BE54>]",
            b"1",
            b"1/",
            b"3.43",
            b"3.43<",
            b"1.2",
            b"[<0ab>]",
            b"1 0 R%comment\x0a",
            b"[1 0 R /n 2 0 R]",
            b"<</n 1 0 R>>",
            b"(!\\(S:\\356[\\272H\\355>>R{sb\\007)",
            b"18446744071963345064 0 R",
        ];
        for input in good {
            let (r, _) = run(input);
            assert!(r.is_ok(), "{:?}: {r:?}", String::from_utf8_lossy(input));
        }
        let bad: Vec<&[u8]> = vec![b"/Na#me", b"/Na#2me"];
        for input in bad {
            let e = fails(input);
            assert!(e.is(Sentinel::NameObjectCorrupt), "{e}");
        }
        assert_eq!(fails(b""), err_buf_not_available());
        assert_eq!(fails(b"   "), err_buf_not_available());
    }

    /// Go: TestParseArray.
    #[test]
    fn parse_array_cases() {
        // 30000 x null, and references.
        let nulls = "null ".repeat(10_000);
        let refs = "15538 0 R 15538 0 R 15539 0 R null 15541 0 R ".repeat(500);
        let big = format!("[{nulls}{nulls}{nulls}{refs}]");
        let Object::Array(a) = ok(big.as_bytes()) else {
            panic!("not an array");
        };
        assert_eq!(a.len(), 30_000 + 2500);

        let good: Vec<&[u8]> = vec![
            b"[]",
            b"[     ]",
            b"[  ]abc",
            b"[a]",
            b"[<0ab> <bcf098>a]",
            b"[<0ab> <bcf098> a]",
            b"[<AB >]",
            b"[<     AB >]",
            b"[<abc><def>]",
            b"[<AB>]",
            b"[<ABC>]",
            b"[<0ab> <bcf098>]",
            b"[<0ab>]",
            b"[<0abc>]",
            b"[<0abc> <345bca> <aaf>]",
            b"[<01 05 02 02 03 00 00 00 00 00 00 00>]",
            b"[< 0 0 2 6 6 6 5 6 5 2 2 4>]",
            b"[(abc) (def) <20ff>]..",
            b"[(abc)()]..",
            b"[<743EEC2AFD93A438D87F5ED3D51166A8><B7FFF0ADB814244ABD8576D07849BE54>]",
            b"[(abc(i)) <FFb> (<>)]",
            b"[(abc)<20ff>]..",
            b"[/N[/A/B/C]]",
            b"[<</Obj 71 0 R/Type/OBJR>>269 0 R]",
            b"[/Name 123<</A 123 /B<c0ff>>>]",
            b"[/DictName<</A 123 /B<c0ff>>>]",
            b"[/DictName<</A 123 /B<c0ff>/C[(Go!)]>>]",
            b"[/Name<<>>]",
            b"[/Name<</Sub 1>>]",
            b"[/Name<</Sub<BBB>>>]",
            b"[/Name<</Sub[1]>>]",
            b"[/CalRGB<</Matrix[0.41239 0.21264]/Gamma[2.22 2.22 2.22]/WhitePoint[0.95043 1 1.09]>>]",
            b"[/]",
            b"[/ ]",
            b"[/N]",
            b"[/Name]",
            b"[/First /Last]",
            b"[ /First/Last]",
            b"[ /First/Last  ]",
            b"[/PDF/ImageC/Text]",
            b"[/PDF /Text /ImageB /ImageC /ImageI]",
            b"[<004>]/Name[(abc)]]",
            b"[0.000000-16763662]",
            b"[1.09 2.00056]",
            b"[1.09 null true false [/Name1 2.00056]]",
            b"[[2.22 2.22\t2.22][0.95043 1 1.09]]",
            b"[1]",
            b"[1.01]",
            b"[1 null]",
            b"[1 true[2 0]]",
            b"[1.09 2.00056]]",
            b"[0 0 841.89 595.28]",
            b"[ 487 190]",
            b"[0.95043 1 1.09]",
            b"[1 true[2 0 R 14 0 R]]",
            b"[22 0 1 R 14 23 0 R 24 0 R 25 0 R 27 0 R 28 0 R 29 0 R 31 0 R]",
            b"[/Name<< /Sub /Value>>]",
        ];
        for input in good {
            let (r, _) = run(input);
            assert!(r.is_ok(), "{:?}: {r:?}", String::from_utf8_lossy(input));
        }
        let bad: Vec<&[u8]> = vec![
            b"[",
            b"[            ",
            b"[<",
            b"[<>",
            b"[<    >",
            b"[   <",
            b"[   <>",
            b"[   <    >",
            b"[<ABG>",
            b"[<FEFF ABC2>",
            b"[<   ABG>",
            b"[<AB>",
            b"[<ABc>    ",
            b"[<0ab> <bcf098>",
        ];
        for input in bad {
            let (r, _) = run(input);
            assert!(r.is_err(), "{:?} parsed", String::from_utf8_lossy(input));
        }
    }

    /// Go: TestParseDict.
    #[test]
    fn parse_dict_cases() {
        let good: Vec<&[u8]> = vec![
            b"<</Type /Pages /Count 24 /Kids [6 0 R 16 0 R 21 0 R 27 0 R] /Parent 2 0 R>>",
            b"<< /Key1 <abc> /Key2 <d> >>",
            b"<<>>",
            b"<<     >>",
            b"<</Key1/Value1/key1/Value2>>",
            b"<</Type/Page/Parent 2 0 R/Resources<</Font<</F1 5 0 R/F2 7 0 R/F3 9 0 R>>/XObject<</Image11 11 0 R>>/ProcSet[/PDF/Text/ImageB/ImageC/ImageI]>>/MediaBox[ 0 0 595.32 841.92]/Contents 4 0 R>>",
            b"<</Title \x0a/Type /Outline\x0a/Key /Value>>",
            b"<</Key1 /Value1\x0a/Title \x0a/Type /Outline\x0a/Key /Value>>",
            b"<</S/A>>",
            b"<</K1 / /K2 /Name2>>",
            b"<</Key/Value>>",
            b"<< /Key\t/Value>>",
            b"<<\t/Key/Value\t>>",
            b"<<\t/Key\t/Value\t>>",
            b"<</Key1/Value1/Key2/Value2>>",
            b"<</Key1(abc)/Key2(def)>>..",
            b"<</Key1(abc(inner1<<>>inner2)def)    >>..",
            b"<</Key<ABG>>>",
            b"<</Key<   ABG>>>",
            b"<</Key<0ab><bcf098>",
            b"<</Key1<abc>/Key2<def>>>",
            b"<< /Key1 <abc> /Key2 <def> >>",
            b"<</Key1<AB>>>",
            b"<</Key1<ABC>>>",
            b"<</Key1<0ab>>>",
            b"<</Key<>>>",
            b"<< /Panose <01 05 02 02 03 00 00 00 00 00 00 00> >>",
            b"<< /Panose < 0 0 2 6 6 6 5 6 5 2 2 4> >>",
            b"<</Key <FEFF ABC2>>>",
            b"<</Key<</Sub1 1/Sub2 2>>>>",
            b"<</Key<</Sub1(xyz)>>>>",
            b"<</Key<</Sub1[]>>>>",
            b"<</Key<</Sub1[1]>>>>",
            b"<</Key<</Sub1[(Go)]>>>>",
            b"<</Key<</Sub1[(Go)]/Sub2[(rocks!)]>>>>",
            b"<</A[/B1 /B2<</C 1>>]>>",
            b"<</A[/B1 /B2<</C 1>> /B3]>>",
            b"<</Name1[/CalRGB<</Matrix[0.41239 0.21264]/Gamma[2.22 2.22 2.22]/WhitePoint[0.95043 1 1.09]>>]>>",
            b"<</A[/DictName<</A 123 /B<c0ff>>>]>>",
            b"<</A[/B]>>",
            b"<</Key1[<abc><def>12.24 (gopher)]>>",
            b"<</Key1[<abc><def>12.24 (gopher)] /Key2[(abc)2.34[<c012>2 0 R]]>>",
            b"<</Key1[1 2 3 [4]]>>",
            b"<</K[<</Obj 71 0 R/Type/OBJR>>269 0 R]/P 258 0 R/S/Link/Pg 19 0 R>>",
            b"<</Key1 true>>",
            b"<</Key1 \t\t\tfalse>>",
            b"<</Key1 null /Key2 true /Key3 false>>",
            b"<</Key1 TRUE>>",
            b"<</Key1 FAlse>>",
            b"<</Key1 16>>",
            b"<</Key1 .034>>",
            b"<</Key1 ,034>>",
            b"<</Key1 32 0 R>>",
            b"<</Key1 32 0 R/Key2 32 /Key3 3.34>>",
            b"<</Root 1 0 R/Info%comment after name\n<</Subject(Compacted Syntax v3.0)%comment after literal string end\n/Title<436f6d7061637465642073796e746178>%comment after hex string end\n/Keywords(PDF,Compacted,Syntax,ISO 32000-2:2020)/CreationDate(D:20200317)/Author(Peter Wyatt)/Creator<48616e642d65646974>/Producer<48616e642d65646974>>>/ID[<18D6B641245C03FABE67D93AD879D6EC><6264992C92074533A46A019C7CF9B6A2>]>>",
            b"<</Type/Page/Parent 3 0 R/MediaBox[%comment after array start token\n+0 .0 999 999.]%comment after array end token\n/CropBox[+0 .0 999%comment after an integer\n999.]/Contents[5 0 R]/UserUnit +0.88/Annots null%comment after null\n/Resources<</Pattern<<>>/ProcSet[null]/ExtGState<</ 6 0 R>>/Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Times-Bold/Encoding/WinAnsiEncoding>>>>>>>>",
        ];
        for input in good {
            let (r, _) = run(input);
            assert!(r.is_ok(), "{:?}: {r:?}", String::from_utf8_lossy(input));
        }
        let bad: Vec<&[u8]> = vec![
            b"<<",
            b"<<>",
            b"<</Key<>>",
            b"<</Key<a4>>",
            b"<</Key<    >",
            b"<</Key<ade>",
        ];
        for input in bad {
            let (r, _) = run(input);
            assert!(r.is_err(), "{:?} parsed", String::from_utf8_lossy(input));
        }

        // A large dictionary.
        let mut big = String::from("<</Key#28#29 (Value)");
        for i in 0..50_000 {
            big.push_str(&format!("/Key{i} (Value)"));
        }
        big.push_str(">>");
        let Object::Dict(d) = ok(big.as_bytes()) else {
            panic!("not a dictionary");
        };
        assert_eq!(d.len(), 50_001);
    }

    /// Go: TestParseObjectWithPolicyDictionaryKeyFallback, relaxed mode.
    #[test]
    fn policy_dictionary_key_fallback() {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = b"<<@/Type /Catalog>>";
        let r = parse_object_with_policy(&mut cx, &mut line, 0, None).unwrap();
        let failure = r.strict_failure.expect("the strict failure is kept");
        assert!(is_dictionary_key_error(&failure));
        assert!(failure.is(Sentinel::NameObjectCorrupt));
        assert!(failure.to_string().contains("corrupt dictionary key"));
        let Object::Dict(d) = r.object else {
            panic!("not a dictionary");
        };
        assert_eq!(d.name_entry(b"Type"), Some(&b"Catalog"[..]));
    }

    /// Go: TestParseObjectWithPolicyReportsFailedRelaxedAttempt.
    #[test]
    fn policy_reports_failed_relaxed_attempt() {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = b"<<@/Type /Catalog /Broken (unterminated>>";
        let e = parse_object_with_policy(&mut cx, &mut line, 0, None).unwrap_err();
        assert_eq!(e, err_string_literal_corrupt());
        assert!(e.to_string().contains("corrupt string literal"));
    }

    /// Go: TestParseObjectWithPolicyPreservesUnclassifiedFallback.
    #[test]
    fn policy_preserves_unclassified_fallback() {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        let mut line: &[u8] = b"<</Missing\n/Type /Catalog>>";
        let r = parse_object_with_policy(&mut cx, &mut line, 0, None).unwrap();
        assert!(r.strict_failure.is_none());
        let Object::Dict(d) = r.object else {
            panic!("not a dictionary");
        };
        assert_eq!(d.string_entry(b"Missing"), Some(&b""[..]));
        assert_eq!(d.name_entry(b"Type"), Some(&b"Catalog"[..]));

        // A clean parse has no strict failure either.
        let mut line: &[u8] = b"<</Type /Catalog>>";
        let r = parse_object_with_policy(&mut cx, &mut line, 0, None).unwrap();
        assert!(r.strict_failure.is_none());
    }

    #[test]
    fn dictionary_errors_are_classified() {
        assert!(is_dictionary_corrupt(&fails(b"<<>")));
        assert!(is_dictionary_corrupt(&fails(b"<</Key<a4>>")));
        assert!(!is_dictionary_corrupt(&fails(b"[")));
        assert!(!is_dictionary_key_error(&fails(b"[")));
    }

    fn xref_sd(entries: Vec<(&str, Object)>) -> StreamDict {
        StreamDict {
            dict: dict(entries),
            ..StreamDict::default()
        }
    }

    /// Go: TestParseXRefStreamDictRejectsSizeLimit.
    #[test]
    fn xref_stream_dict_rejects_size_limit() {
        let sd = xref_sd(vec![("Size", Object::Integer(3)), ("W", w111())]);
        let limits = Limits {
            max_object_count: 2,
            max_xref_entries: 10,
            max_object_stream_count: 1,
            max_object_stream_first: 1,
            ..Limits::default()
        };
        let e = parse_xref_stream_dict_with_limits(&sd, &limits).unwrap_err();
        assert!(e.to_string().contains("Size"), "{e}");
    }

    #[test]
    fn xref_stream_object_list_is_charged_before_it_is_allocated() {
        let mem = crate::ledger::Mem::with_max(3 * 8);
        let mut held = mem.hold();
        let sd = xref_sd(vec![("Size", Object::Integer(4)), ("W", w111())]);
        let e = parse_xref_stream_dict_charged(&sd, &Limits::default(), &mut held).unwrap_err();
        assert!(matches!(e, Error::Alloc { got: 32, max: 24 }), "{e}");
        let sd = xref_sd(vec![("Size", Object::Integer(3)), ("W", w111())]);
        let x = parse_xref_stream_dict_charged(&sd, &Limits::default(), &mut held).unwrap();
        assert_eq!(x.objects, vec![0, 1, 2]);
        assert_eq!(mem.used(), 24);
    }

    /// Go: TestParseXRefStreamDictRequiresDirectSize.
    #[test]
    fn xref_stream_dict_requires_direct_size() {
        let sd = xref_sd(vec![
            (
                "Size",
                Object::Ref(Ref {
                    num: 7,
                    generation: 0,
                }),
            ),
            ("W", w111()),
        ]);
        let e = parse_xref_stream_dict_with_limits(&sd, &Limits::default()).unwrap_err();
        assert!(e.to_string().contains("\"Size\" not available"), "{e}");
    }

    #[test]
    fn xref_stream_dict_fields() {
        let sd = xref_sd(vec![
            ("Size", Object::Integer(4)),
            ("Prev", Object::Integer(116)),
            (
                "W",
                Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(3),
                    Object::Integer(0),
                ]),
            ),
        ]);
        let x = parse_xref_stream_dict_with_limits(&sd, &Limits::default()).unwrap();
        assert_eq!(x.size, 4);
        assert_eq!(x.objects, vec![0, 1, 2, 3]);
        assert_eq!(x.w, [1, 3, 0]);
        assert_eq!(x.previous_offset, Some(116));

        for w in [
            None,
            Some(Object::Array(vec![Object::Integer(1), Object::Integer(1)])),
            Some(Object::Array(vec![
                Object::Integer(1),
                Object::Integer(-1),
                Object::Integer(1),
            ])),
            Some(Object::Array(vec![
                Object::Integer(1),
                Object::Name(b"x".to_vec()),
                Object::Integer(1),
            ])),
        ] {
            let mut entries = vec![("Size", Object::Integer(4))];
            if let Some(w) = w {
                entries.push(("W", w));
            }
            let sd = xref_sd(entries);
            assert!(parse_xref_stream_dict_with_limits(&sd, &Limits::default()).is_err());
        }

        // No Size, a Size of 0, and, relaxed, a Size of 0 with an Index.
        let sd = xref_sd(vec![("W", w111())]);
        assert!(parse_xref_stream_dict_with_limits(&sd, &Limits::default()).is_err());
        let sd = xref_sd(vec![("Size", Object::Integer(0)), ("W", w111())]);
        assert!(parse_xref_stream_dict_with_limits(&sd, &Limits::default()).is_err());
        let sd = xref_sd(vec![
            ("Size", Object::Integer(0)),
            (
                "Index",
                Object::Array(vec![Object::Integer(5), Object::Integer(2)]),
            ),
            ("W", w111()),
        ]);
        assert!(parse_xref_stream_dict_with_limits(&sd, &Limits::default()).is_err());
        let x = parse_xref_stream_dict_relaxed_with_limits(&sd, &Limits::default()).unwrap();
        assert_eq!((x.size, x.objects.clone()), (7, vec![5, 6]));
    }

    /// Go: TestParseXRefStreamDictAllocatesForIndexedEntries.
    #[test]
    fn xref_stream_dict_allocates_for_indexed_entries() {
        let sd = xref_sd(vec![
            ("Size", Object::Integer(1_000_000)),
            (
                "Index",
                Object::Array(vec![Object::Integer(0), Object::Integer(4)]),
            ),
            ("W", w111()),
        ]);
        let limits = Limits {
            max_object_count: 1_000_000,
            max_xref_entries: 4,
            ..Limits::default()
        };
        for relaxed in [false, true] {
            let x = if relaxed {
                parse_xref_stream_dict_relaxed_with_limits(&sd, &limits)
            } else {
                parse_xref_stream_dict_with_limits(&sd, &limits)
            }
            .unwrap();
            assert_eq!(x.objects, vec![0, 1, 2, 3]);
            // Capacity for the 4 entries, not for the declared range of a million.
            assert!(x.objects.capacity() < 64, "{}", x.objects.capacity());
        }
        // One entry more than the limit.
        let sd = xref_sd(vec![
            ("Size", Object::Integer(1_000_000)),
            (
                "Index",
                Object::Array(vec![Object::Integer(0), Object::Integer(5)]),
            ),
            ("W", w111()),
        ]);
        assert!(parse_xref_stream_dict_with_limits(&sd, &limits).is_err());
    }

    /// Go: TestParseXRefStreamDictRepairsIndexSizeMismatch.
    #[test]
    fn xref_stream_dict_repairs_index_size_mismatch() {
        let sd = xref_sd(vec![
            ("Size", Object::Integer(6)),
            (
                "Index",
                Object::Array(vec![Object::Integer(0), Object::Integer(7)]),
            ),
            ("W", w111()),
        ]);
        let mut limits = Limits::default();
        let e = parse_xref_stream_dict_with_limits(&sd, &limits).unwrap_err();
        assert!(e.is(Sentinel::XRefStreamIndexSizeMismatch));

        let x = parse_xref_stream_dict_relaxed_with_limits(&sd, &limits).unwrap();
        assert_eq!((x.size, x.objects.len()), (7, 7));

        limits.max_object_count = 6;
        assert!(parse_xref_stream_dict_relaxed_with_limits(&sd, &limits).is_err());
    }

    #[test]
    fn xref_stream_index_errors() {
        let w = ("W", w111());
        // An odd number of entries, a non-integer, a negative start or count.
        for index in [
            vec![Object::Integer(0)],
            vec![Object::Integer(0), Object::Name(b"x".to_vec())],
            vec![Object::Integer(-1), Object::Integer(2)],
            vec![Object::Integer(0), Object::Integer(-2)],
        ] {
            let sd = xref_sd(vec![
                ("Size", Object::Integer(5)),
                ("Index", Object::Array(index)),
                w.clone(),
            ]);
            let e =
                parse_xref_stream_dict_relaxed_with_limits(&sd, &Limits::default()).unwrap_err();
            assert_eq!(e, err_xref_stream_corrupt_index());
        }
        // Several ranges.
        let sd = xref_sd(vec![
            ("Size", Object::Integer(10)),
            (
                "Index",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(2),
                    Object::Integer(7),
                    Object::Integer(2),
                ]),
            ),
            w,
        ]);
        let x = parse_xref_stream_dict_with_limits(&sd, &Limits::default()).unwrap();
        assert_eq!(x.objects, vec![0, 1, 7, 8]);
        assert_eq!(x.size, 10);
    }

    /// Go: TestObjectStreamDictRejectsLimits.
    #[test]
    fn object_stream_dict_rejects_limits() {
        let mut sd = xref_sd(vec![
            ("Type", Object::name("ObjStm")),
            ("N", Object::Integer(3)),
            ("First", Object::Integer(10)),
        ]);
        let limits = Limits {
            max_object_stream_count: 2,
            max_object_stream_first: 20,
            ..Limits::default()
        };
        let e = object_stream_dict_with_limits(&sd, &limits).unwrap_err();
        assert!(e.to_string().contains('N'), "{e}");

        sd.dict.set(b"N", Object::Integer(2));
        let limits = Limits {
            max_object_stream_count: 2,
            max_object_stream_first: 9,
            ..Limits::default()
        };
        let e = object_stream_dict_with_limits(&sd, &limits).unwrap_err();
        assert!(e.to_string().contains("First"), "{e}");

        let limits = Limits {
            max_object_stream_count: 2,
            max_object_stream_first: 10,
            max_decode_bytes: 77,
            ..Limits::default()
        };
        let osd = object_stream_dict_with_limits(&sd, &limits).unwrap();
        assert_eq!((osd.obj_count, osd.first_obj_offset), (2, 10));
        assert_eq!(osd.max_decode_bytes, 77);
        assert!(osd.obj_array.is_empty());

        // Missing and non-positive entries.
        sd.dict.delete(b"First");
        assert!(object_stream_dict_with_limits(&sd, &limits).is_err());
        sd.dict.set(b"First", Object::Integer(0));
        sd.dict.set(b"N", Object::Integer(0));
        assert!(object_stream_dict_with_limits(&sd, &limits).is_err());
        sd.dict.delete(b"N");
        assert!(object_stream_dict_with_limits(&sd, &limits).is_err());
    }

    /// Go: TestObjectStreamDictWithResolvedIntegersPreservesIndirectEntries.
    #[test]
    fn object_stream_dict_with_resolved_integers_preserves_indirect_entries() {
        let n_ref = Object::Ref(Ref {
            num: 7,
            generation: 0,
        });
        let first_ref = Object::Ref(Ref {
            num: 8,
            generation: 0,
        });
        let sd = xref_sd(vec![
            ("Type", Object::name("ObjStm")),
            ("N", n_ref),
            ("First", first_ref),
        ]);
        let osd =
            object_stream_dict_with_resolved_integers(&sd, &Limits::default(), Some(3), Some(12))
                .unwrap();
        assert_eq!((osd.obj_count, osd.first_obj_offset), (3, 12));
        assert!(matches!(
            sd.dict.find(b"N"),
            Some(Object::Ref(Ref {
                num: 7,
                generation: 0
            }))
        ));
        assert!(matches!(
            sd.dict.find(b"First"),
            Some(Object::Ref(Ref {
                num: 8,
                generation: 0
            }))
        ));
        assert!(
            object_stream_dict_with_resolved_integers(&sd, &Limits::default(), None, Some(1))
                .is_err()
        );
    }

    fn detect(s: &[u8]) -> (i64, i64) {
        let cancel = AtomicBool::new(false);
        detect_keywords(&cancel, s).unwrap()
    }

    /// Go: TestDetectKeywords.
    #[test]
    fn detect_keywords_skips_strings_and_comments() {
        // process: # gen obj ... obj dict ... {stream ... data ... endstream} endobj
        //                                    streamInd                        endInd
        //                                  -1 if absent                    -1 if absent
        let s =
            b"1 0 obj\n<<\n /Lang (en-endobject-stream-UK%)  % comment \n>>\nendobj\n\n2 0 obj\n";
        assert_eq!(detect(s).0, 59);

        // Negative test.
        let s = b"1 0 obj\n<<\n /Lang (en-endobject-stream-UK%)  % endobject";
        assert!(detect(s).0 <= 0);
        assert_eq!(detect(s), (-1, -1));
    }

    #[test]
    fn detect_keywords_finds_stream_and_endobj() {
        let s = "1 0 obj\n<</Length 3>>\nstream\nxyz\nendstream\nendobj\n";
        let want_stream = s.find("stream").unwrap() as i64;
        let want_end = s.find("endobj").unwrap() as i64;
        assert_eq!(detect(s.as_bytes()), (want_end, want_stream));

        // No stream: only the end.
        let s = "5 0 obj\n<</A 1>>\nendobj\n";
        assert_eq!(detect(s.as_bytes()), (s.find("endobj").unwrap() as i64, -1));

        // A keyword at the very end of the buffer is not followed by whitespace yet.
        assert_eq!(detect(b"5 0 obj\n<<>>\nendobj"), (-1, -1));
        assert_eq!(detect(b"5 0 obj\n<<>>\nstream"), (-1, -1));

        // Markers inside a string or a comment are skipped.
        let s = "5 0 obj\n<</A (endobj\n)>>\n% stream\nendobj\n";
        let last = s.rfind("endobj").unwrap() as i64;
        assert_eq!(detect(s.as_bytes()), (last, -1));

        // `endobj` followed by `xref` is accepted even if the marker's neighbour is not blank.
        let s = b"5 0 obj\n<<>>\nendobjxref\n";
        assert_eq!(detect(s), (13, -1));
    }

    #[test]
    fn detect_keywords_errors() {
        let cancel = AtomicBool::new(false);
        // A string that is not closed, before a marker is found past it.
        let e = detect_keywords(&cancel, b"5 0 obj\n<</A (unclosed\nendobj\n").unwrap_err();
        assert_eq!(e, err_string_literal_corrupt());
        let cancel = AtomicBool::new(true);
        assert_eq!(
            detect_keywords(&cancel, b"endobj\n").unwrap_err(),
            Error::Cancelled
        );
    }
}
