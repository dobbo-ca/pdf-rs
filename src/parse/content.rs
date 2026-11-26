//! The content-stream parser (Go: pkg/pdfcpu/model/parseContent.go). It does not build an
//! operator list. It finds the names of the resources a content stream uses, so that
//! `ConsolidatePageResources` can drop the ones it does not.
//!
//! Go works on `string`s and cuts them with `[i:]`; here every function takes a `&[u8]` and
//! returns or updates the remainder, so nothing is copied except the rare operator that is
//! glued to its successor (the `pre` text of [`parse_content`]).
//!
//! Differences from Go:
//!
//! * The errors `errPageContentCorrupt`, `errTJExpressionCorrupt` and `errBIExpressionCorrupt`
//!   are [`Sentinel`]s; the dictionary and string errors are [`Error::Corrupt`] with the
//!   message of the parser's own errors, so the predicates of `parse` see them.
//! * Go's `log.Parse` tracing and `ShowSkipped` are not ported.
//! * [`PageResourceNames`] lives in `model/resources.rs` with the rest of Go's `resource.go`.

use super::{
    ERR_DICTIONARY_CORRUPT, ERR_STRING_LITERAL_CORRUPT, balanced_parentheses_prefix, delimiter,
    parse_name, pdf_whitespace, position_to_next_eol, position_to_next_whitespace_or_char,
    trim_left_space,
};
use crate::error::{Error, Sentinel};
use crate::ledger::{Held, Mem};
use crate::model::resources::PageResourceNames;
use crate::object::string::decode_name;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

const HEX_LITERAL_CORRUPT: &str = "parse: corrupt hex literal";

/// The names found so far and their charge to the ledger.
struct Names {
    prn: PageResourceNames,
    held: Held,
}

impl Names {
    /// Adds `name` to the set for `kind`. A new name is charged before it is copied, as a
    /// dictionary key is (`entry_copy_bytes`): two slots of a half-full B-tree node, 16 for the
    /// internal nodes, and the name.
    fn insert(&mut self, kind: &'static str, name: &[u8]) -> Result<(), Error> {
        if self.prn.0.get(kind).is_some_and(|set| set.contains(name)) {
            return Ok(());
        }
        let slot = 2 * size_of::<Vec<u8>>() as u64 + 16;
        self.held.grow_by(slot.saturating_add(name.len() as u64))?;
        self.prn.insert(kind, name.to_vec());
        Ok(())
    }
}

const EMPTY: &[u8] = &[];

fn err_dictionary_corrupt() -> Error {
    Error::Corrupt(ERR_DICTIONARY_CORRUPT.to_string())
}

fn err_string_literal_corrupt() -> Error {
    Error::Corrupt(ERR_STRING_LITERAL_CORRUPT.to_string())
}

fn err_hex_literal_corrupt() -> Error {
    Error::Corrupt(HEX_LITERAL_CORRUPT.to_string())
}

fn err_page_content_corrupt() -> Error {
    Error::Sentinel(Sentinel::PageContentCorrupt)
}

fn err_tj_expression_corrupt() -> Error {
    Error::Sentinel(Sentinel::TJExpressionCorrupt)
}

fn err_bi_expression_corrupt() -> Error {
    Error::Sentinel(Sentinel::BIExpressionCorrupt)
}

/// `strings.TrimLeftFunc(s, whitespaceOrEOL)`.
fn trim_ws(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&b| pdf_whitespace(b)).count();
    &s[n..]
}

/// `positionToNextEOL`'s remainder: `s` from its first line ending, empty if there is none.
fn from_eol(s: &[u8]) -> &[u8] {
    match position_to_next_eol(s) {
        Some(i) => &s[i..],
        None => EMPTY,
    }
}

/// `strings.Index(s, "~>")`.
fn find_a85_eod(s: &[u8]) -> Option<usize> {
    s.windows(2).position(|w| w == b"~>")
}

fn skip_dict(l: &mut &[u8]) -> Result<(), Error> {
    let mut s = *l;
    if !s.starts_with(b"<<") {
        return Err(err_dictionary_corrupt());
    }
    s = &s[2..];
    let mut depth: u64 = 1;
    while !s.is_empty() {
        let Some(i) = s
            .iter()
            .position(|&b| matches!(b, b'<' | b'>' | b'(' | b'%'))
        else {
            return Err(err_dictionary_corrupt());
        };
        s = &s[i..];
        match s[0] {
            b'(' => {
                // Delimiters within strings do not affect dictionary nesting (7.3.4).
                if skip_string_literal(&mut s).is_err() {
                    return Err(err_dictionary_corrupt());
                }
            }
            b'%' => s = from_eol(s),
            b'<' => {
                if s.starts_with(b"<<") {
                    depth += 1;
                    s = &s[2..];
                } else if skip_hex_string_literal(&mut s).is_err() {
                    return Err(err_dictionary_corrupt());
                }
            }
            _ => {
                if !s.starts_with(b">>") {
                    return Err(err_dictionary_corrupt());
                }
                depth -= 1;
                s = &s[2..];
                if depth == 0 {
                    *l = s;
                    return Ok(());
                }
            }
        }
    }
    Err(err_dictionary_corrupt())
}

fn skip_string_literal(l: &mut &[u8]) -> Result<(), Error> {
    // Literal strings may contain balanced, unescaped parentheses (7.3.4.2).
    let s = *l;
    if !s.starts_with(b"(") {
        return Err(err_string_literal_corrupt());
    }
    let Some(i) = balanced_parentheses_prefix(s) else {
        return Err(err_string_literal_corrupt());
    };
    *l = &s[i + 1..];
    Ok(())
}

fn skip_hex_string_literal(l: &mut &[u8]) -> Result<(), Error> {
    let s = *l;
    let Some(i) = s.iter().position(|&b| b == b'>') else {
        return Err(err_hex_literal_corrupt());
    };
    *l = &s[i + 1..];
    Ok(())
}

fn skip_tj(l: &mut &[u8]) -> Result<(), Error> {
    // Each element shall be either a string or a number.
    let mut s = *l;
    loop {
        s = trim_ws(s);
        match s.first() {
            None => return Err(err_tj_expression_corrupt()),
            Some(b']') => {
                *l = &s[1..];
                return Ok(());
            }
            Some(b'(') => {
                skip_string_literal(&mut s)?;
                continue;
            }
            Some(b'<') => {
                skip_hex_string_literal(&mut s)?;
                continue;
            }
            Some(_) => {}
        }
        let Some(i) = position_to_next_whitespace_or_char(s, b"<(]") else {
            return Err(err_tj_expression_corrupt());
        };
        s = &s[i..];
    }
}

/// `relaxedEIEnd`: the legacy tolerance for omitted whitespace before EI, as in issue 1080.
fn relaxed_ei_end(s: &[u8]) -> bool {
    let Some(&first) = s.first() else {
        return true;
    };
    if !pdf_whitespace(first) {
        return false;
    }
    let s = trim_ws(s);
    match s {
        [] => true,
        [b'Q', rest @ ..] => match rest.first() {
            None => true,
            Some(&c) => pdf_whitespace(c) || delimiter(c) || c == b'%',
        },
        _ => false,
    }
}

/// `inlineImageEndBoundary`: the `EI` at `i` ends a token. `i + 2` is at most `s.len()`.
fn inline_image_end_boundary(s: &[u8], i: usize) -> bool {
    match s.get(i + 2) {
        None => true,
        Some(&c) => pdf_whitespace(c) || delimiter(c) || c == b'%',
    }
}

/// `inlineImageEnd`: the `EI` at `i` (`i >= 1`, `i + 2 <= s.len()`) ends the inline image.
fn inline_image_end(s: &[u8], i: usize, relaxed: bool) -> bool {
    if !pdf_whitespace(s[i - 1]) {
        return relaxed && relaxed_ei_end(&s[i + 2..]);
    }
    inline_image_end_boundary(s, i)
}

fn lookup_ascii85_ei(s: &[u8], relaxed: bool) -> Result<usize, Error> {
    // ASCII85 image bytes may contain EI tokens; only ~> terminates encoded data (7.4.3).
    let Some(i) = find_a85_eod(s) else {
        return Err(err_bi_expression_corrupt());
    };
    let tail = trim_ws(&s[i + 2..]);
    let i = s.len() - tail.len();
    if tail.starts_with(b"EI") && inline_image_end(s, i, relaxed) {
        return Ok(i);
    }
    Err(err_bi_expression_corrupt())
}

fn ascii_filter(first_filter: &[u8]) -> bool {
    matches!(
        first_filter,
        b"ASCII85Decode" | b"A85" | b"ASCIIHexDecode" | b"AHx"
    )
}

fn inline_image_data_start(s: &[u8], first_filter: &[u8]) -> Result<usize, Error> {
    if s.len() < 3 || !s.starts_with(b"ID") || !pdf_whitespace(s[2]) {
        return Err(err_bi_expression_corrupt());
    }
    let mut start = 3;
    if s[2] == b'\r' && s.get(start) == Some(&b'\n') {
        start += 1;
    }
    if ascii_filter(first_filter) {
        while start < s.len() && pdf_whitespace(s[start]) {
            start += 1;
        }
    }
    Ok(start)
}

fn lookup_length_ei(s: &[u8], length: i64, first_filter: &[u8]) -> Result<usize, Error> {
    let start = inline_image_data_start(s, first_filter)?;
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length > s.len() - start {
        return Err(err_bi_expression_corrupt());
    }
    let mut i = start + length;
    while i < s.len() && pdf_whitespace(s[i]) {
        i += 1;
    }
    if i + 2 > s.len() || &s[i..i + 2] != b"EI" || !inline_image_end_boundary(s, i) {
        return Err(err_bi_expression_corrupt());
    }
    Ok(i)
}

fn lookup_ei(
    s: &[u8],
    relaxed: bool,
    first_filter: &[u8],
    length: Option<i64>,
) -> Result<usize, Error> {
    if let Some(n) = length {
        return lookup_length_ei(s, n, first_filter);
    }
    if first_filter == b"ASCII85Decode" || first_filter == b"A85" {
        return lookup_ascii85_ei(s, relaxed);
    }
    // Any content operator may follow EI (8.9.7). Do not skip ahead to a later EI Q.
    let mut i = 3;
    while i + 2 <= s.len() {
        if &s[i..i + 2] == b"EI" && inline_image_end(s, i, relaxed) {
            return Ok(i);
        }
        i += 1;
    }
    Err(err_bi_expression_corrupt())
}

const BI_NAME_STOP: &[u8] = b"/[]<>()%";

/// `skipBIColorSpaceArray`: skips an Indexed colour space and its string lookup table (8.9.7).
fn skip_bi_color_space_array<'a>(s: &'a [u8], prn: &mut Names) -> Result<&'a [u8], Error> {
    let mut s = &s[1..];
    loop {
        s = trim_left_space(s, false).0;
        let Some(&first) = s.first() else {
            return Err(err_bi_expression_corrupt());
        };
        match first {
            b']' => return Ok(&s[1..]),
            b'(' => skip_string_literal(&mut s)?,
            b'<' => skip_hex_string_literal(&mut s)?,
            b'/' => s = skip_bi_color_space_name(s, prn)?,
            _ => match position_to_next_whitespace_or_char(s, BI_NAME_STOP) {
                Some(i) if i > 0 => s = &s[i..],
                _ => return Err(err_bi_expression_corrupt()),
            },
        }
    }
}

fn skip_bi_color_space_name<'a>(s: &'a [u8], prn: &mut Names) -> Result<&'a [u8], Error> {
    let s = trim_left_space(s, false).0;
    if s.starts_with(b"[") {
        return skip_bi_color_space_array(s, prn);
    }
    if s.len() < 2 || s[0] != b'/' {
        return Err(err_bi_expression_corrupt());
    }
    let s = &s[1..];
    let Some(i) = position_to_next_whitespace_or_char(s, BI_NAME_STOP) else {
        return Err(err_bi_expression_corrupt());
    };
    let Ok(name) = decode_name(&s[..i]) else {
        return Err(err_bi_expression_corrupt());
    };
    if !matches!(
        &*name,
        b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" | b"Indexed" | b"G" | b"RGB" | b"CMYK" | b"I"
    ) {
        prn.insert("ColorSpace", &name)?;
    }
    Ok(&s[i..])
}

/// `firstInlineImageFilter`: the name of the first filter of an inline image, empty if there is
/// none.
fn first_inline_image_filter(s: &[u8]) -> Result<Vec<u8>, Error> {
    let mut s = trim_left_space(s, false).0;
    if s.starts_with(b"[") {
        s = trim_left_space(&s[1..], false).0;
        if s.starts_with(b"]") {
            return Ok(Vec::new());
        }
    }
    parse_name(&mut s).map_err(|_| err_bi_expression_corrupt())
}

fn inline_image_length(s: &[u8], previous: Option<i64>) -> Result<i64, Error> {
    let s = trim_ws(s);
    let n = match position_to_next_whitespace_or_char(s, BI_NAME_STOP) {
        Some(i) if i > 0 => std::str::from_utf8(&s[..i])
            .ok()
            .and_then(|t| t.parse::<i64>().ok()),
        _ => None,
    };
    match n {
        Some(n) if n >= 0 && previous.is_none_or(|p| p == n) => Ok(n),
        _ => Err(err_bi_expression_corrupt()),
    }
}

#[derive(Default)]
struct InlineImageState {
    first_filter: Vec<u8>,
    length: Option<i64>,
}

fn skip_bi_entry<'a>(
    s: &'a [u8],
    prn: &mut Names,
    pdf20: bool,
    state: &mut InlineImageState,
) -> Result<&'a [u8], Error> {
    let s = &s[1..];
    let Some(i) = position_to_next_whitespace_or_char(s, b"/") else {
        return Err(err_bi_expression_corrupt());
    };
    match &s[..i] {
        b"L" | b"Length" => {
            if pdf20 {
                state.length = Some(inline_image_length(&s[i..], state.length)?);
            }
        }
        b"CS" | b"ColorSpace" => return skip_bi_color_space_name(&s[i..], prn),
        b"F" | b"Filter" => state.first_filter = first_inline_image_filter(&s[i..])?,
        _ => {}
    }
    Ok(&s[i..])
}

fn skip_bi(l: &mut &[u8], prn: &mut Names, relaxed: bool, pdf20: bool) -> Result<(), Error> {
    let mut s = *l;
    let mut state = InlineImageState::default();
    loop {
        let n = s
            .iter()
            .take_while(|&&b| pdf_whitespace(b) || b == b']')
            .count();
        s = &s[n..];
        let Some(&first) = s.first() else {
            return Err(err_bi_expression_corrupt());
        };
        if s.len() > 2 && s.starts_with(b"ID") && pdf_whitespace(s[2]) {
            if pdf20 && !relaxed && state.length.is_none() {
                return Err(err_bi_expression_corrupt());
            }
            let i = lookup_ei(s, relaxed, &state.first_filter, state.length)?;
            s = &s[i + 2..];
            break;
        }
        if first == b'/' {
            s = skip_bi_entry(s, prn, pdf20, &mut state)?;
            continue;
        }
        let Some(i) = position_to_next_whitespace_or_char(s, b"/]") else {
            return Err(err_bi_expression_corrupt());
        };
        s = &s[i..];
    }
    *l = s;
    Ok(())
}

fn skip_inline_image(
    l: &mut &[u8],
    prn: &mut Names,
    relaxed: bool,
    pdf20: bool,
) -> Result<bool, Error> {
    let s = *l;
    if !s.starts_with(b"BI") {
        return Ok(false);
    }
    if s.len() == 2 {
        return Err(err_bi_expression_corrupt());
    }
    if s[2] != b'/' && !pdf_whitespace(s[2]) {
        return Ok(false);
    }
    let mut s = &s[2..];
    skip_bi(&mut s, prn, relaxed, pdf20)?;
    *l = s;
    Ok(true)
}

/// `positionToNextContentToken`: skips whitespace, comments, text strings, TJ arrays and inline
/// images. `true` if nothing but those was left.
fn position_to_next_content_token(
    line: &mut &[u8],
    prn: &mut Names,
    relaxed: bool,
    pdf20: bool,
) -> Result<bool, Error> {
    let mut l = *line;
    loop {
        l = trim_ws(l);
        let Some(&first) = l.first() else {
            // Whitespace or eol only.
            return Ok(true);
        };
        match first {
            // Skip a comment.
            b'%' => {
                l = from_eol(l);
                continue;
            }
            // Skip a TJ expression: [()...()] TJ and [<>...<>] TJ.
            b'[' => {
                skip_tj(&mut l)?;
                continue;
            }
            // Skip text strings as in TJ, Tj, ' and ".
            b'(' => {
                skip_string_literal(&mut l)?;
                continue;
            }
            b'<' => {
                skip_hex_string_literal(&mut l)?;
                continue;
            }
            _ => {}
        }
        if skip_inline_image(&mut l, prn, relaxed, pdf20)? {
            continue;
        }
        *line = l;
        return Ok(false);
    }
}

/// A token of [`next_content_token`].
enum Token<'a> {
    /// A name, decoded, without the `/`.
    Name(Vec<u8>),
    /// Anything else: an operator or operand, or the raw name in front of a `<<` dictionary.
    Word(&'a [u8]),
}

/// `nextContentToken` on the text `l` (Go's `pre + *line`, which is not empty): the next token
/// and the offset in `l` of the text behind it. `None` is Go's empty token: nothing is left.
fn next_content_token<'a>(
    l: &'a [u8],
    prn: &mut Names,
    relaxed: bool,
    pdf20: bool,
) -> Result<Option<(Token<'a>, usize)>, Error> {
    // A token is either a name or some chunk terminated by white space or one of /, (, [
    let mut l = l;
    let total = l.len();
    // Skip Tj, TJ and inline images.
    if position_to_next_content_token(&mut l, prn, relaxed, pdf20)? {
        return Ok(None);
    }

    if l[0] == b'/' {
        // Cut off at / [ ( < or white space.
        let l1 = &l[1..];
        let i = match position_to_next_whitespace_or_char(l1, b"/[(<") {
            Some(i) if i > 0 => i,
            _ => return Err(err_page_content_corrupt()),
        };
        let t = &l1[..i];
        let mut l1 = trim_ws(&l1[i..]);
        if !l1.starts_with(b"<<") {
            // Resource dictionary keys use decoded names (7.3.5).
            let Ok(name) = decode_name(t) else {
                return Err(err_page_content_corrupt());
            };
            return Ok(Some((Token::Name(name.into_owned()), total - l1.len())));
        }
        skip_dict(&mut l1)?;
        return Ok(Some((Token::Word(t), total - l1.len())));
    }

    let i = match position_to_next_whitespace_or_char(l, b"/[(<") {
        Some(i) if i > 0 => i,
        _ => return Ok(Some((Token::Word(l), total))),
    };
    let t = &l[..i];
    let mut rest = &l[i..];
    if rest.starts_with(b"<<") {
        skip_dict(&mut rest)?;
    }
    Ok(Some((Token::Word(t), total - rest.len())))
}

fn color_space(s: &[u8], name: &[u8], prn: &mut Names) -> Result<bool, Error> {
    if s.starts_with(b"cs") || s.starts_with(b"CS") {
        if !matches!(
            name,
            b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" | b"Pattern"
        ) {
            prn.insert("ColorSpace", name)?;
        }
        return Ok(true);
    }
    Ok(false)
}

/// `resourceNameAtPos1`: the operator directly behind a name. The text behind the operator
/// within the same word, if the word is one.
fn resource_name_at_pos1<'a>(
    s: &'a [u8],
    name: &[u8],
    prn: &mut Names,
) -> Result<Option<&'a [u8]>, Error> {
    if color_space(s, name, prn)? {
        return Ok(Some(&s[2..]));
    }
    if s.starts_with(b"gs") {
        prn.insert("ExtGState", name)?;
        return Ok(Some(&s[2..]));
    }
    if s.starts_with(b"Do") {
        prn.insert("XObject", name)?;
        return Ok(Some(&s[2..]));
    }
    if s.starts_with(b"sh") {
        prn.insert("Shading", name)?;
        return Ok(Some(&s[2..]));
    }
    if s.starts_with(b"scn") || s.starts_with(b"SCN") {
        prn.insert("Pattern", name)?;
        return Ok(Some(&s[3..]));
    }
    if s.starts_with(b"ri") || s.starts_with(b"MP") {
        return Ok(Some(&s[2..]));
    }
    if s.starts_with(b"BMC") {
        return Ok(Some(&s[3..]));
    }
    Ok(None)
}

/// `resourceNameAtPos2`: the operator behind a name and one operand.
fn resource_name_at_pos2(s: &[u8], name: &[u8], prn: &mut Names) -> Result<bool, Error> {
    match s {
        b"Tf" => prn.insert("Font", name)?,
        b"BDC" | b"DP" => prn.insert("Properties", name)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// `parseContent`: the resource names that the content stream `s` uses. `relaxed` accepts an
/// inline image whose `EI` has no whitespace in front of it; `pdf20` requires the `/Length` of
/// an inline image unless `relaxed`.
///
/// # Errors
/// [`Sentinel::PageContentCorrupt`], [`Sentinel::TJExpressionCorrupt`],
/// [`Sentinel::BIExpressionCorrupt`], and the errors of the dictionary, string literal and hex
/// literal skipping ([`Error::Corrupt`]), [`Error::Alloc`] if `mem` refuses a name or a copy, or
/// [`Error::OutOfMemory`], or [`Error::Cancelled`] if `cancel` is set (checked once per token,
/// which includes once per `pre + rest` copy).
///
/// The names are charged to `mem`, and the returned [`Held`] is their charge; the `pre + rest`
/// copies of the text are charged while they live.
pub(crate) fn parse_content(
    s: &[u8],
    relaxed: bool,
    pdf20: bool,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<(PageResourceNames, Held), Error> {
    let mut prn = Names {
        prn: PageResourceNames::new(),
        held: mem.hold(),
    };
    // The text to parse: `s`, or a copy that has the rest of a word in front of it, and the
    // charge of the copy.
    let mut buf: Cow<'_, [u8]> = Cow::Borrowed(s);
    let mut _buf_held: Option<Held> = None;
    let mut pos = 0;
    let mut pre: Vec<u8> = Vec::new();
    let mut pre_held = mem.hold();
    let mut name: Vec<u8> = Vec::new();
    let mut n = false;
    let mut at = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if pos >= buf.len() {
            // `noBuf`: the text is looked at without `pre`.
            return Ok((prn.prn, prn.held));
        }
        if !pre.is_empty() {
            let len = pre.len().saturating_add(buf.len() - pos);
            let held = mem.charge(len as u64)?;
            let mut v = Vec::new();
            v.try_reserve_exact(len)?;
            v.extend_from_slice(&pre);
            v.extend_from_slice(&buf[pos..]);
            buf = Cow::Owned(v);
            _buf_held = Some(held);
            pos = 0;
            pre.clear();
        }
        let Some((t, next)) = next_content_token(&buf[pos..], &mut prn, relaxed, pdf20)? else {
            return Ok((prn.prn, prn.held));
        };
        pos += next;

        let word = match t {
            Token::Name(nm) => {
                name = nm;
                if n {
                    at += 1;
                } else {
                    n = true;
                    at = 0;
                }
                continue;
            }
            Token::Word(w) => w,
        };

        if !n {
            continue;
        }

        at += 1;
        if at == 1 {
            if let Some(rest) = resource_name_at_pos1(word, &name, &mut prn)? {
                // `pre` keeps its capacity, which its charge covers.
                let need = pre.len().saturating_add(rest.len()) as u64;
                pre_held.grow_by(need.saturating_sub(pre_held.bytes()))?;
                pre.try_reserve_exact(rest.len())?;
                pre.extend_from_slice(rest);
                n = false;
            }
            continue;
        }
        if at == 2 {
            if resource_name_at_pos2(word, &name, &mut prn)? {
                n = false;
            }
            continue;
        }
        n = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// [`super::parse_content`] with an unlimited ledger.
    fn parse_content(s: &[u8], relaxed: bool, pdf20: bool) -> Result<PageResourceNames, Error> {
        super::parse_content(
            s,
            relaxed,
            pdf20,
            &Mem::with_max(u64::MAX),
            &AtomicBool::new(false),
        )
        .map(|(prn, _)| prn)
    }

    #[test]
    fn parse_content_cancelled() {
        let s = b"/a gsq ".repeat(1000);
        let cancel = AtomicBool::new(true);
        let r = super::parse_content(&s, false, false, &Mem::with_max(u64::MAX), &cancel);
        assert!(matches!(r, Err(Error::Cancelled)));
    }

    fn kinds(prn: &PageResourceNames) -> BTreeMap<&'static str, BTreeSet<Vec<u8>>> {
        prn.0.clone()
    }

    fn expected(want: &[(&'static str, &[&str])]) -> BTreeMap<&'static str, BTreeSet<Vec<u8>>> {
        let mut e = PageResourceNames::new().0;
        for &(kind, names) in want {
            for nm in names {
                e.entry(kind).or_default().insert(nm.as_bytes().to_vec());
            }
        }
        e
    }

    fn check_mode(content: &[u8], want: &[(&'static str, &[&str])], relaxed: bool) {
        let got = parse_content(content, relaxed, false)
            .unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(content)));
        assert_eq!(
            kinds(&got),
            expected(want),
            "relaxed={relaxed} {}",
            String::from_utf8_lossy(content)
        );
    }

    fn check(content: &str, want: &[(&'static str, &[&str])]) {
        for relaxed in [false, true] {
            check_mode(content.as_bytes(), want, relaxed);
        }
    }

    fn corrupt_bi(content: &[u8], relaxed: bool, pdf20: bool) {
        let err = parse_content(content, relaxed, pdf20).unwrap_err();
        assert!(
            err.is(Sentinel::BIExpressionCorrupt),
            "relaxed={relaxed} {}: {err}",
            String::from_utf8_lossy(content)
        );
    }

    // Port of TestContentResourceNameEscapes.
    #[test]
    fn content_resource_name_escapes() {
        check(
            "/Im#201 Do /F#31 12 Tf /CS#30 cs /GS#31 gs /P#31 scn /SH#31 sh /Span /MC#31 BDC",
            &[
                ("XObject", &["Im 1"]),
                ("Font", &["F1"]),
                ("ColorSpace", &["CS0"]),
                ("ExtGState", &["GS1"]),
                ("Pattern", &["P1"]),
                ("Shading", &["SH1"]),
                ("Properties", &["MC1"]),
            ],
        );
        check("/Im#2f#23 Do /#44eviceRGB cs", &[("XObject", &["Im/#"])]);
        check("BI /CS /CS#300 ID x EI Q", &[("ColorSpace", &["CS00"])]);
    }

    // Port of TestInlineImageEndBoundaries.
    #[test]
    fn inline_image_end_boundaries() {
        for tail in [
            "",
            " Q",
            "\nBT /F1 12 Tf ET",
            "   /F1 12 Tf",
            "/F1 12 Tf",
            "% comment\n/F1 12 Tf",
        ] {
            let mut want: Vec<(&'static str, &[&str])> = Vec::new();
            if tail.len() > 2 {
                want.push(("Font", &["F1"]));
            }
            check(&format!("BI /CS /G ID x EI{tail}"), &want);
        }
        check(
            "BI /CS /G ID x EI /F1 12 Tf BI /CS /G ID y EI Q /Im1 Do",
            &[("Font", &["F1"]), ("XObject", &["Im1"])],
        );
        // A longer regular token is not an end marker.
        check(
            "BI /CS /G ID x EIx /Fake Do x EI /Real Do",
            &[("XObject", &["Real"])],
        );
    }

    // Port of TestInlineImageMissingEndWhitespace.
    #[test]
    fn inline_image_missing_end_whitespace() {
        let image = b"BI /W 1 /H 1 /CS /RGB /BPC 8 /I true ID \xdb\xdb#EI";
        for tail in ["", "\n", "\nQ /Im1 Do", "\nQ/Im1 Do"] {
            let content = [image.as_slice(), tail.as_bytes()].concat();
            let mut want: Vec<(&'static str, &[&str])> = Vec::new();
            if tail.len() > 1 {
                want.push(("XObject", &["Im1"]));
            }
            check_mode(&content, &want, true);
            corrupt_bi(&content, false, false);
        }
        let content = [
            image.as_slice(),
            b"\nQ /F1 12 Tf BI /CS /G ID y EI Q /Im1 Do",
        ]
        .concat();
        check_mode(&content, &[("Font", &["F1"]), ("XObject", &["Im1"])], true);
        check_mode(
            b"BI /CS /G ID xEI Query /Fake Do x EI /Real Do",
            &[("XObject", &["Real"])],
            true,
        );
    }

    // Port of TestInlineImageASCII85EndMarker.
    #[test]
    fn inline_image_ascii85_end_marker() {
        for filter in [
            "/A85",
            "/ASCII85Decode",
            "[/A85 /Fl]",
            "[/ASCII85Decode /FlateDecode]",
            "/#4185",
        ] {
            // Both EI] and EI) occur at line starts inside the qpdf-278-0.pdf image.
            let content = format!(
                "BI /W 4 /H 1 /BPC 8 /CS /RGB /F {filter} ID\r\nEI]ab\r\nEI)cd\r\nEI /X\r\n~>\r\nEI /Im1 Do"
            );
            check(&content, &[("XObject", &["Im1"])]);
        }
        check(
            "BI /Filter /A85 ID z~> EI /F1 12 Tf BI /F /A85 ID z~> EI Q /Im1 Do",
            &[("Font", &["F1"]), ("XObject", &["Im1"])],
        );
        // Only the first filter describes the encoding of bytes present in the content stream.
        check("BI /F [/Fl /A85] ID x EI /Im1 Do", &[("XObject", &["Im1"])]);
        let content = b"BI /F /A85 ID z~>EI Q /Im1 Do";
        check_mode(content, &[("XObject", &["Im1"])], true);
        corrupt_bi(content, false, false);
    }

    // Port of TestInlineImageASCII85MalformedEnd.
    #[test]
    fn inline_image_ascii85_malformed_end() {
        for content in [
            "BI /F /A85 ID z EI Q",
            "BI /F /A85 ID z~>",
            "BI /F /A85 ID z~> Q EI Q",
            "BI /F /A85 ID z~> EIx Q",
            "BI /F",
            "BI /F [",
            "BI /F [123] ID z~> EI Q",
        ] {
            for relaxed in [false, true] {
                corrupt_bi(content.as_bytes(), relaxed, false);
            }
        }
    }

    // Port of TestInlineImagePDF20Length.
    #[test]
    fn inline_image_pdf20_length() {
        for key in ["L", "Length"] {
            for relaxed in [false, true] {
                let content =
                    format!("BI /W 13 /H 1 /BPC 8 /CS /G /{key} 13 ID x EI /Fake Do EI /Real Do");
                let got = parse_content(content.as_bytes(), relaxed, true).unwrap();
                assert_eq!(kinds(&got), expected(&[("XObject", &["Real"])]), "{key}");
            }
        }
    }

    // Port of TestInlineImagePDF20LengthBoundaries.
    #[test]
    fn inline_image_pdf20_length_boundaries() {
        for content in [
            "BI /L 1 ID\r\nx EI /Real Do",
            "BI /L 2 ID  x EI /Real Do",
            "BI /L 3 /F /A85 ID \r\n z~> EI /Real Do",
            "BI /L 3 /Filter /ASCIIHexDecode ID \n 61> EI /Real Do",
        ] {
            let got = parse_content(content.as_bytes(), false, true).unwrap();
            assert_eq!(
                kinds(&got),
                expected(&[("XObject", &["Real"])]),
                "{content}"
            );
        }
    }

    // Port of TestInlineImagePDF20InvalidLength.
    #[test]
    fn inline_image_pdf20_invalid_length() {
        for content in [
            "BI /CS /G ID x EI /Real Do",
            "BI /L -1 ID x EI /Real Do",
            "BI /L x ID x EI /Real Do",
            "BI /L 1 /Length 2 ID x EI /Real Do",
            "BI /L 1 ID ab EI /Real Do",
            "BI /L 100 ID x EI /Real Do",
        ] {
            corrupt_bi(content.as_bytes(), false, true);
        }
        // The Length entry was introduced in PDF 2.0; earlier files may omit it.
        assert!(parse_content(b"BI /CS /G ID x EI /Real Do", false, false).is_ok());
    }

    // Port of TestInlineImageColorSpaceArrays.
    #[test]
    fn inline_image_color_space_arrays() {
        for cs in [
            "[/I /RGB 1 <000000FFFFFF>]",
            "[/Indexed /DeviceRGB 1 (ab(cd))]",
            "[/#49 /#52GB 1 (]/IDxx)]",
        ] {
            check(
                &format!("BI /CS {cs} ID x EI /Im1 Do"),
                &[("XObject", &["Im1"])],
            );
        }
        // Preserve a resource reference even in the tolerated nonstandard named-base form.
        check(
            "BI /CS [/Indexed /CS#30 1 (]/IDxx)] ID x EI Q /Im1 Do",
            &[("ColorSpace", &["CS0"]), ("XObject", &["Im1"])],
        );
    }

    // Port of TestContentNestedStringsAndDictionaries.
    #[test]
    fn content_nested_strings_and_dictionaries() {
        for content in [
            "(a(b)/Fake Do) Tj",
            "[(a(b)/Fake Do) 10 (c)] TJ",
            "(a\\)b(c)/Fake Do) Tj",
            "(a\\\\) Tj",
            "/Span <</ActualText (<<)>> BDC",
            "/Span <</ActualText (>> /Fake Do)>> BDC",
            "/Span <</ActualText <3E3E>>> BDC",
            "/Span <</Nested [<< /Text (a(b)<<>>) >>]>> BDC",
            "/Span << % >> ( <<\n/ActualText (text)>> BDC",
        ] {
            check(
                &format!("{content} /F1 12 Tf /Im1 Do"),
                &[("Font", &["F1"]), ("XObject", &["Im1"])],
            );
        }
    }

    // Port of TestContentMalformedExpressions.
    #[test]
    fn content_malformed_expressions() {
        for content in [
            "/Im# Do",
            "/F#GG 12 Tf",
            "/Im#00 Do",
            "BI /CS /CS#G0 ID x EI Q",
            "BI /CS [/I /CS#0 0 (x)] ID x EI Q",
            "(a(b)",
            "(abc\\",
            "/Span << /Text (a(b) >>",
            "/Span << /Text <123",
            "/Span << /Nested <<>>",
            "/Span << /Text (x) >",
            "BI /CS [",
            "BI /CS [/I /RGB 0 (abc",
            "BI /CS [/I /RGB 0 <abc",
            "BI /CS [/I /RGB 0 (abc)",
            "BI /CS [[] ID x EI Q",
            "BI /CS /G ID x EIx Q",
        ] {
            assert!(
                parse_content(content.as_bytes(), false, false).is_err(),
                "{content}"
            );
        }
    }

    // Port of TestParseContent.
    #[test]
    fn parse_content_collects_every_kind() {
        let s = "/CS0 cs/DeviceGray CS/Span<</ActualText <FEFF000900090009>>>, Span<</ActualText<FEFF0009>>>, Span<</ActualText<FEFF0020>>>,
	Span<</ActualText<FEFF0020002E>>>, Span<</ActualText<FEFF002E>>>, Span<</ActualText<FEFF00090009>>> BDC
	/a1 BMC/a2 MP /a3 /MC0 BDC/P0 scn/RelativeColorimetric ri/P1 SCN/GS0 gs[(Q[i,j]/2.)16.6(The/]maxi\\)-)]TJ/CS1 CS/a4<</A<FEFF>>> BDC /a5 <</A<FEFF>>>
	BDC (0.5*\\(1/8\\)*64 or +/4.\\))Tj/T1_0 1 Tf <00150015> Tj /Im5 Do/a5 << /A <FEFF> >> BDC/a6/MC1 DP /a7<<>>DP
	BI /IM true/W 1/CS/CS2/H 1/BPC 1 ID EI Q /Pattern cs/Span<</ActualText<FEFF0009>>> BDC/SH1 sh";
        let want = expected(&[
            ("ColorSpace", &["CS0", "CS1", "CS2"]),
            ("ExtGState", &["GS0"]),
            ("Font", &["T1_0"]),
            ("Pattern", &["P0", "P1"]),
            ("Properties", &["MC0", "MC1"]),
            ("Shading", &["SH1"]),
            ("XObject", &["Im5"]),
        ]);
        let got = parse_content(s.as_bytes(), false, false).unwrap();
        assert_eq!(kinds(&got), want);
    }

    // Port of TestParseCorruptContent.
    #[test]
    fn parse_corrupt_content() {
        for content in ["[(text)", "BI", "BI ID", "BI /CS "] {
            assert!(
                parse_content(content.as_bytes(), false, false).is_err(),
                "{content}"
            );
        }
        assert!(
            parse_content(b"[(text)", false, false)
                .unwrap_err()
                .is(Sentinel::TJExpressionCorrupt)
        );
        assert!(
            parse_content(b"/Im# Do", false, false)
                .unwrap_err()
                .is(Sentinel::PageContentCorrupt)
        );
    }

    // Port of TestContentPDFWhitespace.
    #[test]
    fn content_pdf_whitespace() {
        for ws in [0u8, 9, 10, 12, 13, 32] {
            let sep = char::from(ws);
            check(
                &format!("q{sep}/Im0{sep}Do{sep}Q"),
                &[("XObject", &["Im0"])],
            );
            check(
                &format!("BI{sep}/CS /G ID{sep}x{sep}EI{sep}Q /Im1 Do"),
                &[("XObject", &["Im1"])],
            );
            check(
                &format!("BI /F /A85 ID z~>{sep}EI Q /Im1 Do"),
                &[("XObject", &["Im1"])],
            );
        }
    }

    // Port of TestContentNonPDFWhitespace.
    #[test]
    fn content_non_pdf_whitespace() {
        let suffixes: [&[u8]; 9] = [
            b"\x0b",
            b"\x85",
            b"\xa0",
            "\u{85}".as_bytes(),
            "\u{a0}".as_bytes(),
            "\u{2003}".as_bytes(),
            "\u{2028}".as_bytes(),
            "\u{100}".as_bytes(),
            b"\xff",
        ];
        for suffix in suffixes {
            let cat = |parts: &[&[u8]]| parts.concat();
            check_mode(
                &cat(&[b"q /Im0 ", suffix, b"Do Q q /Im1 Do Q"]),
                &[("XObject", &["Im1"])],
                false,
            );
            check_mode(
                &cat(&[b"q BI", suffix, b"/Im1 Do Q"]),
                &[("XObject", &["Im1"])],
                true,
            );
            // Misleading EI sequences remain image bytes; resources inside them must not escape
            // into content.
            let content = cat(&[b"BI /CS /G ID EI", suffix, b"Q/Fake Do\nEI Q /Real Do"]);
            check_mode(&content, &[("XObject", &["Real"])], false);
            check_mode(&content, &[("XObject", &["Real"])], true);
            let content = cat(&[b"BI /CS /G ID EI", suffix, b"Q(\nEI Q"]);
            check_mode(&content, &[], false);
            check_mode(&content, &[], true);
            // The relaxed missing-separator exception still requires a complete Q token.
            let content = cat(&[b"BI /CS /G ID xEI Q", suffix, b"/Fake Do\nEI Q /Real Do"]);
            check_mode(&content, &[("XObject", &["Real"])], false);
            check_mode(&content, &[("XObject", &["Real"])], true);
            for relaxed in [false, true] {
                corrupt_bi(
                    &cat(&[b"BI /F /A85 ID z~>", suffix, b"EI Q"]),
                    relaxed,
                    false,
                );
            }
        }
    }

    // An operator glued to the next word keeps the rest of the word.
    #[test]
    fn operator_followed_by_text_in_the_same_word() {
        check("/Im1 Doq /Im2 Do", &[("XObject", &["Im1", "Im2"])]);
        check(
            "/CS0 cs1 /Im2 Do",
            &[("ColorSpace", &["CS0"]), ("XObject", &["Im2"])],
        );
    }
}
