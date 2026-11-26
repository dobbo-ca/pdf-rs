//! The part of Go's `encoding/gob` that pdf-go reaches (docs/design.md section 1, row 3).
//! `dctDecode.go` gob-encodes the image `image/jpeg` returns into the stream's `Content`: the
//! limit check, the cap charge and the render all depend on those bytes. This module writes the
//! exact bytes for the four result types, `*image.Gray`, `*image.YCbCr`, `*image.RGBA` and
//! `*image.CMYK`, and reads such bytes back into an `image.CMYK` with gob's match-by-field-name
//! rule (`renderCMYKToPng`, writeImage.go:1119-1146).
//!
//! Ported from Go 1.27.4 `encoding/gob`: `encoder.go` (`writeMessage`, `sendActualType`,
//! `EncodeValue`), `encode.go` (`encodeUint`, `encodeInt`, `encodeStruct`, the zero-value rules),
//! `type.go` (`newTypeObject`, the id assignment) and `decoder.go`/`decode.go` (`recvMessage`,
//! `decodeTypeSequence`, `compileDec`, `decodeStruct`, `compatibleType`).
//!
//! Go assigns type ids per process, on a type's first use: the first user type gets
//! `firstUserId` (64), and `Rectangle` and `Point` keep the ids of whichever image type came
//! first. [`encode_image`] therefore keeps a process-wide registry ([`TypeIds`]); the explicit
//! [`encode_image_with`] takes one, so tests are not order-dependent.
//!
//! Subset: the encoder writes only these four struct types. The decoder reads struct type
//! definitions, and skips unknown remote fields of the basic types and of structs; a field of an
//! array, slice or map type, an interface, or a GobEncoder type is [`Error::Gob`]
//! (`unsupported-type`) where Go would skip it.

use crate::error::{Error, Sentinel};
use crate::filter::gojpeg::GoImage;
use jpeg::{Rect, Subsample};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// `firstUserId` (type.go:167).
const FIRST_USER_ID: u32 = 64;
/// `tInt` and `tBytes`: the ids of the predefined types (type.go:258-265).
const T_INT: u32 = 2;
const T_BYTES: u32 = 5;
/// `tooBig` on a 64-bit machine: `(1 << 30) << (^uint(0) >> 62)` (decoder.go:19).
const TOO_BIG: u64 = 1 << 33;
/// How deep a skipped struct may nest. Go bounds the ops it compiles; here it bounds the stack.
const MAX_IGNORE_DEPTH: usize = 64;

/// The fields of `image.Gray`, `image.RGBA` and `image.CMYK`, and of `image.YCbCr`.
const PACKED_FIELDS: &[&str] = &["Pix", "Stride", "Rect"];
const YCBCR_FIELDS: &[&str] = &[
    "Y",
    "Cb",
    "Cr",
    "YStride",
    "CStride",
    "SubsampleRatio",
    "Rect",
];

/// Which image type: the index into [`TypeIds`].
#[derive(Clone, Copy)]
enum Top {
    Gray = 0,
    YCbCr = 1,
    Rgba = 2,
    Cmyk = 3,
}

/// The type ids Go's process-global registry has handed out (`setTypeId`). 0 is unassigned.
#[derive(Debug, Clone, Default)]
pub struct TypeIds {
    next: u32,
    top: [u32; 4],
    rectangle: u32,
    point: u32,
}

impl TypeIds {
    /// A registry in which nothing has been encoded yet.
    pub const fn new() -> TypeIds {
        TypeIds {
            next: 0,
            top: [0; 4],
            rectangle: 0,
            point: 0,
        }
    }

    fn alloc(&mut self) -> u32 {
        if self.next < FIRST_USER_ID {
            self.next = FIRST_USER_ID;
        }
        let id = self.next;
        self.next += 1;
        id
    }

    /// The ids of the image type, `Rectangle` and `Point`, assigned in the order of
    /// `newTypeObject`: the struct before its fields, a field's struct before the next field.
    fn assign(&mut self, t: Top) -> (u32, u32, u32) {
        let i = t as usize;
        if self.top[i] == 0 {
            self.top[i] = self.alloc();
            if self.rectangle == 0 {
                self.rectangle = self.alloc();
                self.point = self.alloc();
            }
        }
        (self.top[i], self.rectangle, self.point)
    }
}

/// Go's `types` map and `idToTypeSlice`, for the one process.
static PROCESS_IDS: Mutex<TypeIds> = Mutex::new(TypeIds::new());

/// One gob message, as one `Write` call of the encoder: the length prefix and the body. The
/// pixel planes are borrowed from the image, not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message<'a> {
    parts: Vec<Cow<'a, [u8]>>,
    len: usize,
}

impl<'a> Message<'a> {
    /// The length of the `Write` call: the prefix and the body.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the message has no bytes (never, there is always a prefix).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The pieces to write, in order.
    pub fn parts(&self) -> &[Cow<'a, [u8]>] {
        &self.parts
    }

    /// The message as one buffer.
    ///
    /// # Errors
    /// [`Error::OutOfMemory`] when the allocator refuses the buffer.
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        let mut v = Vec::new();
        v.try_reserve_exact(self.len)?;
        for p in &self.parts {
            v.extend_from_slice(p);
        }
        Ok(v)
    }
}

/// `encodeUint`.
fn put_uint(v: &mut Vec<u8>, x: u64) {
    if x <= 0x7F {
        v.push(x as u8);
        return;
    }
    // The count of the high zero bytes, then the negated byte count and the other bytes.
    let skip = (x.leading_zeros() >> 3) as usize;
    v.push((skip as u8).wrapping_sub(8));
    v.extend_from_slice(&x.to_be_bytes()[skip..]);
}

/// `encodeInt`.
fn put_int(v: &mut Vec<u8>, i: i64) {
    let x = if i < 0 {
        (((!i) as u64) << 1) | 1
    } else {
        (i as u64) << 1
    };
    put_uint(v, x);
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    put_uint(v, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
}

/// `writeMessage`'s framing: the body preceded by its length.
fn message(body: Vec<Cow<'_, [u8]>>) -> Result<Message<'_>, Error> {
    let body_len = body
        .iter()
        .try_fold(0usize, |n, p| n.checked_add(p.len()))
        .ok_or(Error::Gob("message-too-big"))?;
    if body_len as u64 >= TOO_BIG {
        return Err(Error::Gob("message-too-big"));
    }
    let mut prefix = Vec::new();
    put_uint(&mut prefix, body_len as u64);
    let len = prefix.len() + body_len;
    let mut parts = Vec::with_capacity(body.len() + 1);
    parts.push(Cow::Owned(prefix));
    parts.extend(body);
    Ok(Message { parts, len })
}

/// The message that defines a struct type (`sendActualType`): the negated id, then the
/// `wireType` with only `StructT` set. `fields` are the `fieldType`s, name and type id.
fn def_message<'a>(name: &str, id: u32, fields: &[(&str, u32)]) -> Result<Message<'a>, Error> {
    let mut v = Vec::new();
    put_int(&mut v, -i64::from(id));
    v.push(3); // wireType.StructT
    v.push(1); // structType.CommonType
    v.push(1); // CommonType.Name
    put_str(&mut v, name);
    v.push(1); // CommonType.Id
    put_int(&mut v, i64::from(id));
    v.push(0);
    v.push(1); // structType.Field
    put_uint(&mut v, fields.len() as u64);
    for &(n, t) in fields {
        v.push(1); // fieldType.Name
        put_str(&mut v, n);
        v.push(1); // fieldType.Id
        put_int(&mut v, i64::from(t));
        v.push(0);
    }
    v.push(0); // structType
    v.push(0); // wireType
    message(vec![Cow::Owned(v)])
}

/// A value message under construction: the type id, then the fields of a struct. A field whose
/// value is zero is left out, as `encodeStruct` does, and a nested struct is always sent.
struct Body<'a> {
    parts: Vec<Cow<'a, [u8]>>,
    cur: Vec<u8>,
    /// `state.fieldnum`.
    last: i64,
}

impl<'a> Body<'a> {
    fn new(type_id: u32) -> Body<'a> {
        let mut cur = Vec::new();
        put_int(&mut cur, i64::from(type_id));
        Body {
            parts: Vec::new(),
            cur,
            last: -1,
        }
    }

    fn flush(&mut self) {
        if !self.cur.is_empty() {
            self.parts.push(Cow::Owned(std::mem::take(&mut self.cur)));
        }
    }

    /// `state.update`: the delta to field `n`.
    fn field(&mut self, n: i64) {
        put_uint(&mut self.cur, (n - self.last) as u64);
        self.last = n;
    }

    fn bytes(&mut self, n: i64, data: &'a [u8]) {
        if data.is_empty() {
            return;
        }
        self.field(n);
        put_uint(&mut self.cur, data.len() as u64);
        self.flush();
        self.parts.push(Cow::Borrowed(data));
    }

    fn int(&mut self, n: i64, x: i64) {
        if x != 0 {
            self.field(n);
            put_int(&mut self.cur, x);
        }
    }

    fn rect(&mut self, n: i64, r: Rect) {
        self.field(n);
        put_uint(&mut self.cur, 1); // Rectangle.Min
        put_point(&mut self.cur, r.min_x, r.min_y);
        put_uint(&mut self.cur, 1); // Rectangle.Max
        put_point(&mut self.cur, r.max_x, r.max_y);
        self.cur.push(0);
    }

    /// The `Pix`, `Stride`, `Rect` layout of `Gray`, `RGBA` and `CMYK`.
    fn packed(&mut self, pix: &'a [u8], stride: usize, rect: Rect) {
        self.bytes(0, pix);
        self.int(1, to_i64(stride));
        self.rect(2, rect);
    }

    fn finish(mut self) -> Vec<Cow<'a, [u8]>> {
        self.cur.push(0);
        self.flush();
        self.parts
    }
}

fn put_point(v: &mut Vec<u8>, x: i32, y: i32) {
    let mut last = -1i64;
    if x != 0 {
        put_uint(v, 1);
        put_int(v, i64::from(x));
        last = 0;
    }
    if y != 0 {
        put_uint(v, (1 - last) as u64);
        put_int(v, i64::from(y));
    }
    v.push(0);
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `image.YCbCrSubsampleRatio`.
fn ratio(r: Subsample) -> i64 {
    match r {
        Subsample::S444 => 0,
        Subsample::S422 => 1,
        Subsample::S420 => 2,
        Subsample::S440 => 3,
        Subsample::S411 => 4,
        Subsample::S410 => 5,
    }
}

/// `enc.Encode(im)` for the image `jpeg.Decode` returned, as the `Write` calls the encoder
/// makes: the type definition of the image, of `Rectangle` and of `Point`, then the value. The
/// type ids come from the process-wide registry, as Go's do.
///
/// # Errors
/// [`Error::Gob`] (`message-too-big`) where Go sets "gob: encoder: message too big".
pub(crate) fn encode_image(img: &GoImage) -> Result<Vec<Message<'_>>, Error> {
    let mut ids = PROCESS_IDS.lock().unwrap_or_else(PoisonError::into_inner);
    encode_image_with(img, &mut ids)
}

/// [`encode_image`] with the given registry.
///
/// # Errors
/// As [`encode_image`].
pub(crate) fn encode_image_with<'a>(
    img: &'a GoImage,
    ids: &mut TypeIds,
) -> Result<Vec<Message<'a>>, Error> {
    let (top, name, fields) = match img {
        GoImage::Gray(_) => (Top::Gray, "Gray", PACKED_FIELDS),
        GoImage::YCbCr(_) => (Top::YCbCr, "YCbCr", YCBCR_FIELDS),
        GoImage::Rgba(_) => (Top::Rgba, "RGBA", PACKED_FIELDS),
        GoImage::Cmyk(_) => (Top::Cmyk, "CMYK", PACKED_FIELDS),
    };
    let (id, rect_id, point_id) = ids.assign(top);
    // Each field's type id: the predefined ids for the basic types, `Rectangle` for `Rect`.
    let types: Vec<(&str, u32)> = fields
        .iter()
        .map(|&n| {
            let t = match n {
                "Rect" => rect_id,
                "Stride" | "YStride" | "CStride" | "SubsampleRatio" => T_INT,
                _ => T_BYTES,
            };
            (n, t)
        })
        .collect();
    let mut b = Body::new(id);
    match img {
        GoImage::Gray(i) => b.packed(&i.pix, i.stride, i.rect),
        GoImage::Rgba(i) => b.packed(&i.pix, i.stride, i.rect),
        GoImage::Cmyk(i) => b.packed(&i.pix, i.stride, i.rect),
        GoImage::YCbCr(i) => {
            b.bytes(0, &i.y);
            b.bytes(1, &i.cb);
            b.bytes(2, &i.cr);
            b.int(3, to_i64(i.y_stride));
            b.int(4, to_i64(i.c_stride));
            b.int(5, ratio(i.ratio));
            b.rect(6, i.rect);
        }
    }
    Ok(vec![
        def_message(name, id, &types)?,
        def_message(
            "Rectangle",
            rect_id,
            &[("Min", point_id), ("Max", point_id)],
        )?,
        def_message("Point", point_id, &[("X", T_INT), ("Y", T_INT)])?,
        message(b.finish())?,
    ])
}

// ---------------------------------------------------------------------------------------------
// The decoder.

/// `image.Point`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

/// `image.Rectangle`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rectangle {
    pub min: Point,
    pub max: Point,
}

/// The `image.CMYK` that `dec.Decode(&img)` fills. A field the stream does not send stays zero:
/// a YCbCr stream, for one, sets only `Rect`, and `img.At` then indexes an empty `Pix`. `pix`
/// is borrowed from the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmykImage<'a> {
    pub pix: &'a [u8],
    pub stride: i64,
    pub rect: Rectangle,
}

/// What a local field is.
enum Local {
    Int,
    Bytes,
    Struct(&'static [(&'static str, Local)]),
}

const POINT: &[(&str, Local)] = &[("X", Local::Int), ("Y", Local::Int)];
const RECTANGLE: &[(&str, Local)] = &[("Min", Local::Struct(POINT)), ("Max", Local::Struct(POINT))];
const CMYK: &[(&str, Local)] = &[
    ("Pix", Local::Bytes),
    ("Stride", Local::Int),
    ("Rect", Local::Struct(RECTANGLE)),
];

/// A decoded field.
enum Val<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    Struct(Vec<Option<Val<'a>>>),
}

/// A remote struct: its fields' names and type ids.
type Wire = Vec<(String, i64)>;

/// What to do with a remote field.
enum Plan {
    /// Decode it into local field `.0`.
    Local(usize),
    /// Skip it, a value of type `.0`.
    Ignore(i64),
}

/// The bytes of one message (`dec.buf`).
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    /// `decodeUint`.
    fn uint(&mut self) -> Result<u64, Error> {
        let short = Error::Gob("short-data");
        let b = *self.b.get(self.pos).ok_or_else(|| short.clone())?;
        self.pos += 1;
        if b <= 0x7F {
            return Ok(u64::from(b));
        }
        let n = usize::from(b.wrapping_neg());
        if n > 8 {
            return Err(Error::Gob("bad-uint"));
        }
        let bytes = self.take(n).ok_or(short)?;
        Ok(bytes.iter().fold(0u64, |x, &c| (x << 8) | u64::from(c)))
    }

    /// `decodeInt`.
    fn int(&mut self) -> Result<i64, Error> {
        let x = self.uint()?;
        let i = (x >> 1) as i64;
        Ok(if x & 1 != 0 { !i } else { i })
    }

    /// The first integer of a message, `decodeUintReader`'s way: nothing there is `io.EOF`, and
    /// a cut-off one `io.ErrUnexpectedEOF`.
    fn first_int(&mut self) -> Result<i64, Error> {
        if self.remaining() == 0 {
            return Err(Error::Sentinel(Sentinel::Eof));
        }
        self.int().map_err(|e| {
            if e == Error::Gob("short-data") {
                Error::UnexpectedEof
            } else {
                e
            }
        })
    }

    /// A length that must fit in what is left (`getLength`), and that many bytes.
    fn sized(&mut self) -> Result<&'a [u8], Error> {
        let n = self.uint()?;
        let n = usize::try_from(n).map_err(|_| Error::Gob("bad-length"))?;
        self.take(n).ok_or(Error::Gob("bad-length"))
    }

    /// The next field delta of a struct, `None` at its end (a zero delta, or no more data).
    fn delta(&mut self) -> Result<Option<i64>, Error> {
        if self.remaining() == 0 {
            return Ok(None);
        }
        let d = self.uint()?;
        if d > i64::MAX as u64 {
            return Err(Error::Gob("negative-delta"));
        }
        Ok(if d == 0 { None } else { Some(d as i64) })
    }
}

/// The next count-delimited message of the stream (`recvMessage`); `None` at its clean end.
fn next_message<'a>(top: &mut Reader<'a>) -> Result<Option<&'a [u8]>, Error> {
    if top.remaining() == 0 {
        return Ok(None);
    }
    let n = top.uint().map_err(|e| {
        if e == Error::Gob("short-data") {
            Error::UnexpectedEof
        } else {
            e
        }
    })?;
    if n >= TOO_BIG {
        return Err(Error::Gob("bad-count"));
    }
    let n = usize::try_from(n).map_err(|_| Error::Gob("bad-count"))?;
    top.take(n).map(Some).ok_or(Error::UnexpectedEof)
}

/// The field at `delta` past `last`, checked against `len` as `decodeStruct` does.
fn next_field(last: i64, delta: i64, len: usize) -> Result<usize, Error> {
    if last >= len as i64 - delta {
        return Err(Error::Gob("field-range"));
    }
    usize::try_from(last + delta).map_err(|_| Error::Gob("field-range"))
}

/// The `wireType` of a type definition (`decodeValue(tWireType, ..)`): only `StructT` is read.
fn read_wire_type(m: &mut Reader<'_>) -> Result<Wire, Error> {
    let mut last = -1i64;
    let mut out = None;
    while let Some(d) = m.delta()? {
        let f = next_field(last, d, 7)?;
        if f != 2 {
            return Err(Error::Gob("unsupported-type"));
        }
        out = Some(read_struct_type(m)?);
        last = f as i64;
    }
    out.ok_or(Error::Gob("unsupported-type"))
}

/// `structType`: `CommonType` and `Field`.
fn read_struct_type(m: &mut Reader<'_>) -> Result<Wire, Error> {
    let mut last = -1i64;
    let mut fields = Vec::new();
    while let Some(d) = m.delta()? {
        let f = next_field(last, d, 2)?;
        if f == 0 {
            read_pair(m)?;
        } else {
            let n = m.uint()?;
            if n > m.remaining() as u64 {
                return Err(Error::Gob("slice-too-big"));
            }
            for _ in 0..n {
                fields.push(read_pair(m)?);
            }
        }
        last = f as i64;
    }
    Ok(fields)
}

/// `CommonType` or `fieldType`: a name and a type id.
fn read_pair(m: &mut Reader<'_>) -> Result<(String, i64), Error> {
    let mut last = -1i64;
    let (mut name, mut id) = (String::new(), 0);
    while let Some(d) = m.delta()? {
        let f = next_field(last, d, 2)?;
        if f == 0 {
            name = String::from_utf8_lossy(m.sized()?).into_owned();
        } else {
            id = m.int()?;
        }
        last = f as i64;
    }
    Ok((name, id))
}

/// The remote struct `id`, for a local struct.
fn wire_of(wires: &BTreeMap<i64, Wire>, id: i64) -> Result<&Wire, Error> {
    wires
        .get(&id)
        .ok_or(Error::Gob(if id < i64::from(FIRST_USER_ID) {
            "type-mismatch"
        } else {
            "bad-type"
        }))
}

/// `compileDec`: pairs each remote field with the local field of the same name, or marks it to
/// be skipped.
fn compile(remote: &Wire, local: &[(&str, Local)]) -> Result<Vec<Plan>, Error> {
    let mut plan = Vec::with_capacity(remote.len());
    for (name, id) in remote {
        if name.is_empty() {
            return Err(Error::Gob("empty-field-name"));
        }
        let Some(j) = local.iter().position(|(n, _)| *n == name.as_str()) else {
            plan.push(Plan::Ignore(*id));
            continue;
        };
        // `compatibleType`: a struct matches any remote type here, and is checked when it is
        // compiled.
        let ok = match local[j].1 {
            Local::Int => *id == i64::from(T_INT),
            Local::Bytes => *id == i64::from(T_BYTES),
            Local::Struct(_) => true,
        };
        if !ok {
            return Err(Error::Gob("wrong-type"));
        }
        plan.push(Plan::Local(j));
    }
    Ok(plan)
}

/// `ignoreStruct` and the ignore ops: skips one value of remote type `id`.
fn skip(
    m: &mut Reader<'_>,
    id: i64,
    wires: &BTreeMap<i64, Wire>,
    depth: usize,
) -> Result<(), Error> {
    match id {
        1..=4 => {
            m.uint()?;
        }
        5 | 6 => {
            m.sized()?;
        }
        7 => {
            m.uint()?;
            m.uint()?;
        }
        _ => {
            if depth >= MAX_IGNORE_DEPTH {
                return Err(Error::Gob("nesting-too-deep"));
            }
            let w = wires.get(&id).ok_or(Error::Gob("undefined-type"))?;
            let mut last = -1i64;
            while let Some(d) = m.delta()? {
                let f = next_field(last, d, w.len())?;
                let (_, t) = w.get(f).ok_or(Error::Gob("field-range"))?;
                skip(m, *t, wires, depth + 1)?;
                last = f as i64;
            }
        }
    }
    Ok(())
}

/// `decodeStruct` into the local struct `local`.
fn decode_struct<'a>(
    m: &mut Reader<'a>,
    remote: &Wire,
    plan: &[Plan],
    local: &[(&str, Local)],
    wires: &BTreeMap<i64, Wire>,
) -> Result<Vec<Option<Val<'a>>>, Error> {
    let mut vals: Vec<Option<Val<'a>>> = local.iter().map(|_| None).collect();
    let mut last = -1i64;
    while let Some(d) = m.delta()? {
        let f = next_field(last, d, plan.len())?;
        match plan.get(f) {
            Some(Plan::Ignore(id)) => skip(m, *id, wires, 0)?,
            Some(Plan::Local(j)) => {
                let v = match &local[*j].1 {
                    Local::Int => Val::Int(m.int()?),
                    Local::Bytes => Val::Bytes(m.sized()?),
                    Local::Struct(sub) => {
                        let id = remote.get(f).map_or(0, |(_, t)| *t);
                        let w = wire_of(wires, id)?;
                        let sub_plan = compile(w, sub)?;
                        Val::Struct(decode_struct(m, w, &sub_plan, sub, wires)?)
                    }
                };
                vals[*j] = Some(v);
            }
            None => return Err(Error::Gob("field-range")),
        }
        last = f as i64;
    }
    Ok(vals)
}

fn val<'v, 'a>(vals: &'v [Option<Val<'a>>], i: usize) -> Option<&'v Val<'a>> {
    vals.get(i)?.as_ref()
}

fn int_at(vals: &[Option<Val<'_>>], i: usize) -> i64 {
    match val(vals, i) {
        Some(Val::Int(x)) => *x,
        _ => 0,
    }
}

fn point_at(vals: &[Option<Val<'_>>], i: usize) -> Point {
    match val(vals, i) {
        Some(Val::Struct(p)) => Point {
            x: int_at(p, 0),
            y: int_at(p, 1),
        },
        _ => Point::default(),
    }
}

/// `dec.Decode(&img)` for an `image.CMYK`: reads type definitions, then the first value, and
/// stores the fields it shares with `image.CMYK` by name. Trailing bytes are ignored, as Go's
/// decoder leaves them for the next `Decode`. The decoder allocates only the type definitions,
/// which are bounded by the input, and borrows `Pix` from it.
///
/// # Errors
/// [`Error::Sentinel`] (`Eof`) for an empty input, [`Error::UnexpectedEof`] for one cut short,
/// and [`Error::Gob`] for what Go reports as a gob error: `bad-uint`, `bad-count`, `bad-length`,
/// `short-data`, `negative-delta`, `field-range`, `duplicate-type`, `extra-data`, `bad-type`,
/// `type-mismatch`, `wrong-type`, `empty-field-name`, `nesting-too-deep`, `undefined-type` and
/// `unsupported-type`.
pub(crate) fn decode_cmyk(data: &[u8]) -> Result<CmykImage<'_>, Error> {
    let mut top = Reader { b: data, pos: 0 };
    let mut wires: BTreeMap<i64, Wire> = BTreeMap::new();
    let mut first = true;
    loop {
        let Some(msg) = next_message(&mut top)? else {
            // Only an empty input is a clean end; after type definitions a value must follow.
            return Err(if first {
                Error::Sentinel(Sentinel::Eof)
            } else {
                Error::UnexpectedEof
            });
        };
        let mut m = Reader { b: msg, pos: 0 };
        // `typeId` is an `int32`.
        let id = i64::from(m.first_int()? as i32);
        if id >= 0 {
            return decode_value(&mut m, id, &wires);
        }
        let id = -id;
        if id < i64::from(FIRST_USER_ID) || wires.contains_key(&id) {
            return Err(Error::Gob("duplicate-type"));
        }
        let w = read_wire_type(&mut m)?;
        if m.remaining() > 0 {
            return Err(Error::Gob("extra-data"));
        }
        wires.insert(id, w);
        first = false;
    }
}

/// `decodeValue` of the value of remote type `id` into an `image.CMYK`.
fn decode_value<'a>(
    m: &mut Reader<'a>,
    id: i64,
    wires: &BTreeMap<i64, Wire>,
) -> Result<CmykImage<'a>, Error> {
    let remote = wire_of(wires, id)?;
    let plan = compile(remote, CMYK)?;
    let matched = plan.iter().filter(|p| matches!(p, Plan::Local(_))).count();
    if matched == 0 && !remote.is_empty() {
        return Err(Error::Gob("type-mismatch"));
    }
    let vals = decode_struct(m, remote, &plan, CMYK, wires)?;
    let pix = match val(&vals, 0) {
        Some(Val::Bytes(b)) => *b,
        _ => &[],
    };
    let rect = match val(&vals, 2) {
        Some(Val::Struct(r)) => Rectangle {
            min: point_at(r, 0),
            max: point_at(r, 1),
        },
        _ => Rectangle::default(),
    };
    Ok(CmykImage {
        pix,
        stride: int_at(&vals, 1),
        rect,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg::{Cmyk, Gray, Rgba, YCbCr};

    fn all(msgs: &[Message<'_>]) -> Vec<u8> {
        msgs.iter().flat_map(|m| m.to_vec().unwrap()).collect()
    }

    fn gray_2x1() -> GoImage {
        GoImage::Gray(Gray {
            pix: vec![10, 20],
            stride: 2,
            rect: Rect::new(0, 0, 2, 1),
        })
    }

    #[test]
    fn uint_and_int_encodings() {
        let enc = |f: fn(&mut Vec<u8>, i64), x: i64| {
            let mut v = Vec::new();
            f(&mut v, x);
            v
        };
        let u = |v: &mut Vec<u8>, x: i64| put_uint(v, x as u64);
        assert_eq!(enc(u, 0), [0]);
        assert_eq!(enc(u, 127), [0x7F]);
        assert_eq!(enc(u, 128), [0xFF, 0x80]);
        assert_eq!(enc(u, 256), [0xFE, 0x01, 0x00]);
        assert_eq!(enc(u, 1 << 56), [0xF8, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(enc(put_int, 0), [0]);
        assert_eq!(enc(put_int, 22), [0x2C]);
        assert_eq!(enc(put_int, 33), [0x42]);
        assert_eq!(enc(put_int, -1), [0x01]);
        assert_eq!(enc(put_int, -64), [0x7F]);
        assert_eq!(enc(put_int, -65), [0xFF, 0x81]);
        assert_eq!(enc(put_int, 65), [0xFF, 0x82]);
        assert_eq!(
            enc(put_int, i64::MIN),
            [0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn encoding_doc_example() {
        // The bytes in encoding/gob/doc.go for `type Point struct{ X, Y int }` and
        // `Point{22, 33}`, with the type id 65 the document gives.
        let def = def_message("Point", 65, &[("X", T_INT), ("Y", T_INT)]).unwrap();
        assert_eq!(
            def.to_vec().unwrap(),
            [
                0x1F, 0xFF, 0x81, 0x03, 0x01, 0x01, 0x05, 0x50, 0x6F, 0x69, 0x6E, 0x74, 0x01, 0xFF,
                0x82, 0x00, 0x01, 0x02, 0x01, 0x01, 0x58, 0x01, 0x04, 0x00, 0x01, 0x01, 0x59, 0x01,
                0x04, 0x00, 0x00, 0x00
            ]
        );
        let mut b = Body::new(65);
        b.int(0, 22);
        b.int(1, 33);
        let value = message(b.finish()).unwrap();
        assert_eq!(
            value.to_vec().unwrap(),
            [0x07, 0xFF, 0x82, 0x01, 0x2C, 0x01, 0x42, 0x00]
        );
        assert_eq!(value.len(), 8);
    }

    #[test]
    fn gray_messages() {
        let img = gray_2x1();
        let msgs = encode_image_with(&img, &mut TypeIds::new()).unwrap();
        assert_eq!(msgs.len(), 4);
        // Gray is 64, Rectangle 65, Point 66.
        let gray = msgs[0].to_vec().unwrap();
        assert_eq!(&gray[..4], &[0x2E, 0x7F, 0x03, 0x01]);
        assert_eq!(msgs[0].len(), 0x2E + 1);
        let rect = msgs[1].to_vec().unwrap();
        // Rectangle: -65, then Min and Max of type 66.
        assert_eq!(&rect[1..4], &[0xFF, 0x81, 0x03]);
        let point = msgs[2].to_vec().unwrap();
        assert_eq!(&point[1..4], &[0xFF, 0x83, 0x03]);
        // Pix 0a 14, Stride 2, Rect (0,0)-(2,1).
        assert_eq!(
            msgs[3].to_vec().unwrap(),
            [
                0x13, 0xFF, 0x80, 0x01, 0x02, 0x0A, 0x14, 0x01, 0x04, 0x01, 0x01, 0x00, 0x01, 0x01,
                0x04, 0x01, 0x02, 0x00, 0x00, 0x00
            ]
        );
        // The planes are not copied.
        assert!(matches!(msgs[3].parts()[2], Cow::Borrowed(_)));
    }

    #[test]
    fn type_ids_follow_first_use() {
        let mut ids = TypeIds::new();
        let gray = gray_2x1();
        let cmyk = GoImage::Cmyk(Cmyk {
            pix: vec![1, 2, 3, 4],
            stride: 4,
            rect: Rect::new(0, 0, 1, 1),
        });
        encode_image_with(&gray, &mut ids).unwrap();
        assert_eq!(ids.top[Top::Gray as usize], 64);
        assert_eq!((ids.rectangle, ids.point), (65, 66));
        // CMYK is new (67); Rectangle and Point keep their ids.
        let m = encode_image_with(&cmyk, &mut ids).unwrap();
        assert_eq!(ids.top[Top::Cmyk as usize], 67);
        assert_eq!((ids.rectangle, ids.point), (65, 66));
        let def = m[0].to_vec().unwrap();
        // -67 is 0xFF 0x85, and the type's own id 67 is 0xFF 0x86.
        assert_eq!(&def[1..3], &[0xFF, 0x85]);
        // The same image again is the same bytes.
        let again = encode_image_with(&cmyk, &mut ids).unwrap();
        assert_eq!(all(&m), all(&again));
        // A fresh registry numbers CMYK first.
        let fresh = encode_image_with(&cmyk, &mut TypeIds::new()).unwrap();
        assert_eq!(&fresh[0].to_vec().unwrap()[1..2], &[0x7F]);
    }

    #[test]
    fn zero_fields_are_left_out_and_nested_structs_are_not() {
        // A CMYK image with an empty Pix, a zero Stride and an all-zero Rect.
        let img = GoImage::Cmyk(Cmyk {
            pix: Vec::new(),
            stride: 0,
            rect: Rect::new(0, 0, 0, 0),
        });
        let msgs = encode_image_with(&img, &mut TypeIds::new()).unwrap();
        // 64; Rect (delta 3 from -1): Min (delta 1, empty point), Max (delta 1, empty point), end.
        assert_eq!(
            msgs[3].to_vec().unwrap(),
            [0x09, 0xFF, 0x80, 0x03, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn ycbcr_messages() {
        let img = GoImage::YCbCr(YCbCr {
            y: vec![1; 4],
            cb: vec![2],
            cr: vec![3],
            y_stride: 2,
            c_stride: 1,
            ratio: Subsample::S420,
            rect: Rect::new(0, 0, 2, 2),
        });
        let msgs = encode_image_with(&img, &mut TypeIds::new()).unwrap();
        // 64, Y (0), Cb (1), Cr (2), YStride 2 (3), CStride 1 (4), ratio 2 (5), Rect (6).
        assert_eq!(
            msgs[3].to_vec().unwrap(),
            [
                0x1F, 0xFF, 0x80, 0x01, 0x04, 1, 1, 1, 1, 0x01, 0x01, 0x02, 0x01, 0x01, 0x03, 0x01,
                0x04, 0x01, 0x02, 0x01, 0x04, 0x01, 0x01, 0x00, 0x01, 0x01, 0x04, 0x01, 0x04, 0x00,
                0x00, 0x00
            ]
        );
        // The type definition lists the seven fields.
        let def = msgs[0].to_vec().unwrap();
        assert_eq!(usize::from(def[0]) + 1, msgs[0].len());
        assert!(def.windows(14).any(|w| w == b"SubsampleRatio"));
    }

    #[test]
    fn messages_are_written_one_per_write() {
        let img = gray_2x1();
        let msgs = encode_image_with(&img, &mut TypeIds::new()).unwrap();
        let lens: Vec<usize> = msgs.iter().map(Message::len).collect();
        assert_eq!(lens, [47, 42, 32, 20]);
        assert_eq!(all(&msgs).len(), lens.iter().sum::<usize>());
    }

    #[test]
    fn round_trip_into_cmyk() {
        let cmyk = GoImage::Cmyk(Cmyk {
            pix: (0u8..8).collect(),
            stride: 8,
            rect: Rect::new(0, 0, 2, 1),
        });
        let bytes = all(&encode_image_with(&cmyk, &mut TypeIds::new()).unwrap());
        let img = decode_cmyk(&bytes).unwrap();
        assert_eq!(img.pix, &[0, 1, 2, 3, 4, 5, 6, 7][..]);
        assert_eq!(img.stride, 8);
        assert_eq!(
            img.rect,
            Rectangle {
                min: Point { x: 0, y: 0 },
                max: Point { x: 2, y: 1 }
            }
        );
        // The process-wide registry gives bytes that decode the same way.
        let bytes = all(&encode_image(&cmyk).unwrap());
        assert_eq!(decode_cmyk(&bytes).unwrap(), img);
        // Gray and RGBA have the same fields by name.
        let gray = all(&encode_image_with(&gray_2x1(), &mut TypeIds::new()).unwrap());
        let g = decode_cmyk(&gray).unwrap();
        assert_eq!((g.pix, g.stride), (&[10u8, 20][..], 2));
        let rgba = GoImage::Rgba(Rgba {
            pix: vec![9; 4],
            stride: 4,
            rect: Rect::new(-1, -2, 0, -1),
        });
        let rgba_bytes = all(&encode_image_with(&rgba, &mut TypeIds::new()).unwrap());
        let r = decode_cmyk(&rgba_bytes).unwrap();
        assert_eq!((r.pix.len(), r.stride), (4, 4));
        assert_eq!(r.rect.min, Point { x: -1, y: -2 });
        assert_eq!(r.rect.max, Point { x: 0, y: -1 });
    }

    #[test]
    fn ycbcr_decodes_into_cmyk_with_only_the_rect() {
        let img = GoImage::YCbCr(YCbCr {
            y: vec![1; 4],
            cb: vec![2],
            cr: vec![3],
            y_stride: 2,
            c_stride: 1,
            ratio: Subsample::S420,
            rect: Rect::new(0, 0, 2, 2),
        });
        let bytes = all(&encode_image_with(&img, &mut TypeIds::new()).unwrap());
        let c = decode_cmyk(&bytes).unwrap();
        // Y, Cb, Cr, YStride, CStride and SubsampleRatio match nothing in image.CMYK.
        assert!(c.pix.is_empty());
        assert_eq!(c.stride, 0);
        assert_eq!(c.rect.max, Point { x: 2, y: 2 });
    }

    #[test]
    fn decode_errors() {
        // badDataTests: empty input is EOF, a count with too little data is unexpected EOF.
        assert_eq!(
            decode_cmyk(&[]).unwrap_err(),
            Error::Sentinel(Sentinel::Eof)
        );
        assert_eq!(
            decode_cmyk(&[0x7F, 0x68, 0x69]).unwrap_err(),
            Error::UnexpectedEof
        );
        // An unknown type id.
        let mut b = Body::new(70);
        b.int(0, 1);
        let value = message(b.finish()).unwrap().to_vec().unwrap();
        assert_eq!(decode_cmyk(&value).unwrap_err(), Error::Gob("bad-type"));
        // A type definition with no value after it.
        let img = gray_2x1();
        let msgs = encode_image_with(&img, &mut TypeIds::new()).unwrap();
        let defs: Vec<u8> = all(&msgs[..3]);
        assert_eq!(decode_cmyk(&defs).unwrap_err(), Error::UnexpectedEof);
        // The same type twice.
        let mut twice = msgs[0].to_vec().unwrap();
        twice.extend(msgs[0].to_vec().unwrap());
        assert_eq!(
            decode_cmyk(&twice).unwrap_err(),
            Error::Gob("duplicate-type")
        );
        // A field of the wrong type: Pix defined as an int.
        let bad = def_message("CMYK", 64, &[("Pix", T_INT)]).unwrap();
        let mut b = Body::new(64);
        b.int(0, 1);
        let mut stream = bad.to_vec().unwrap();
        stream.extend(message(b.finish()).unwrap().to_vec().unwrap());
        assert_eq!(decode_cmyk(&stream).unwrap_err(), Error::Gob("wrong-type"));
        // No field in common with image.CMYK.
        let none = def_message("T", 64, &[("A", T_INT)]).unwrap();
        let mut b = Body::new(64);
        b.int(0, 1);
        let mut stream = none.to_vec().unwrap();
        stream.extend(message(b.finish()).unwrap().to_vec().unwrap());
        assert_eq!(
            decode_cmyk(&stream).unwrap_err(),
            Error::Gob("type-mismatch")
        );
        // A field number past the type's fields.
        let one = def_message("T", 64, &[("Stride", T_INT)]).unwrap();
        let mut stream = one.to_vec().unwrap();
        stream.extend_from_slice(&[0x04, 0xFF, 0x80, 0x03, 0x04]);
        assert_eq!(decode_cmyk(&stream).unwrap_err(), Error::Gob("field-range"));
        // A bad unsigned integer: a 9-byte count.
        assert_eq!(decode_cmyk(&[0xF7]).unwrap_err(), Error::Gob("bad-uint"));
        // A length over tooBig.
        assert_eq!(
            decode_cmyk(&[0xF8, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap_err(),
            Error::Gob("bad-count")
        );
    }

    #[test]
    fn unknown_remote_fields_are_skipped() {
        // A struct with an extra string field before Stride and Pix: both are still found.
        let def =
            def_message("T", 64, &[("Name", 6), ("Stride", T_INT), ("Pix", T_BYTES)]).unwrap();
        let mut b = Body::new(64);
        // Name: "ab".
        b.field(0);
        put_uint(&mut b.cur, 2);
        b.cur.extend_from_slice(b"ab");
        b.int(1, 7);
        b.bytes(2, &[5, 6]);
        let mut stream = def.to_vec().unwrap();
        stream.extend(message(b.finish()).unwrap().to_vec().unwrap());
        let img = decode_cmyk(&stream).unwrap();
        assert_eq!((img.stride, img.pix), (7, &[5u8, 6][..]));
    }
}
