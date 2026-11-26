//! The XMP packet of the catalog (Go: `model.XMPMeta` of model/metadata.go:17-119, filled by
//! `xml.Unmarshal` in validate/metaData.go:101).
//!
//! Rust has no XML parser in std, so this is a port of the part of Go's `encoding/xml` that
//! `xml.Unmarshal(data, &XMPMeta{})` runs: the strict decoder (`Decoder.Token`, `rawToken`,
//! `text`, name space translation) and the struct-tag matching of `Decoder.unmarshal`, written
//! out by hand for the six struct types of the model. Only whether the parse succeeded and what
//! it filled in is observable (relaxed validation skips a packet that fails to parse), so an
//! error is a bare marker and [`parse_xmp`] returns an `Option`. A refused charge or a set cancel
//! flag is not a parse failure: [`parse_xmp`] returns it as an `Err`, which the Go port has no
//! counterpart of (Go's allocations are not metered).
//!
//! Differences from `encoding/xml`: the Unicode range tables for names (`first` and `second`,
//! xml.go:1186-1700) are replaced by `char::is_alphabetic` and `char::is_alphanumeric` for
//! characters beyond ASCII, so a name with an exotic non-ASCII letter can differ; ASCII names
//! behave exactly as in Go. Text is kept as UTF-8 (`String`), which is what Go's `string` fields
//! hold after the decoder's own validity check.

use crate::env::GoTime;
use crate::error::Error;
use crate::ledger::{Held, Mem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_XAP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_PDF: &str = "http://ns.adobe.com/pdf/1.3/";
const NS_META: &str = "adobe:ns:meta/";
const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";

/// `maxUnmarshalDepth` (read.go:300).
const MAX_UNMARSHAL_DEPTH: usize = 10_000;

/// The instant of `time.Time{}`: 0001-01-01T00:00:00Z.
const ZERO_UNIX: i64 = -62_135_596_800;

/// `model.UserDate`: an XMP date. There is no sub-second part in [`GoTime`], so it is kept
/// beside it, for the comparison with the info dictionary's date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmpDate {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub unix: i64,
    pub nanos: u32,
    /// The offset east of UTC, in minutes.
    pub utc_offset_min: i32,
}

impl XmpDate {
    /// `time.Time.IsZero`.
    pub fn is_zero(&self) -> bool {
        self.unix == ZERO_UNIX && self.nanos == 0
    }

    /// The time without its sub-second part.
    pub fn go_time(&self) -> GoTime {
        GoTime {
            unix: self.unix,
            utc_offset_min: self.utc_offset_min,
        }
    }
}

/// `model.Description`: what the catalog's XMP packet says about the document. An absent date
/// is `None` (Go's zero `time.Time`).
#[derive(Debug, Clone, Default)]
pub struct Description {
    /// `Title.Alt.Entries`.
    pub title: Vec<String>,
    /// `Author.Seq.Entries`.
    pub author: Vec<String>,
    /// `Subject.Alt.Entries`.
    pub subject: Vec<String>,
    pub creator: String,
    pub creation_date: Option<XmpDate>,
    pub mod_date: Option<XmpDate>,
    pub producer: String,
    pub trapped: bool,
    pub keywords: String,
}

/// `model.RDF`.
#[derive(Debug, Clone, Default)]
pub struct Rdf {
    pub description: Description,
}

/// `model.XMPMeta`.
#[derive(Debug, Clone, Default)]
pub struct XmpMeta {
    pub rdf: Rdf,
}

impl XmpMeta {
    /// The heap of a clone: the text of each string, and a `String` per list entry.
    pub fn copy_bytes(&self) -> u64 {
        let d = &self.rdf.description;
        let list = |l: &[String]| {
            l.iter()
                .map(|s| (size_of::<String>() + s.len()) as u64)
                .sum::<u64>()
        };
        list(&d.title)
            + list(&d.author)
            + list(&d.subject)
            + (d.creator.len() + d.producer.len() + d.keywords.len()) as u64
    }
}

/// `xml.Unmarshal(data, &XMPMeta{})`: `None` if it fails (a syntax error, the wrong root
/// element, a date or a boolean that does not parse, nesting beyond 10,000 levels).
///
/// Everything the parse builds is charged to `mem` and released when it returns (the caller
/// charges the result it keeps).
///
/// # Errors
/// [`Error::Alloc`] or [`Error::OutOfMemory`] when a buffer cannot be had, [`Error::Cancelled`]
/// once `cancel` is set.
pub fn parse_xmp(data: &[u8], mem: &Mem, cancel: &AtomicBool) -> Result<Option<XmpMeta>, Error> {
    let mut d = Decoder::new(data, mem, cancel);
    let r = d.parse();
    match d.fatal.take() {
        Some(e) => Err(e),
        None => Ok(r.ok()),
    }
}

/// Any decoder or unmarshal error.
struct XmlErr;

type XResult<T> = Result<T, XmlErr>;

#[derive(Clone, Debug)]
struct Name {
    space: String,
    local: String,
}

enum Raw {
    Start(Name, Vec<(Name, String)>),
    End(Name),
    Text(Vec<u8>),
    Other,
}

enum Tok {
    Start(Name),
    End,
    Text(Vec<u8>),
    Other,
}

/// The stack of `Decoder`: open elements, and the name space translations to undo when they
/// close (they sit below their element).
enum Frame {
    Start(Name),
    Ns { local: String, prev: Option<String> },
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    /// The input position at or past which [`Decoder::poll_cancel`] checks the flag again.
    next_poll: usize,
    ns: BTreeMap<String, String>,
    stk: Vec<Frame>,
    need_close: Option<Name>,
    /// `stkDepth`: the number of open elements.
    depth: usize,
    /// What the parse keeps until it returns: stack, name spaces, strings.
    held: Held,
    /// What the current token holds, released when the next one starts.
    tok: Held,
    cancel: &'a AtomicBool,
    /// The error that is not a parse failure (see [`parse_xmp`]).
    fatal: Option<Error>,
}

/// Makes room for one more element of `v`, charging the growth of its capacity first.
fn grow_vec<T>(held: &mut Held, v: &mut Vec<T>) -> Result<(), Error> {
    if v.len() == v.capacity() {
        let add = v.capacity().max(4);
        held.grow_by((add * size_of::<T>()) as u64)?;
        v.try_reserve_exact(add)?;
    }
    Ok(())
}

/// What one name space translation adds to the map (as `entry_copy_bytes`).
fn ns_entry_bytes(key: usize, val: usize) -> u64 {
    (2 * size_of::<String>() + 16 + key + val) as u64
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'.' | b'-')
}

fn is_first(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphabetic() || c == '_' || c == ':'
    } else {
        c.is_alphabetic()
    }
}

fn is_second(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_digit() || c == '.' || c == '-'
    } else {
        c.is_alphanumeric() || c == '\u{B7}'
    }
}

/// `isName`.
fn is_name(b: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(b) else {
        return false;
    };
    let mut it = s.chars();
    let Some(c) = it.next() else {
        return false;
    };
    is_first(c) && it.all(|c| is_first(c) || is_second(c))
}

/// `isInCharacterRange`.
fn is_in_character_range(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// `procInst`: the value of `param` in the pseudo-attributes of a processing instruction.
fn proc_inst(param: &str, s: &[u8]) -> Vec<u8> {
    let param = format!("{param}=").into_bytes();
    let lenp = param.len();
    let mut i = 0usize;
    let mut sep = 0u8;
    while i < s.len() {
        let sub = &s[i..];
        let Some(k) = sub.windows(lenp).position(|w| w == param.as_slice()) else {
            return Vec::new();
        };
        if lenp + k >= sub.len() {
            return Vec::new();
        }
        i += lenp + k + 1;
        let c = sub[lenp + k];
        if c == b'\'' || c == b'"' {
            sep = c;
            break;
        }
    }
    if sep == 0 {
        return Vec::new();
    }
    match s[i..].iter().position(|&c| c == sep) {
        Some(j) => s[i..i + j].to_vec(),
        None => Vec::new(),
    }
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8], mem: &Mem, cancel: &'a AtomicBool) -> Decoder<'a> {
        Decoder {
            data,
            pos: 0,
            next_poll: 0,
            ns: BTreeMap::new(),
            stk: Vec::new(),
            need_close: None,
            depth: 0,
            held: mem.hold(),
            tok: mem.hold(),
            cancel,
            fatal: None,
        }
    }

    fn parse(&mut self) -> XResult<XmpMeta> {
        // `unmarshal` with no start element: the first start element of the stream.
        let start = loop {
            if let Tok::Start(n) = self.token()? {
                break n;
            }
        };
        self.unmarshal_meta(&start)
    }

    /// Records `e` as the reason the parse stopped, which is not a syntax error.
    fn fail(&mut self, e: Error) -> XmlErr {
        self.fatal = Some(e);
        XmlErr
    }

    /// Charges `n` bytes that live until the parse returns.
    fn keep(&mut self, n: u64) -> XResult<()> {
        self.held.grow_by(n).map_err(|e| self.fail(e))
    }

    /// Releases `n` bytes of what [`Decoder::keep`] charged.
    fn release(&mut self, n: u64) {
        self.held.shrink_to(self.held.bytes().saturating_sub(n));
    }

    /// Charges `n` bytes that live until the next token starts.
    fn temp(&mut self, n: usize) -> XResult<()> {
        self.tok.grow_by(n as u64).map_err(|e| self.fail(e))
    }

    /// A buffer of `n` bytes, charged to the token before it is allocated.
    fn temp_buf(&mut self, n: usize) -> XResult<Vec<u8>> {
        self.temp(n)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|e| self.fail(e.into()))?;
        Ok(v)
    }

    /// `Error::Cancelled` once the cancel flag is set.
    fn check_cancel(&mut self) -> XResult<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(self.fail(Error::Cancelled));
        }
        Ok(())
    }

    /// `check_cancel` every 4096 bytes of input, for the loops over bytes.
    fn poll_cancel(&mut self) -> XResult<()> {
        if self.pos >= self.next_poll {
            self.next_poll = self.pos.saturating_add(4096);
            self.check_cancel()?;
        }
        Ok(())
    }

    /// Where the bytes from here end at the first of `stops` (the end of the data if none does):
    /// the most that a scan up to it can build.
    fn run_len(&self, stops: &[u8]) -> usize {
        let rest = &self.data[self.pos.min(self.data.len())..];
        rest.iter()
            .position(|b| stops.contains(b))
            .unwrap_or(rest.len())
    }

    /// Appends `s` to `v`, charging the growth of `v`.
    fn push_entry(&mut self, v: &mut Vec<String>, s: String) -> XResult<()> {
        grow_vec(&mut self.held, v).map_err(|e| self.fail(e))?;
        v.push(s);
        Ok(())
    }

    fn getc(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn mustgetc(&mut self) -> XResult<u8> {
        self.getc().ok_or(XmlErr)
    }

    fn ungetc(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    /// `space`: skips blanks.
    fn space(&mut self) {
        while let Some(b) = self.data.get(self.pos) {
            if matches!(b, b' ' | b'\r' | b'\n' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// `readName`: false if the next byte starts no name.
    fn read_name(&mut self, buf: &mut Vec<u8>) -> XResult<bool> {
        let b = self.mustgetc()?;
        if b < 0x80 && !is_name_byte(b) {
            self.ungetc();
            return Ok(false);
        }
        // The rest of the name runs to the first ASCII byte that is not a name byte.
        let n = self.data[self.pos..]
            .iter()
            .position(|&c| c < 0x80 && !is_name_byte(c))
            .unwrap_or(self.data.len() - self.pos)
            + 1;
        self.temp(n)?;
        buf.try_reserve_exact(n).map_err(|e| self.fail(e.into()))?;
        buf.push(b);
        loop {
            let b = self.mustgetc()?;
            if b < 0x80 && !is_name_byte(b) {
                self.ungetc();
                break;
            }
            buf.push(b);
        }
        Ok(true)
    }

    /// `name`.
    fn name(&mut self) -> XResult<String> {
        let mut buf = Vec::new();
        if !self.read_name(&mut buf)? || !is_name(&buf) {
            return Err(XmlErr);
        }
        String::from_utf8(buf).map_err(|_| XmlErr)
    }

    /// `nsname`: a name with an optional prefix.
    fn nsname(&mut self) -> XResult<Name> {
        let s = self.name()?;
        if s.matches(':').count() > 1 {
            return Err(XmlErr);
        }
        match s.split_once(':') {
            Some((space, local)) if !space.is_empty() && !local.is_empty() => {
                self.temp(s.len())?;
                Ok(Name {
                    space: space.to_string(),
                    local: local.to_string(),
                })
            }
            _ => Ok(Name {
                space: String::new(),
                local: s,
            }),
        }
    }

    /// The character after a `&` in text, up to and including the `;`. Strict mode: only the
    /// five predefined entities and character references.
    fn entity(&mut self) -> XResult<char> {
        let b = self.mustgetc()?;
        if b == b'#' {
            let mut b = self.mustgetc()?;
            let mut base = 10;
            if b == b'x' {
                base = 16;
                b = self.mustgetc()?;
            }
            // The value saturates above U+10FFFF: Go fails the same way for a number that
            // overflows and for one that is too big.
            let mut n = 0u32;
            let mut any = false;
            while b.is_ascii_digit() || (base == 16 && b.is_ascii_hexdigit()) {
                n = n
                    .saturating_mul(base)
                    .saturating_add(char::from(b).to_digit(base).unwrap_or(0))
                    .min(0x11_0000);
                any = true;
                b = self.mustgetc()?;
            }
            if b != b';' || !any {
                return Err(XmlErr);
            }
            if n > 0x10FFFF {
                return Err(XmlErr);
            }
            // A surrogate becomes U+FFFD, as `string(rune(n))` does.
            return Ok(char::from_u32(n).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        self.ungetc();
        let mut name = Vec::new();
        self.read_name(&mut name)?;
        if self.mustgetc()? != b';' {
            return Err(XmlErr);
        }
        match name.as_slice() {
            b"lt" => Ok('<'),
            b"gt" => Ok('>'),
            b"amp" => Ok('&'),
            b"apos" => Ok('\''),
            b"quot" => Ok('"'),
            _ => Err(XmlErr),
        }
    }

    /// `text`: character data up to a `<` (or the closing `quote`; or the `]]>` of a CDATA
    /// section). `\r` and `\r\n` become `\n`.
    fn text(&mut self, quote: Option<u8>, cdata: bool) -> XResult<Vec<u8>> {
        // The text is no longer than the run up to its end (an entity or `\r\n` only shrinks).
        let n = if cdata {
            let rest = &self.data[self.pos.min(self.data.len())..];
            rest.windows(3)
                .position(|w| w == b"]]>")
                .map_or(rest.len(), |i| i + 3)
        } else {
            self.run_len(&[b'<', quote.unwrap_or(b'<')])
        };
        let mut buf = self.temp_buf(n + 1)?;
        let (mut b0, mut b1) = (0u8, 0u8);
        let mut trunc = 0usize;
        loop {
            self.poll_cancel()?;
            let Some(b) = self.getc() else {
                if cdata {
                    return Err(XmlErr);
                }
                break;
            };
            // `]]>` ends a CDATA section, and is an error in ordinary text outside quotes.
            if quote.is_none() && b0 == b']' && b1 == b']' && b == b'>' {
                if cdata {
                    trunc = 2;
                    break;
                }
                return Err(XmlErr);
            }
            if b == b'<' && !cdata {
                if quote.is_some() {
                    return Err(XmlErr);
                }
                self.ungetc();
                break;
            }
            if quote == Some(b) {
                break;
            }
            if b == b'&' && !cdata {
                let c = self.entity()?;
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                b0 = 0;
                b1 = 0;
                continue;
            }
            if b == b'\r' {
                buf.push(b'\n');
            } else if !(b1 == b'\r' && b == b'\n') {
                buf.push(b);
            }
            b0 = b1;
            b1 = b;
        }
        buf.truncate(buf.len().saturating_sub(trunc));
        let s = std::str::from_utf8(&buf).map_err(|_| XmlErr)?;
        if !s.chars().all(is_in_character_range) {
            return Err(XmlErr);
        }
        Ok(buf)
    }

    /// `rawToken`.
    fn raw_token(&mut self) -> XResult<Raw> {
        if let Some(n) = self.need_close.take() {
            return Ok(Raw::End(n));
        }
        let b = self.getc().ok_or(XmlErr)?;
        if b != b'<' {
            self.ungetc();
            return Ok(Raw::Text(self.text(None, false)?));
        }
        let b = self.mustgetc()?;
        match b {
            b'/' => {
                let name = self.nsname()?;
                self.space();
                if self.mustgetc()? != b'>' {
                    return Err(XmlErr);
                }
                Ok(Raw::End(name))
            }
            b'?' => {
                let target = self.name()?;
                self.space();
                // The instruction runs to its `?>`; `proc_inst` copies at most all of it.
                let rest = &self.data[self.pos.min(self.data.len())..];
                let n = rest
                    .windows(2)
                    .position(|w| w == b"?>")
                    .map_or(rest.len(), |i| i + 2);
                let mut buf = self.temp_buf(n)?;
                self.temp(n)?;
                let mut b0 = 0u8;
                loop {
                    self.poll_cancel()?;
                    let b = self.mustgetc()?;
                    buf.push(b);
                    if b0 == b'?' && b == b'>' {
                        break;
                    }
                    b0 = b;
                }
                buf.truncate(buf.len() - 2);
                if target == "xml" {
                    let ver = proc_inst("version", &buf);
                    if !ver.is_empty() && ver != b"1.0" {
                        return Err(XmlErr);
                    }
                    // No CharsetReader: only UTF-8 may be declared.
                    let enc = proc_inst("encoding", &buf);
                    if !enc.is_empty() && !enc.eq_ignore_ascii_case(b"utf-8") {
                        return Err(XmlErr);
                    }
                }
                Ok(Raw::Other)
            }
            b'!' => self.markup_declaration(),
            _ => {
                self.ungetc();
                self.start_element()
            }
        }
    }

    /// `<!`: a comment, a CDATA section or a directive.
    fn markup_declaration(&mut self) -> XResult<Raw> {
        let b = self.mustgetc()?;
        match b {
            b'-' => {
                if self.mustgetc()? != b'-' {
                    return Err(XmlErr);
                }
                let (mut b0, mut b1) = (0u8, 0u8);
                loop {
                    self.poll_cancel()?;
                    let b = self.mustgetc()?;
                    if b0 == b'-' && b1 == b'-' {
                        if b != b'>' {
                            return Err(XmlErr);
                        }
                        break;
                    }
                    b0 = b1;
                    b1 = b;
                }
                Ok(Raw::Other)
            }
            b'[' => {
                for c in b"CDATA[" {
                    if self.mustgetc()? != *c {
                        return Err(XmlErr);
                    }
                }
                Ok(Raw::Text(self.text(None, true)?))
            }
            _ => {
                // A directive (<!DOCTYPE ...>): quoted angle brackets do not count, comments
                // inside it are skipped.
                let mut inquote = 0u8;
                let mut depth = 0i64;
                let mut b = self.mustgetc()?;
                loop {
                    self.poll_cancel()?;
                    if inquote == 0 && b == b'>' && depth == 0 {
                        break;
                    }
                    // HandleB
                    loop {
                        if b == inquote {
                            inquote = 0;
                        } else if inquote != 0 {
                            // In quotes: nothing special.
                        } else if b == b'\'' || b == b'"' {
                            inquote = b;
                        } else if b == b'>' {
                            depth -= 1;
                        } else if b == b'<' {
                            let mut mismatch = false;
                            for c in b"!--" {
                                let nb = self.mustgetc()?;
                                if nb != *c {
                                    depth += 1;
                                    b = nb;
                                    mismatch = true;
                                    break;
                                }
                            }
                            if mismatch {
                                self.poll_cancel()?;
                                continue;
                            }
                            // A comment: look for its terminator.
                            let (mut b0, mut b1) = (0u8, 0u8);
                            loop {
                                self.poll_cancel()?;
                                let nb = self.mustgetc()?;
                                if b0 == b'-' && b1 == b'-' && nb == b'>' {
                                    break;
                                }
                                b0 = b1;
                                b1 = nb;
                            }
                        }
                        break;
                    }
                    b = self.mustgetc()?;
                }
                Ok(Raw::Other)
            }
        }
    }

    /// An open element such as `<a href="foo">`.
    fn start_element(&mut self) -> XResult<Raw> {
        let name = self.nsname()?;
        let mut attrs: Vec<(Name, String)> = Vec::new();
        let mut empty = false;
        loop {
            self.space();
            let b = self.mustgetc()?;
            if b == b'/' {
                empty = true;
                if self.mustgetc()? != b'>' {
                    return Err(XmlErr);
                }
                break;
            }
            if b == b'>' {
                break;
            }
            self.ungetc();
            let aname = self.nsname()?;
            self.space();
            // Strict: an attribute has a quoted value.
            if self.mustgetc()? != b'=' {
                return Err(XmlErr);
            }
            self.space();
            let q = self.mustgetc()?;
            if q != b'"' && q != b'\'' {
                return Err(XmlErr);
            }
            let v = self.text(Some(q), false)?;
            self.temp(v.len() + aname.space.len() + aname.local.len())?;
            grow_vec(&mut self.tok, &mut attrs).map_err(|e| self.fail(e))?;
            attrs.push((aname, String::from_utf8_lossy(&v).into_owned()));
        }
        if empty {
            self.temp(name.space.len() + name.local.len())?;
            self.need_close = Some(name.clone());
        }
        Ok(Raw::Start(name, attrs))
    }

    /// `translate` for an element name.
    fn translate(&mut self, n: &mut Name) -> XResult<()> {
        if n.space == "xmlns" {
            return Ok(());
        }
        if n.space == "xml" {
            n.space = NS_XML.to_string();
        } else if n.space.is_empty() && n.local == "xmlns" {
            return Ok(());
        }
        if let Some(len) = self.ns.get(&n.space).map(String::len) {
            self.temp(len)?;
            if let Some(v) = self.ns.get(&n.space) {
                n.space = v.clone();
            }
        }
        Ok(())
    }

    /// `popElement`: the end tag has to match the element on top of the stack.
    fn pop_element(&mut self, end: &Name) -> XResult<()> {
        let Some(Frame::Start(s)) = self.stk.pop() else {
            return Err(XmlErr);
        };
        self.release((s.space.len() + s.local.len()) as u64);
        if s.local != end.local || s.space != end.space {
            return Err(XmlErr);
        }
        self.depth = self.depth.saturating_sub(1);
        while matches!(self.stk.last(), Some(Frame::Ns { .. })) {
            if let Some(Frame::Ns { local, prev }) = self.stk.pop() {
                let cur = self.ns.get(&local).map_or(0, String::len);
                self.release(local.len() as u64 + ns_entry_bytes(local.len(), cur));
                match prev {
                    Some(v) => {
                        self.ns.insert(local, v);
                    }
                    None => {
                        self.ns.remove(&local);
                    }
                }
            }
        }
        Ok(())
    }

    /// `Token`: the next token, with the name space prefixes translated and the elements
    /// checked to nest.
    fn token(&mut self) -> XResult<Tok> {
        self.check_cancel()?;
        self.tok.shrink_to(0);
        Ok(match self.raw_token()? {
            Raw::Start(mut name, attrs) => {
                for (an, av) in &attrs {
                    let local = if an.space == "xmlns" {
                        an.local.as_str()
                    } else if an.space.is_empty() && an.local == "xmlns" {
                        ""
                    } else {
                        continue;
                    };
                    self.keep(local.len() as u64 + ns_entry_bytes(local.len(), av.len()))?;
                    let prev = self.ns.insert(local.to_string(), av.clone());
                    grow_vec(&mut self.held, &mut self.stk).map_err(|e| self.fail(e))?;
                    self.stk.push(Frame::Ns {
                        local: local.to_string(),
                        prev,
                    });
                }
                self.keep((name.space.len() + name.local.len()) as u64)?;
                grow_vec(&mut self.held, &mut self.stk).map_err(|e| self.fail(e))?;
                self.stk.push(Frame::Start(name.clone()));
                self.depth += 1;
                self.translate(&mut name)?;
                Tok::Start(name)
            }
            Raw::End(name) => {
                self.pop_element(&name)?;
                Tok::End
            }
            Raw::Text(t) => Tok::Text(t),
            Raw::Other => Tok::Other,
        })
    }

    /// `Skip`: to the end of the element that was just started.
    fn skip(&mut self) -> XResult<()> {
        let mut depth = 0i64;
        loop {
            match self.token()? {
                Tok::Start(_) => depth += 1,
                Tok::End => {
                    if depth == 0 {
                        return Ok(());
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    /// The loop of `unmarshal` over the content of an element: `on_child` handles a child
    /// element and says whether it consumed it, an unconsumed one is skipped. The character
    /// data of the element itself is returned if `want_text`.
    fn read_children<F>(&mut self, want_text: bool, mut on_child: F) -> XResult<Vec<u8>>
    where
        F: FnMut(&mut Decoder<'a>, &Name) -> XResult<bool>,
    {
        let mut data = Vec::new();
        loop {
            match self.token()? {
                Tok::Start(n) => {
                    // The name outlives its token's charge: it is kept while the element is read.
                    let nb = (n.space.len() + n.local.len()) as u64;
                    self.keep(nb)?;
                    if !on_child(self, &n)? {
                        self.skip()?;
                    }
                    self.release(nb);
                }
                Tok::End => break,
                Tok::Text(t) => {
                    if want_text {
                        // The capacity growth (doubling, so many small runs stay linear) is
                        // charged; `read_string` keeps it for the `String`.
                        let cap = data.capacity();
                        if cap - data.len() < t.len() {
                            let want = (data.len() + t.len()).max(2 * cap);
                            self.keep((want - cap) as u64)?;
                            data.try_reserve_exact(want - data.len())
                                .map_err(|e| self.fail(e.into()))?;
                        }
                        data.extend_from_slice(&t);
                    }
                }
                Tok::Other => {}
            }
        }
        Ok(data)
    }

    /// The depth check at the top of `unmarshal`.
    fn enter(&self) -> XResult<()> {
        if self.depth > MAX_UNMARSHAL_DEPTH {
            return Err(XmlErr);
        }
        Ok(())
    }

    /// An element unmarshaled into a `string`.
    fn read_string(&mut self) -> XResult<String> {
        self.enter()?;
        let data = self.read_children(true, |_, _| Ok(false))?;
        // The text was checked to be UTF-8, so the `String` is as long as `data`.
        self.keep(data.len() as u64)?;
        let s = String::from_utf8_lossy(&data).into_owned();
        self.release(data.len() as u64);
        Ok(s)
    }

    /// An element unmarshaled into a `bool` (`copyValue`).
    fn read_bool(&mut self) -> XResult<bool> {
        let s = self.read_string()?;
        if s.is_empty() {
            return Ok(false);
        }
        parse_bool(s.trim())
    }

    /// `UserDate.UnmarshalXML` (metadata.go:44).
    fn read_date(&mut self) -> XResult<XmpDate> {
        let s = self.read_string()?;
        parse_xmp_date(s.trim())
    }

    /// `Title`, `Creator` or `Desc`: a struct with one field, the container (`Alt` or `Seq`),
    /// which has the `li` entries.
    fn unmarshal_list(&mut self, entries: &mut Vec<String>, container: &str) -> XResult<()> {
        self.enter()?;
        self.read_children(false, |d, n| {
            if n.space != NS_RDF || n.local != container {
                return Ok(false);
            }
            d.enter()?;
            d.read_children(false, |d2, n2| {
                if n2.space == NS_RDF && n2.local == "li" {
                    let s = d2.read_string()?;
                    d2.push_entry(entries, s)?;
                    return Ok(true);
                }
                Ok(false)
            })?;
            Ok(true)
        })?;
        Ok(())
    }

    /// `Description`.
    fn unmarshal_description(&mut self, desc: &mut Description) -> XResult<()> {
        self.enter()?;
        self.read_children(false, |d, n| {
            match (n.space.as_str(), n.local.as_str()) {
                (NS_DC, "title") => d.unmarshal_list(&mut desc.title, "Alt")?,
                (NS_DC, "creator") => d.unmarshal_list(&mut desc.author, "Seq")?,
                (NS_DC, "description") => d.unmarshal_list(&mut desc.subject, "Alt")?,
                (NS_XAP, "CreatorTool") => desc.creator = d.read_string()?,
                (NS_XAP, "CreateDate") => desc.creation_date = Some(d.read_date()?),
                (NS_XAP, "ModifyDate") => desc.mod_date = Some(d.read_date()?),
                (NS_PDF, "Producer") => desc.producer = d.read_string()?,
                (NS_PDF, "Trapped") => desc.trapped = d.read_bool()?,
                (NS_PDF, "Keywords") => desc.keywords = d.read_string()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(())
    }

    /// `RDF`: its `XMLName` demands the RDF name space.
    fn unmarshal_rdf(&mut self, start: &Name, rdf: &mut Rdf) -> XResult<()> {
        self.enter()?;
        if start.local != "RDF" || start.space != NS_RDF {
            return Err(XmlErr);
        }
        self.read_children(false, |d, n| {
            if n.local == "Description" {
                d.unmarshal_description(&mut rdf.description)?;
                return Ok(true);
            }
            Ok(false)
        })?;
        Ok(())
    }

    /// `XMPMeta`: its `XMLName` demands `xmpmeta` in the `adobe:ns:meta/` name space.
    fn unmarshal_meta(&mut self, start: &Name) -> XResult<XmpMeta> {
        self.enter()?;
        if start.local != "xmpmeta" || start.space != NS_META {
            return Err(XmlErr);
        }
        let mut m = XmpMeta::default();
        self.read_children(false, |d, n| {
            if n.local == "RDF" {
                d.unmarshal_rdf(n, &mut m.rdf)?;
                return Ok(true);
            }
            Ok(false)
        })?;
        Ok(m)
    }
}

/// `strconv.ParseBool`.
fn parse_bool(s: &str) -> XResult<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(XmlErr),
    }
}

/// The calendar parts a layout of `userDateFormats` has.
#[derive(Clone, Copy)]
struct Layout {
    month: bool,
    day: bool,
    time: bool,
    seconds: bool,
    zone: bool,
}

/// `userDateFormats` (metadata.go:29-37), in order.
const LAYOUTS: [Layout; 7] = [
    // time.RFC3339
    Layout {
        month: true,
        day: true,
        time: true,
        seconds: true,
        zone: true,
    },
    // 2006-01-02T15:04Z07:00
    Layout {
        month: true,
        day: true,
        time: true,
        seconds: false,
        zone: true,
    },
    // 2006-01-02T15:04:05
    Layout {
        month: true,
        day: true,
        time: true,
        seconds: true,
        zone: false,
    },
    // 2006-01-02T15:04
    Layout {
        month: true,
        day: true,
        time: true,
        seconds: false,
        zone: false,
    },
    // 2006-01-02
    Layout {
        month: true,
        day: true,
        time: false,
        seconds: false,
        zone: false,
    },
    // 2006-01
    Layout {
        month: true,
        day: false,
        time: false,
        seconds: false,
        zone: false,
    },
    // 2006
    Layout {
        month: false,
        day: false,
        time: false,
        seconds: false,
        zone: false,
    },
];

/// `UserDate.UnmarshalXML`: the first of the layouts that parses.
fn parse_xmp_date(s: &str) -> XResult<XmpDate> {
    LAYOUTS
        .iter()
        .find_map(|l| parse_layout(*l, s.as_bytes()))
        .ok_or(XmlErr)
}

/// `getnum`: one or two digits (exactly two if `fixed`).
fn getnum(v: &[u8], i: &mut usize, fixed: bool) -> Option<i64> {
    let a = *v.get(*i)?;
    if !a.is_ascii_digit() {
        return None;
    }
    match v.get(*i + 1) {
        Some(b) if b.is_ascii_digit() => {
            *i += 2;
            Some(i64::from(a - b'0') * 10 + i64::from(b - b'0'))
        }
        _ if fixed => None,
        _ => {
            *i += 1;
            Some(i64::from(a - b'0'))
        }
    }
}

/// Consumes the literal byte `c`.
fn skip(v: &[u8], i: &mut usize, c: u8) -> Option<()> {
    if v.get(*i) == Some(&c) {
        *i += 1;
        return Some(());
    }
    None
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `time.Parse` of one layout (format.go:1058ff), UTC if the layout has no zone.
fn parse_layout(l: Layout, v: &[u8]) -> Option<XmpDate> {
    let mut i = 0usize;
    // 2006
    if v.len() < 4 || !v[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = v[..4]
        .iter()
        .fold(0i64, |a, c| a * 10 + i64::from(c - b'0'));
    i += 4;
    let (mut month, mut day) = (1i64, 1i64);
    if l.month {
        skip(v, &mut i, b'-')?;
        month = getnum(v, &mut i, true)?;
        if !(1..=12).contains(&month) {
            return None;
        }
    }
    if l.day {
        skip(v, &mut i, b'-')?;
        day = getnum(v, &mut i, true)?;
    }
    let (mut hour, mut min, mut sec, mut nanos) = (0i64, 0i64, 0i64, 0u32);
    if l.time {
        skip(v, &mut i, b'T')?;
        hour = getnum(v, &mut i, false)?;
        if hour >= 24 {
            return None;
        }
        skip(v, &mut i, b':')?;
        min = getnum(v, &mut i, true)?;
        if min >= 60 {
            return None;
        }
    }
    if l.seconds {
        skip(v, &mut i, b':')?;
        sec = getnum(v, &mut i, true)?;
        if sec >= 60 {
            return None;
        }
        // A fractional second that the layout does not have.
        if v.len() >= i + 2 && matches!(v[i], b'.' | b',') && v[i + 1].is_ascii_digit() {
            let mut n = i + 1;
            while n < v.len() && v[n].is_ascii_digit() {
                n += 1;
            }
            // At most nine digits count.
            let digits = &v[i + 1..n.min(i + 10)];
            let mut ns = digits
                .iter()
                .fold(0u32, |a, c| a * 10 + u32::from(c - b'0'));
            for _ in digits.len()..9 {
                ns *= 10;
            }
            nanos = ns;
            i = n;
        }
    }
    let mut offset_min = 0i64;
    if l.zone {
        if v.get(i) == Some(&b'Z') {
            i += 1;
        } else {
            // Z07:00
            if v.len() < i + 6 || v[i + 3] != b':' {
                return None;
            }
            let sign = v[i];
            let mut j = i + 1;
            let hr = getnum(v, &mut j, true)?;
            j += 1;
            let mm = getnum(v, &mut j, true)?;
            // Go tests with > rather than >=: 24 hours and 60 minutes are accepted.
            if hr > 24 || mm > 60 {
                return None;
            }
            offset_min = match sign {
                b'+' => hr * 60 + mm,
                b'-' => -(hr * 60 + mm),
                _ => return None,
            };
            i += 6;
        }
    }
    if i != v.len() {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let unix =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec - offset_min * 60;
    Some(XmpDate {
        unix,
        nanos,
        utc_offset_min: i32::try_from(offset_min).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::parse_xmp`] with a ledger that has room and no cancel.
    fn parse_xmp(data: &[u8]) -> Option<XmpMeta> {
        super::parse_xmp(data, &Mem::with_max(u64::MAX), &AtomicBool::new(false)).expect("no error")
    }

    #[test]
    fn parse_is_charged_and_cancellable() {
        let mut p = String::from(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title><rdf:Alt>",
        );
        for _ in 0..1000 {
            p.push_str("<rdf:li/>");
        }
        p.push_str("</rdf:Alt></dc:title></rdf:Description></rdf:RDF></x:xmpmeta>");
        let no = AtomicBool::new(false);
        let mem = Mem::with_max(u64::MAX);
        let m = super::parse_xmp(p.as_bytes(), &mem, &no)
            .expect("ok")
            .expect("packet");
        assert_eq!(m.rdf.description.title.len(), 1000);
        assert_eq!(mem.used(), 0);
        assert!(mem.peak() >= 1000 * size_of::<String>() as u64);
        let small = Mem::with_max(1000 * size_of::<String>() as u64 / 2);
        assert!(matches!(
            super::parse_xmp(p.as_bytes(), &small, &no),
            Err(Error::Alloc { .. })
        ));
        let yes = AtomicBool::new(true);
        assert!(matches!(
            super::parse_xmp(p.as_bytes(), &mem, &yes),
            Err(Error::Cancelled)
        ));
    }

    const PACKET: &str = "<?xpacket begin='\u{feff}' id='W5M0MpCehiHzreSzNTczkc9d'?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">\
<dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">A &amp; B</rdf:li></rdf:Alt></dc:title>\
<dc:creator><rdf:Seq><rdf:li>Ann</rdf:li><rdf:li>Bob</rdf:li></rdf:Seq></dc:creator>\
<xmp:CreatorTool>Tool</xmp:CreatorTool><xmp:ModifyDate>2006-06-05T21:58:13-05:00</xmp:ModifyDate>\
<pdf:Producer><![CDATA[Prod]]></pdf:Producer><pdf:Keywords>a, b</pdf:Keywords>\
</rdf:Description></rdf:RDF></x:xmpmeta>\n<?xpacket end='w'?>";

    #[test]
    fn parses_a_packet() {
        let m = parse_xmp(PACKET.as_bytes()).expect("packet");
        let d = &m.rdf.description;
        assert_eq!(d.title, ["A & B"]);
        assert_eq!(d.author, ["Ann", "Bob"]);
        assert_eq!(d.creator, "Tool");
        assert_eq!(d.producer, "Prod");
        assert_eq!(d.keywords, "a, b");
        let t = d.mod_date.expect("date");
        // 2006-06-05T21:58:13-05:00 is 2006-06-06T02:58:13Z.
        assert_eq!(t.unix, 1_149_562_693);
        assert_eq!(t.utc_offset_min, -300);
        assert!(d.creation_date.is_none());
        assert!(!d.trapped);
    }

    #[test]
    fn the_wrong_root_or_syntax_fails() {
        assert!(parse_xmp(b"<a/>").is_none());
        assert!(parse_xmp(b"").is_none());
        assert!(parse_xmp(b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><b></x:xmpmeta>").is_none());
        assert!(parse_xmp(b"<xmpmeta xmlns=\"other\"/>").is_none());
        // The RDF element has to be in the RDF name space.
        assert!(parse_xmp(b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><RDF/></x:xmpmeta>").is_none());
        // An unterminated packet.
        assert!(parse_xmp(b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">").is_none());
    }

    #[test]
    fn a_bad_date_or_boolean_fails_the_whole_packet() {
        let head = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">";
        let tail = "</rdf:Description></rdf:RDF></x:xmpmeta>";
        let ok = format!(
            "{head}<xmp:CreateDate>2006</xmp:CreateDate><pdf:Trapped>True</pdf:Trapped>{tail}"
        );
        assert!(parse_xmp(ok.as_bytes()).is_some());
        let bad_date = format!("{head}<xmp:CreateDate>June</xmp:CreateDate>{tail}");
        assert!(parse_xmp(bad_date.as_bytes()).is_none());
        // strconv.ParseBool rejects "Unknown".
        let bad_bool = format!("{head}<pdf:Trapped>Unknown</pdf:Trapped>{tail}");
        assert!(parse_xmp(bad_bool.as_bytes()).is_none());
    }

    #[test]
    fn the_layouts_of_user_dates() {
        let p = |s: &str| parse_xmp_date(s).ok();
        assert_eq!(p("2006").map(|t| t.unix), Some(1_136_073_600));
        assert_eq!(p("2006-01").map(|t| t.unix), Some(1_136_073_600));
        assert_eq!(p("2006-01-02").map(|t| t.unix), Some(1_136_160_000));
        assert_eq!(p("2006-01-02T15:04").map(|t| t.unix), Some(1_136_214_240));
        assert_eq!(
            p("2006-01-02T15:04:05.5Z").map(|t| (t.unix, t.nanos)),
            Some((1_136_214_245, 500_000_000))
        );
        assert_eq!(
            p("2006-01-02T15:04+01:00").map(|t| (t.unix, t.utc_offset_min)),
            Some((1_136_210_640, 60))
        );
        assert!(p("2006-02-30").is_none());
        assert!(p("2006-01-02T15:04:05+0100").is_none());
        assert!(p("2006-1-02").is_none());
        assert!(p("2006-01-02T5:04").is_some());
        assert!(p("").is_none());
    }

    #[test]
    fn zero_dates() {
        let t = parse_xmp_date("0001-01-01T00:00:00Z").ok();
        assert!(t.is_some_and(|t| t.is_zero()));
    }

    #[test]
    fn entities_and_depth() {
        let head = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">";
        let tail = "</rdf:Description></rdf:RDF></x:xmpmeta>";
        let s = format!("{head}<xmp:CreatorTool>&#65;&#x42;&lt;</xmp:CreatorTool>{tail}");
        let m = parse_xmp(s.as_bytes()).expect("entities");
        assert_eq!(m.rdf.description.creator, "AB<");
        let s = format!("{head}<xmp:CreatorTool>&bogus;</xmp:CreatorTool>{tail}");
        assert!(parse_xmp(s.as_bytes()).is_none());
        let s = format!("{head}<xmp:CreatorTool>&#0;</xmp:CreatorTool>{tail}");
        assert!(parse_xmp(s.as_bytes()).is_none());
    }
}
