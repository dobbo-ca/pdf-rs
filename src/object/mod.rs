//! The PDF object model (docs/design.md section 4; Go: pkg/pdfcpu/types).
//!
//! * [`Object`] is Go's `types.Object` interface as an enum. Go's `nil` is [`Object::Null`];
//!   `Name`, `String` and `Hex` are byte vectors (`String` holds the escaped body, `Hex` the
//!   digits as stored).
//! * [`Dict`] is ordered by decoded name bytes; [`Array`] is a `Vec`.
//! * [`StreamDict`] and its two variants ([`ObjectStreamDict`], [`XRefStreamDict`]) are in
//!   [`stream`]; their decode and encode pipeline is added by the filter package.
//! * The serializers are in [`write`]: `pdf_string` (Go's `PDFString`), `go_string` (Go's
//!   `String`) and `append_pdf_object` (`appendPDFObject`).
//!
//! Not ported because nothing the port reaches uses them: the unit conversions, paper sizes and
//! `Dim` of types/layout.go and types/paperSize.go, `QuadLiteral`, and the `ByteSize`, `IntSet`
//! and `StringSet` helpers.

pub mod array;
pub mod date;
pub mod dict;
pub mod stream;
mod stream_codec;
pub mod string;
pub mod text;
pub mod write;

pub use array::Array;
pub use dict::Dict;
pub use stream::{LazyRef, ObjectStreamDict, PdfFilter, StreamDict, XRefStreamDict};

/// `FreeHeadGeneration`: the generation number of the head of the free list.
pub const FREE_HEAD_GENERATION: i64 = 65535;

/// A PDF indirect reference (Go `IndirectRef`). The generation field is `generation` because
/// `gen` is a reserved word in edition 2024.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref {
    pub num: i64,
    pub generation: i64,
}

/// A PDF object.
#[derive(Debug, Clone)]
pub enum Object {
    /// Go's `nil`.
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    /// The decoded name.
    Name(Vec<u8>),
    /// A string literal: the escaped body, without the parentheses.
    String(Vec<u8>),
    /// A hex string: the digits as stored, without the angle brackets.
    Hex(Vec<u8>),
    Ref(Ref),
    Array(Array),
    Dict(Dict),
    Stream(Box<StreamDict>),
}

impl Object {
    pub fn is_null(&self) -> bool {
        matches!(self, Object::Null)
    }

    /// A name object.
    pub fn name(s: impl AsRef<[u8]>) -> Object {
        Object::Name(s.as_ref().to_vec())
    }

    /// A string literal object; `s` is the escaped body.
    pub fn string(s: impl AsRef<[u8]>) -> Object {
        Object::String(s.as_ref().to_vec())
    }

    /// `NewHexLiteral`: the hex string of the bytes `b`, written in lower case.
    pub fn hex_from_bytes(b: &[u8]) -> Object {
        Object::Hex(string::hex_encode(b))
    }

    /// An estimate of the heap a clone of this object allocates, for a ledger charge. Stream
    /// buffers are shared by clones and not counted.
    pub fn copy_bytes(&self) -> u64 {
        match self {
            Object::Name(v) | Object::String(v) | Object::Hex(v) => v.len() as u64,
            Object::Array(a) => array_copy_bytes(a),
            Object::Dict(d) => dict_copy_bytes(d),
            Object::Stream(sd) => {
                size_of::<StreamDict>() as u64
                    + dict_copy_bytes(&sd.dict)
                    + pipeline_copy_bytes(&sd.filter_pipeline)
            }
            _ => 0,
        }
    }
}

/// The share of a stream's filter pipeline in [`Object::copy_bytes`]: a clone has exactly its
/// length, and copies each name and parameter dictionary.
fn pipeline_copy_bytes(p: &[PdfFilter]) -> u64 {
    std::mem::size_of_val(p) as u64
        + p.iter()
            .map(|f| f.name.len() as u64 + f.decode_parms.as_ref().map_or(0, dict_copy_bytes))
            .sum::<u64>()
}

/// [`Object::copy_bytes`] of an array: a clone has exactly its length.
pub fn array_copy_bytes(a: &[Object]) -> u64 {
    std::mem::size_of_val(a) as u64 + a.iter().map(Object::copy_bytes).sum::<u64>()
}

/// [`Object::copy_bytes`] of a dictionary: a clone has the B-tree nodes of the original, which
/// can be half full, so each entry is two key and value slots, and 16 for the internal nodes and
/// the key's allocation.
pub fn dict_copy_bytes(d: &Dict) -> u64 {
    d.iter().map(|(k, v)| entry_copy_bytes(k, v)).sum()
}

/// The share of one entry `k`, `v` in [`dict_copy_bytes`].
pub fn entry_copy_bytes(k: &[u8], v: &Object) -> u64 {
    let slot = (size_of::<Vec<u8>>() + size_of::<Object>()) as u64;
    2 * slot + 16 + k.len() as u64 + v.copy_bytes()
}

/// `Point`: a user space location.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// `Translate`.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }
}

/// `Rectangle`: a rectangular region in user space.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub ll: Point,
    pub ur: Point,
}

impl Rect {
    /// `NewRectangle`.
    pub fn new(llx: f64, lly: f64, urx: f64, ury: f64) -> Rect {
        Rect {
            ll: Point::new(llx, lly),
            ur: Point::new(urx, ury),
        }
    }

    /// `RectForArray`: `None` unless the array has four elements; a non-number counts as 0.
    pub fn from_array(arr: &[Object]) -> Option<Rect> {
        fn num(o: &Object) -> f64 {
            match o {
                Object::Float(f) => *f,
                Object::Integer(i) => *i as f64,
                _ => 0.0,
            }
        }
        match arr {
            [llx, lly, urx, ury] => Some(Rect::new(num(llx), num(lly), num(urx), num(ury))),
            _ => None,
        }
    }

    /// `RectForDim`.
    pub fn for_dim(width: f64, height: f64) -> Rect {
        Rect::new(0.0, 0.0, width, height)
    }

    pub fn width(&self) -> f64 {
        self.ur.x - self.ll.x
    }

    pub fn height(&self) -> f64 {
        self.ur.y - self.ll.y
    }

    /// `FitsWithin`.
    pub fn fits_within(&self, r2: &Rect) -> bool {
        self.width() <= r2.width() && self.height() <= r2.height()
    }

    /// `Visible`.
    pub fn visible(&self) -> bool {
        self.width() != 0.0 && self.height() != 0.0
    }

    /// `AspectRatio`.
    pub fn aspect_ratio(&self) -> f64 {
        self.width() / self.height()
    }

    pub fn landscape(&self) -> bool {
        self.aspect_ratio() > 1.0
    }

    pub fn portrait(&self) -> bool {
        self.aspect_ratio() < 1.0
    }

    /// `Contains`. Go compares `p.Y` with `r.LL.Y` on both sides (the upper bound is not
    /// `r.UR.Y`); that is kept.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.ll.x && p.x <= self.ur.x && p.y >= self.ll.y && p.y <= self.ll.y
    }

    /// `Translate`.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.ll.translate(dx, dy);
        self.ur.translate(dx, dy);
    }

    /// `Center`.
    pub fn center(&self) -> Point {
        Point::new(
            self.ur.x - self.width() / 2.0,
            self.ur.y - self.height() / 2.0,
        )
    }

    /// `CroppedCopy`: the rectangle with `margin` taken off every side.
    pub fn cropped_copy(&self, margin: f64) -> Rect {
        Rect::new(
            self.ll.x + margin,
            self.ll.y + margin,
            self.ur.x - margin,
            self.ur.y - margin,
        )
    }

    /// `Array`: the PDF representation, four floats.
    pub fn to_array(&self) -> Array {
        array::new_number_array(&[self.ll.x, self.ll.y, self.ur.x, self.ur.y])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_bytes_covers_the_clone() {
        let o = Object::Array(vec![Object::Null, Object::name(b"abc")]);
        let slot = size_of::<Object>() as u64;
        assert_eq!(o.copy_bytes(), 2 * slot + 3);
        let mut d = Dict::default();
        d.0.insert(b"Kids".to_vec(), o.clone());
        let entry = 2 * (size_of::<Vec<u8>>() as u64 + slot) + 16 + 4;
        assert_eq!(Object::Dict(d).copy_bytes(), entry + o.copy_bytes());
    }

    #[test]
    fn copy_bytes_counts_the_stream_pipeline() {
        let parms = Dict(std::collections::BTreeMap::from([(
            b"Predictor".to_vec(),
            Object::Integer(12),
        )]));
        let mut sd = StreamDict::default();
        let bare = Object::Stream(Box::new(sd.clone())).copy_bytes();
        sd.filter_pipeline = vec![PdfFilter {
            name: b"FlateDecode".to_vec(),
            decode_parms: Some(parms.clone()),
        }];
        let o = Object::Stream(Box::new(sd));
        let want = bare + size_of::<PdfFilter>() as u64 + 11 + dict_copy_bytes(&parms);
        assert_eq!(o.copy_bytes(), want);
    }

    #[test]
    fn rect_array_round_trip() {
        let arr = [
            Object::Integer(0),
            Object::Float(1.5),
            Object::Integer(612),
            Object::Name(b"x".to_vec()),
        ];
        let r = Rect::from_array(&arr);
        assert_eq!(r, Some(Rect::new(0.0, 1.5, 612.0, 0.0)));
        assert_eq!(Rect::from_array(&arr[..3]), None);

        let r = Rect::for_dim(612.0, 792.0);
        assert_eq!((r.width(), r.height()), (612.0, 792.0));
        assert!(r.visible() && r.portrait() && !r.landscape());
        assert_eq!(r.center(), Point::new(306.0, 396.0));
        assert_eq!(r.cropped_copy(12.0), Rect::new(12.0, 12.0, 600.0, 780.0));
        assert!(r.fits_within(&Rect::for_dim(700.0, 800.0)));
        assert_eq!(
            write::array_pdf_string(&r.to_array()),
            b"[0.000000000000 0.000000000000 612.000000000000 792.000000000000]"
        );

        let mut moved = r;
        moved.translate(1.0, 2.0);
        assert_eq!(moved, Rect::new(1.0, 2.0, 613.0, 794.0));
    }

    #[test]
    fn rect_contains_keeps_gos_upper_bound() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(5.0, 0.0)));
        assert!(!r.contains(Point::new(5.0, 5.0)));
    }
}
