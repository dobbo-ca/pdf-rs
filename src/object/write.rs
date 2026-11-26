//! The three serializers of the object model (docs/design.md section 4):
//!
//! * `pdf_string` is Go's `PDFString`. A `Stream` nested in a dict or an array is dropped
//!   silently, as Go's type switch falls into its `default:` branch.
//! * `go_string` is Go's `String()` (the indented debug form). `fileID` hashes it
//!   (crypto.go:2371-2376), so it is byte-exact.
//! * [`append_pdf_object`] is `appendPDFObject` of the writer (writeObjects_pdf.go). It differs
//!   from `pdf_string` in the spacing rules for arrays and dicts and in failing on a `Stream`.
//!
//! Go's `LazyObjectStreamObject` case of `appendPDFObject` needs the decoded object stream and
//! belongs to the writer package.

use super::string::{decode_name, encode_name};
use super::{Dict, Object, Ref};
use crate::error::Error;
use crate::ledger::{Held, Mem};

/// `strconv.FormatFloat(f, 'f', prec, 64)`, including its spelling of NaN and the infinities.
pub fn format_float(f: f64, prec: usize) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        let s = if f > 0.0 { "+Inf" } else { "-Inf" };
        s.to_string()
    } else {
        format!("{f:.prec$}")
    }
}

/// `Float.PDFString`: 12 decimals (the longest precision met so far was a Type 3 font matrix).
pub fn pdf_float(f: f64) -> String {
    format_float(f, 12)
}

impl Ref {
    /// `IndirectRef.PDFString`: `5 0 R`.
    pub fn pdf_string(&self) -> String {
        format!("{} {} R", self.num, self.generation)
    }

    /// `IndirectRef.String`: `(5 0 R)`.
    pub fn go_string(&self) -> String {
        format!("({} {} R)", self.num, self.generation)
    }
}

/// `Name.PDFString`: `/` and the encoded name; the empty name is `/ `.
fn push_name(out: &mut Vec<u8>, n: &[u8]) {
    out.push(b'/');
    if n.is_empty() {
        out.push(b' ');
    } else {
        out.extend_from_slice(&encode_name(n));
    }
}

/// The text of every object except `Null`, `Array`, `Dict` and `Stream`.
fn push_simple(out: &mut Vec<u8>, o: &Object) {
    match o {
        Object::Bool(b) => out.extend_from_slice(if *b { b"true".as_slice() } else { b"false" }),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Float(f) => out.extend_from_slice(pdf_float(*f).as_bytes()),
        Object::Name(n) => push_name(out, n),
        Object::String(s) => {
            out.push(b'(');
            out.extend_from_slice(s);
            out.push(b')');
        }
        Object::Hex(h) => {
            out.push(b'<');
            out.extend_from_slice(h);
            out.push(b'>');
        }
        Object::Ref(r) => out.extend_from_slice(r.pdf_string().as_bytes()),
        Object::Null | Object::Array(_) | Object::Dict(_) | Object::Stream(_) => {}
    }
}

fn push_pdf_dict(out: &mut Vec<u8>, d: &Dict) {
    out.extend_from_slice(b"<<");
    for (k, v) in d.iter() {
        let key = encode_name(k);
        match v {
            Object::Null => {
                out.push(b'/');
                out.extend_from_slice(&key);
                out.extend_from_slice(b" null");
            }
            Object::Dict(x) => {
                out.push(b'/');
                out.extend_from_slice(&key);
                push_pdf_dict(out, x);
            }
            Object::Array(x) => {
                out.push(b'/');
                out.extend_from_slice(&key);
                push_pdf_array(out, x);
            }
            Object::Ref(_) | Object::Integer(_) | Object::Float(_) | Object::Bool(_) => {
                out.push(b'/');
                out.extend_from_slice(&key);
                out.push(b' ');
                push_simple(out, v);
            }
            Object::Name(_) | Object::String(_) | Object::Hex(_) => {
                out.push(b'/');
                out.extend_from_slice(&key);
                push_simple(out, v);
            }
            Object::Stream(_) => {}
        }
    }
    out.extend_from_slice(b">>");
}

fn push_pdf_array(out: &mut Vec<u8>, a: &[Object]) {
    out.push(b'[');
    let mut first = true;
    for entry in a {
        let sep = !first;
        first = false;
        match entry {
            Object::Dict(d) => push_pdf_dict(out, d),
            Object::Array(x) => push_pdf_array(out, x),
            Object::Name(_) => push_simple(out, entry),
            Object::Null => {
                if sep {
                    out.push(b' ');
                }
                out.extend_from_slice(b"null");
            }
            Object::Ref(_)
            | Object::Integer(_)
            | Object::Float(_)
            | Object::Bool(_)
            | Object::String(_)
            | Object::Hex(_) => {
                if sep {
                    out.push(b' ');
                }
                push_simple(out, entry);
            }
            Object::Stream(_) => {}
        }
    }
    out.push(b']');
}

/// Where the indented `String()` writers put their bytes: a `Vec`, a [`Capped`] text or a hasher.
/// `full` turns true once the sink wants no more, so the writers stop early.
pub trait Sink {
    /// Append `b`.
    fn put(&mut self, b: &[u8]);
    /// True once further bytes are dropped.
    fn full(&self) -> bool {
        false
    }
}

impl Sink for Vec<u8> {
    fn put(&mut self, b: &[u8]) {
        self.extend_from_slice(b);
    }
}

/// A [`Sink`] that keeps the first `cap` bytes and remembers that it dropped the rest.
pub struct Capped {
    /// The bytes kept.
    pub buf: Vec<u8>,
    cap: usize,
    /// True if bytes were dropped.
    pub cut: bool,
}

impl Capped {
    /// An empty sink keeping at most `cap` bytes.
    pub fn new(cap: usize) -> Self {
        Capped {
            buf: Vec::new(),
            cap,
            cut: false,
        }
    }
}

impl Sink for Capped {
    fn put(&mut self, b: &[u8]) {
        let room = self.cap - self.buf.len();
        if b.len() > room {
            self.cut = true;
        }
        self.buf.extend_from_slice(&b[..b.len().min(room)]);
    }
    fn full(&self) -> bool {
        self.cut
    }
}

/// What `indentedString` prints for an entry that is neither a dict nor an array: `null`, a name
/// decoded (empty if it does not decode), else the `String()` of the object.
fn indented_leaf(o: &Object, out: &mut dyn Sink) {
    match o {
        Object::Null => out.put(b"null"),
        Object::Name(n) => {
            if let Ok(c) = decode_name(n) {
                out.put(&c);
            }
        }
        _ => o.write_go(out),
    }
}

fn push_tabs(out: &mut dyn Sink, mut n: usize) {
    const TABS: [u8; 64] = [b'\t'; 64];
    while n > 0 && !out.full() {
        let k = n.min(TABS.len());
        out.put(&TABS[..k]);
        n -= k;
    }
}

fn dict_indented(d: &Dict, level: usize, out: &mut dyn Sink) {
    out.put(b"<<\n");
    for (k, v) in d.iter() {
        if out.full() {
            return;
        }
        push_tabs(out, level);
        out.put(b"<");
        out.put(k);
        out.put(b", ");
        match v {
            Object::Dict(x) => dict_indented(x, level + 1, out),
            Object::Array(x) => array_indented(x, level + 1, out),
            _ => indented_leaf(v, out),
        }
        out.put(b">\n");
    }
    push_tabs(out, level.saturating_sub(1));
    out.put(b">>");
}

fn array_indented(a: &[Object], level: usize, out: &mut dyn Sink) {
    out.put(b"[");
    let mut first = true;
    for entry in a {
        if out.full() {
            return;
        }
        let sep: &[u8] = if first { b"" } else { b" " };
        first = false;
        match entry {
            Object::Dict(d) => {
                out.put(b"\n");
                push_tabs(out, level);
                dict_indented(d, level + 1, out);
                out.put(b"\n");
                push_tabs(out, level);
                first = true;
            }
            Object::Array(x) => {
                out.put(sep);
                array_indented(x, level + 1, out);
            }
            _ => {
                out.put(sep);
                indented_leaf(entry, out);
            }
        }
    }
    out.put(b"]");
}

/// An upper bound of the length of `pdf_string` and of [`append_pdf_object`] of `o`, for the
/// ledger charge of the text before it is allocated. A name is counted at three bytes per byte
/// (`#xx`), a float at its integer digits plus one for rounding, a sign, the point and 12
/// decimals.
pub fn pdf_text_bound(o: &Object) -> u64 {
    match o {
        Object::Null | Object::Bool(_) => 5,
        Object::Integer(i) => int_text_bound(*i),
        Object::Float(f) => {
            let a = f.abs();
            let digits = if a.is_finite() && a >= 1.0 {
                a.log10() as u64 + 2
            } else {
                1
            };
            digits + 14
        }
        Object::Name(n) => 2 + 3 * n.len() as u64,
        Object::String(s) | Object::Hex(s) => 2 + s.len() as u64,
        Object::Ref(r) => int_text_bound(r.num) + int_text_bound(r.generation) + 3,
        Object::Array(a) => 2 + a.iter().map(|v| 1 + pdf_text_bound(v)).sum::<u64>(),
        Object::Dict(d) => dict_text_bound(d),
        Object::Stream(sd) => dict_text_bound(&sd.dict),
    }
}

/// The digits of `i` and a sign.
fn int_text_bound(i: i64) -> u64 {
    2 + i.unsigned_abs().checked_ilog10().map_or(0, u64::from)
}

/// [`pdf_text_bound`] of a dictionary.
pub fn dict_text_bound(d: &Dict) -> u64 {
    4 + d
        .iter()
        .map(|(k, v)| 2 + 3 * k.len() as u64 + pdf_text_bound(v))
        .sum::<u64>()
}

/// An empty buffer with room for `n` bytes, charged to `mem` before it is allocated. The
/// [`Held`] is its charge.
///
/// # Errors
/// [`Error::Alloc`] if the ledger refuses `n`, [`Error::OutOfMemory`] if the allocator does.
pub fn text_buffer(n: u64, mem: &Mem) -> Result<(Vec<u8>, Held), Error> {
    let held = mem.charge(n)?;
    let mut out = Vec::new();
    out.try_reserve_exact(usize::try_from(n).map_err(|_| Error::OutOfMemory)?)?;
    Ok((out, held))
}

impl Dict {
    /// `Dict.PDFString`.
    pub fn pdf_string(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_pdf_dict(&mut out, self);
        out
    }

    /// [`Dict::pdf_string`] in a [`text_buffer`] of [`dict_text_bound`], so it never grows.
    ///
    /// # Errors
    /// As [`text_buffer`].
    pub fn pdf_string_charged(&self, mem: &Mem) -> Result<(Vec<u8>, Held), Error> {
        let (mut out, held) = text_buffer(dict_text_bound(self), mem)?;
        push_pdf_dict(&mut out, self);
        Ok((out, held))
    }

    /// `Dict.String`.
    pub fn go_string(&self) -> Vec<u8> {
        let mut out = Vec::new();
        dict_indented(self, 1, &mut out);
        out
    }
}

/// `Array.PDFString`.
pub fn array_pdf_string(a: &[Object]) -> Vec<u8> {
    let mut out = Vec::new();
    push_pdf_array(&mut out, a);
    out
}

/// `Array.String`.
pub fn array_go_string(a: &[Object]) -> Vec<u8> {
    let mut out = Vec::new();
    array_indented(a, 1, &mut out);
    out
}

impl Object {
    /// `PDFString`. A stream gives its dictionary's text (Go's embedded `Dict.PDFString`).
    pub fn pdf_string(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_pdf_object(&mut out, self);
        out
    }

    /// [`Object::pdf_string`] in a [`text_buffer`] of [`pdf_text_bound`], so it never grows.
    ///
    /// # Errors
    /// As [`text_buffer`].
    pub fn pdf_string_charged(&self, mem: &Mem) -> Result<(Vec<u8>, Held), Error> {
        let (mut out, held) = text_buffer(pdf_text_bound(self), mem)?;
        push_pdf_object(&mut out, self);
        Ok((out, held))
    }

    /// `String()`: `%v` of a boolean, `%.2f` of a float, a name as it is, `(s)` for a string,
    /// `<h>` for a hex string, `(1 0 R)` for a reference, and the indented form of a dict or an
    /// array. A stream gives its dictionary's text.
    pub fn go_string(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_go(&mut out);
        out
    }

    /// [`Object::go_string`] streamed into `out`: no child text is copied into its parent.
    pub fn write_go(&self, out: &mut dyn Sink) {
        match self {
            Object::Null => out.put(b"null"),
            Object::Bool(b) => out.put(b.to_string().as_bytes()),
            Object::Integer(i) => out.put(i.to_string().as_bytes()),
            Object::Float(f) => out.put(format_float(*f, 2).as_bytes()),
            Object::Name(n) => out.put(n),
            Object::String(s) => {
                out.put(b"(");
                out.put(s);
                out.put(b")");
            }
            Object::Hex(h) => {
                out.put(b"<");
                out.put(h);
                out.put(b">");
            }
            Object::Ref(r) => out.put(r.go_string().as_bytes()),
            Object::Array(a) => array_indented(a, 1, out),
            Object::Dict(d) => dict_indented(d, 1, out),
            Object::Stream(s) => dict_indented(&s.dict, 1, out),
        }
    }
}

fn push_pdf_object(out: &mut Vec<u8>, o: &Object) {
    match o {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Dict(d) => push_pdf_dict(out, d),
        Object::Stream(s) => push_pdf_dict(out, &s.dict),
        Object::Array(a) => push_pdf_array(out, a),
        _ => push_simple(out, o),
    }
}

fn array_needs_space(o: &Object) -> bool {
    !matches!(o, Object::Dict(_) | Object::Array(_) | Object::Name(_))
}

fn dict_needs_space(o: &Object) -> bool {
    !matches!(
        o,
        Object::Dict(_) | Object::Array(_) | Object::Name(_) | Object::String(_) | Object::Hex(_)
    )
}

/// `appendPDFObject` (writeObjects_pdf.go:24): appends the PDF syntax of `obj` to `dst`.
///
/// # Errors
/// [`Error::Corrupt`] ("unsupported PDF object type types.StreamDict") for a stream, which the
/// object writers handle themselves.
pub fn append_pdf_object(dst: &mut Vec<u8>, obj: &Object) -> Result<(), Error> {
    match obj {
        Object::Null => dst.extend_from_slice(b"null"),
        Object::Dict(d) => append_pdf_dict(dst, d)?,
        Object::Array(a) => append_pdf_array(dst, a)?,
        Object::Stream(_) => {
            return Err(Error::Corrupt(
                "unsupported PDF object type types.StreamDict".into(),
            ));
        }
        _ => push_simple(dst, obj),
    }
    Ok(())
}

fn append_pdf_array(dst: &mut Vec<u8>, a: &[Object]) -> Result<(), Error> {
    dst.push(b'[');
    for (i, obj) in a.iter().enumerate() {
        if i > 0 && array_needs_space(obj) {
            dst.push(b' ');
        }
        append_pdf_object(dst, obj)?;
    }
    dst.push(b']');
    Ok(())
}

fn append_pdf_dict(dst: &mut Vec<u8>, d: &Dict) -> Result<(), Error> {
    dst.extend_from_slice(b"<<");
    for (key, obj) in d.iter() {
        dst.push(b'/');
        dst.extend_from_slice(&encode_name(key));
        if dict_needs_space(obj) {
            dst.push(b' ');
        }
        append_pdf_object(dst, obj)?;
    }
    dst.extend_from_slice(b">>");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::stream::StreamDict;
    use super::*;

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }

    fn r(num: i64, generation: i64) -> Object {
        Object::Ref(Ref { num, generation })
    }

    #[test]
    fn encode_dict() {
        let d = dict(vec![("A()", Object::Integer(1))]);
        assert_eq!(d.pdf_string(), b"<</A#28#29 1>>");
    }

    fn spacing_dict() -> Dict {
        let arr = vec![
            Object::Integer(1),
            Object::name("X"),
            Object::name("Y"),
            Object::Integer(2),
            Object::Dict(Dict::default()),
            Object::Dict(Dict::default()),
            Object::string("s"),
            Object::Hex(b"ab".to_vec()),
            Object::Null,
            r(1, 0),
            Object::Bool(true),
        ];
        dict(vec![
            ("A", Object::Null),
            ("B", Object::Dict(Dict::default())),
            ("C", Object::Array(arr)),
            ("D", Object::name("N")),
            ("E", Object::Float(0.5)),
            ("F", r(5, 0)),
            ("G", Object::string("s")),
            ("H", Object::Hex(b"0A".to_vec())),
            ("I", Object::Bool(false)),
            ("J", Object::Integer(-3)),
        ])
    }

    const SPACING: &[u8] = b"<</A null/B<<>>/C[1/X/Y 2<<>><<>> (s) <ab> null 1 0 R true]/D/N/E 0.500000000000/F 5 0 R/G(s)/H<0A>/I false/J -3>>";

    #[test]
    fn text_bound_covers_both_serializers() {
        let objs = [
            Object::Dict(spacing_dict()),
            Object::Float(-1e300),
            Object::Float(9.9999999999999),
            Object::Float(f64::NAN),
            Object::Integer(i64::MIN),
            Object::Ref(Ref {
                num: i64::MIN,
                generation: -1,
            }),
            Object::name(""),
            Object::name("a b"),
        ];
        for o in objs {
            let n = pdf_text_bound(&o);
            assert!(o.pdf_string().len() as u64 <= n, "{o:?}");
            let mut v = Vec::new();
            append_pdf_object(&mut v, &o).unwrap();
            assert!(v.len() as u64 <= n, "{o:?}");
        }
    }

    #[test]
    fn pdf_string_spacing_and_stream_dropped() {
        let mut d = spacing_dict();
        assert_eq!(d.pdf_string(), SPACING);
        let sd = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        d.set(b"K", Object::Stream(Box::new(sd)));
        // A stream nested in a dict is dropped silently.
        assert_eq!(d.pdf_string(), SPACING);
        // And in an array, after the separator was counted.
        let sd = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        let arr = [Object::Stream(Box::new(sd)), Object::Integer(1)];
        assert_eq!(array_pdf_string(&arr), b"[ 1]");
    }

    #[test]
    fn append_pdf_object_spacing_and_stream_error() {
        let mut d = spacing_dict();
        let mut out = Vec::new();
        assert!(append_pdf_object(&mut out, &Object::Dict(d.clone())).is_ok());
        assert_eq!(out, SPACING);

        let sd = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        d.set(b"K", Object::Stream(Box::new(sd)));
        let mut out = Vec::new();
        let err = append_pdf_object(&mut out, &Object::Dict(d)).err();
        assert_eq!(
            err,
            Some(Error::Corrupt(
                "unsupported PDF object type types.StreamDict".into()
            ))
        );
    }

    #[test]
    fn empty_name_and_names_are_encoded() {
        assert_eq!(Object::name("").pdf_string(), b"/ ");
        assert_eq!(Object::name("Lime Green").pdf_string(), b"/Lime#20Green");
        assert_eq!(Object::Null.pdf_string(), b"null");
        assert_eq!(r(7, 2).pdf_string(), b"7 2 R");
    }

    #[test]
    fn float_formatting_matches_go() {
        // Expected values from Python's correctly rounded '%.12f' and '%.2f', which round exact
        // binary values half to even as Go's strconv does.
        let f12: [(f64, &str); 9] = [
            (0.1, "0.100000000000"),
            (1.0 / 3.0, "0.333333333333"),
            (1e-13, "0.000000000000"),
            (0.0001220703125, "0.000122070312"),
            (0.0003662109375, "0.000366210938"),
            (123456789.12345679, "123456789.123456791043"),
            (-0.0, "-0.000000000000"),
            (-1.5, "-1.500000000000"),
            (1e22, "10000000000000000000000.000000000000"),
        ];
        for (f, want) in f12 {
            assert_eq!(pdf_float(f), want, "{f:e}");
        }
        let f2: [(f64, &str); 7] = [
            (0.125, "0.12"),
            (0.375, "0.38"),
            (2.675, "2.67"),
            (1.005, "1.00"),
            (0.005, "0.01"),
            (7.891, "7.89"),
            (-0.001, "-0.00"),
        ];
        for (f, want) in f2 {
            assert_eq!(format_float(f, 2), want, "{f:e}");
        }
        assert_eq!(pdf_float(f64::NAN), "NaN");
        assert_eq!(pdf_float(f64::INFINITY), "+Inf");
        assert_eq!(pdf_float(f64::NEG_INFINITY), "-Inf");
    }

    #[test]
    fn go_string_indented_form() {
        let d = dict(vec![
            ("A", Object::Integer(1)),
            ("B", Object::name("N#")),
            ("C", Object::Dict(dict(vec![("D", Object::Float(1.234))]))),
            (
                "E",
                Object::Array(vec![
                    Object::Integer(1),
                    Object::Dict(dict(vec![("F", Object::Null)])),
                    Object::name("x"),
                    r(7, 0),
                ]),
            ),
        ]);
        let want: &[u8] = b"<<\n\t<A, 1>\n\t<B, >\n\t<C, <<\n\t\t<D, 1.23>\n\t>>>\n\t<E, [1\n\t\t<<\n\t\t\t<F, null>\n\t\t>>\n\t\tx (7 0 R)]>\n>>";
        assert_eq!(d.go_string(), want);
        assert_eq!(Object::Dict(d).go_string(), want);
    }

    #[test]
    fn capped_go_text_stops_early() {
        let mut v = Object::Integer(7);
        for _ in 0..1000 {
            v = Object::Array(vec![v, Object::Integer(1)]);
        }
        let full = v.go_string();
        let mut c = Capped::new(100);
        v.write_go(&mut c);
        assert!(c.cut);
        assert_eq!(c.buf, full[..100]);
        let mut c = Capped::new(full.len());
        v.write_go(&mut c);
        assert!(!c.cut);
        assert_eq!(c.buf, full);
    }

    #[test]
    fn go_string_of_scalars() {
        assert_eq!(Object::Bool(true).go_string(), b"true");
        assert_eq!(Object::Integer(-4).go_string(), b"-4");
        assert_eq!(Object::Float(2.0).go_string(), b"2.00");
        assert_eq!(Object::name("A#42").go_string(), b"A#42");
        assert_eq!(Object::string("a\\(").go_string(), b"(a\\()");
        assert_eq!(Object::Hex(b"4a".to_vec()).go_string(), b"<4a>");
        assert_eq!(r(1, 0).go_string(), b"(1 0 R)");
        assert_eq!(
            array_go_string(&[Object::Integer(1), Object::Array(vec![Object::Integer(2)])]),
            b"[1 [2]]"
        );
        // A name in an indented form is decoded.
        assert_eq!(array_go_string(&[Object::name("A#42")]), b"[AB]");
    }
}
