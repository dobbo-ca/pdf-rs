//! Object writing (Go: pkg/pdfcpu/writeObjects.go; the annotation-aware parts of
//! writeAnnotation.go: `writingAnnotationEntry`, `checkWrittenAnnotation` and
//! `writeAnnotationAwareEntry`).
//!
//! Go's `writeDeepObject` writes an object, then recurses into everything it refers to, depth
//! first. A chain of objects as long as the object table would overflow the stack of a thread, so
//! the recursion is a loop over an explicit stack of [`Work`] items here. The items are pushed in
//! the order that makes the pops run Go's calls in Go's order, including the state that Go sets
//! around a call (`ctx.Dest`, `ctx.WritingAnnotation`) and restores after it.
//!
//! Differences from Go:
//!
//! * Go changes the dictionaries and arrays of the table in place (it encrypts their strings and
//!   the page tree writer swaps `/Kids` and `/Count`). Here every writer works on a copy; the
//!   table keeps the plain objects.
//! * A dictionary's entries are visited in key order (the determinism patch of the oracle sorts
//!   Go's map loops at writeObjects.go:641, :735 and :769).
//! * `types.LazyObjectStreamObject` never reaches the writer: a dereference decodes it.
//! * Go's `writeFlatObject` and `WriteIncrement` are not ported (docs/scope.md section 6).
//! * The stats counters and the signature offsets that Go records (`BinaryTotalSize`,
//!   `OffsetSigByteRange`, `OffsetSigContents`) are not kept: nothing reads them.

use super::{WriteContext, check, reserve_charged, undelete_object};
use crate::crypto::{encrypt_deep_object, encrypt_stream, stream_crypt_method, strings_encrypted};
use crate::error::Error;
use crate::ledger::Held;
use crate::model::deref::type_name;
use crate::model::{Context, Entry, EntryObject};
use crate::object::write::{append_pdf_object, dict_text_bound, pdf_text_bound, text_buffer};
use crate::object::{Dict, Object, Ref, StreamDict, dict_copy_bytes};
use std::collections::btree_map;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

/// `ObjectStreamMaxObjects`: the most objects within an object stream written.
pub const OBJECT_STREAM_MAX_OBJECTS: i64 = 100;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A nil pointer that Go dereferences.
fn nil(what: &'static str) -> Error {
    Error::Panic(what)
}

/// Runs `f` on the dictionary `d` as an [`Object::Dict`] and puts the (changed) dictionary back.
fn with_object<R>(d: &mut Dict, f: impl FnOnce(&mut Object) -> R) -> R {
    let mut o = Object::Dict(std::mem::take(d));
    let r = f(&mut o);
    if let Object::Dict(back) = o {
        *d = back;
    }
    r
}

// -------------------------------------------------------------------------------------------------
// Headers, trailers

fn write_comment_line(w: &mut WriteContext, comment: &[u8]) -> Result<usize, Error> {
    let mut line = Vec::with_capacity(comment.len() + 3);
    line.push(b'%');
    line.extend_from_slice(comment);
    line.extend_from_slice(w.eol.as_bytes());
    w.write(&line)
}

/// `writeHeader`: `%PDF-<version>` and the line of four high bytes that marks the file as binary.
pub(super) fn write_header(w: &mut WriteContext, version: &str) -> Result<(), Error> {
    let i = write_comment_line(w, format!("PDF-{version}").as_bytes())?;
    let j = write_comment_line(w, b"\xe2\xe3\xcf\xd3")?;
    w.advance(i + j);
    Ok(())
}

/// `writeTrailer`: the end-of-file marker.
pub(super) fn write_trailer(w: &mut WriteContext) -> Result<(), Error> {
    let s = format!("%%EOF{}", w.eol);
    w.write(s.as_bytes())?;
    Ok(())
}

/// `objectHeader`.
fn object_header(obj_nr: i64, gen_nr: i64, eol: &str) -> String {
    format!("{obj_nr} {gen_nr} obj{eol}")
}

// -------------------------------------------------------------------------------------------------
// Object streams

/// `startObjectStream`: a new object stream gets a new object number (7.5.7).
fn start_object_stream(ctx: &mut Context, w: &mut WriteContext) -> Result<(), Error> {
    let entry = Entry {
        generation: Some(0),
        object: Some(EntryObject::ObjStm(Box::default())),
        ref_count: 1,
        ..Entry::default()
    };
    let obj_nr = ctx.xref.insert_new(entry)?;
    w.current_obj_stream = Some(obj_nr);
    Ok(())
}

/// `stopObjectStream`: writes the current object stream, if there is one, and stops writing to
/// object streams.
///
/// # Errors
/// [`Error::Corrupt`] if not writing to an object stream; the errors of the ledger, of
/// `StreamDict::encode` and of [`write_stream_dict_object`].
pub(super) fn stop_object_stream(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !w.write_to_object_stream {
        return Err(Error::Corrupt("not writing to object stream".into()));
    }
    let Some(cur) = w.current_obj_stream else {
        w.write_to_object_stream = false;
        return Ok(());
    };

    let mem = ctx.xref.mem().clone();
    let mut osd = match ctx.xref.table.get_mut(&cur).and_then(|e| e.object.take()) {
        Some(EntryObject::ObjStm(b)) => *b,
        _ => return Err(nil("nil object stream entry")),
    };

    // When we are ready to write: append prolog and content.
    osd.finalize(&mem)?;
    w.obj_stream_held = None;
    // Encode content -> raw, and drop the decoded content to free up memory.
    osd.stream.encode(&mem, cancel)?;
    osd.stream.content = None;
    let (first, n) = (osd.first_obj_offset, osd.obj_count);
    osd.stream.dict.insert_int(b"First", first);
    osd.stream.dict.insert_int(b"N", n);

    let sd = std::mem::take(&mut osd.stream);
    write_stream_dict_object(ctx, w, cur, 0, sd, cancel)?;

    // The table keeps the object stream without what it held.
    osd.prolog = Vec::new();
    if let Some(e) = ctx.xref.table.get_mut(&cur) {
        e.object = Some(EntryObject::ObjStm(Box::new(osd)));
    }
    w.current_obj_stream = None;
    w.write_to_object_stream = false;
    Ok(())
}

/// `writeToObjectStream`: puts `obj`, the object `obj_nr`, into the current object stream if the
/// writer is compressing, starting a stream if there is none. Returns whether it did.
fn write_to_object_stream(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    obj: &Object,
    cancel: &AtomicBool,
) -> Result<bool, Error> {
    // Object streams assume an xref stream, the signal for compression is on, an object stream is
    // being written, and the generation is 0.
    let compress =
        w.write_xref_stream && w.write_object_stream && w.write_to_object_stream && gen_nr == 0;
    if !compress {
        return Ok(false);
    }

    if w.current_obj_stream.is_none() {
        // Create the new object stream on the first write.
        start_object_stream(ctx, w)?;
    }
    let cur = w
        .current_obj_stream
        .ok_or_else(|| nil("nil object stream number"))?;

    // The next free index in the object stream.
    let index = match ctx.xref.table.get(&cur) {
        Some(Entry {
            object: Some(EntryObject::ObjStm(osd)),
            ..
        }) => osd.obj_count,
        _ => return Err(nil("nil object stream entry")),
    };

    // Turn the entry of the object into a compressed entry at that index.
    let Some(entry) = ctx.xref.table.get_mut(&obj_nr) else {
        return Err(nil("nil xref entry"));
    };
    entry.compressed = true;
    entry.object_stream = Some(cur);
    entry.object_stream_ind = Some(index);
    // For a compressed object this is a fake offset: its value does not matter.
    w.set_write_offset(obj_nr);

    let mem = ctx.xref.mem().clone();
    let (mut text, _text_held) = text_buffer(pdf_text_bound(obj), &mem)?;
    append_pdf_object(&mut text, obj)?;
    let count = match ctx.xref.table.get_mut(&cur) {
        Some(Entry {
            object: Some(EntryObject::ObjStm(osd)),
            ..
        }) => {
            reserve_charged(&mut osd.body, &mut w.obj_stream_held, &mem, text.len())?;
            osd.add_object(obj_nr, &text);
            osd.obj_count
        }
        _ => return Err(nil("nil object stream entry")),
    };

    if count == OBJECT_STREAM_MAX_OBJECTS {
        stop_object_stream(ctx, w, cancel)?;
        w.write_to_object_stream = true;
    }
    Ok(true)
}

// -------------------------------------------------------------------------------------------------
// Plain objects

/// `writeObject`: `<nr> <gen> obj`, the text `s` and `endobj`, and the offset bookkeeping.
pub(super) fn write_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    s: &[u8],
) -> Result<(), Error> {
    // Cleanup the entry (necessary for split).
    if let Some(e) = ctx.xref.table.get_mut(&obj_nr) {
        e.compressed = false;
    }

    // Set the write offset of this object.
    w.set_write_offset(obj_nr);

    let header = object_header(obj_nr, gen_nr, w.eol);
    let written = w.write(header.as_bytes())?;
    // Lines that are not part of stream object data are limited to no more than 255 characters.
    let i = w.write(s)?;
    let trailer = format!("{}endobj{}", w.eol, w.eol);
    let j = w.write(trailer.as_bytes())?;

    // The write offset of the next object.
    w.advance(written + i + j);
    Ok(())
}

/// The encryption parameters that Go reads as `ctx.E.R`.
fn encryption_revision(ctx: &Context) -> Result<i64, Error> {
    ctx.xref
        .e
        .as_ref()
        .map(|e| e.r)
        .ok_or_else(|| nil("encryption parameters are not set"))
}

/// Encrypts the strings of `o`, a direct object tree, in place if strings are encrypted at all.
/// A string that outgrows the one it replaces stays charged to `grown`, the charge of `o`.
fn encrypt_strings(
    ctx: &mut Context,
    w: &WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    o: &mut Object,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !strings_encrypted(&mut ctx.xref)? {
        return Ok(());
    }
    let r = encryption_revision(ctx)?;
    encrypt_deep_object(
        o,
        obj_nr,
        gen_nr,
        &ctx.xref.enc_key,
        ctx.xref.aes4_strings,
        r,
        w.env.random,
        ctx.xref.mem(),
        grown,
        cancel,
    )
}

/// `writeBooleanObject`, `writeNameObject`, `writeStringLiteralObject`, `writeHexLiteralObject`,
/// `writeIntegerObject` and `writeFloatObject`. An integer is never compressed.
fn write_simple_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    mut o: Object,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !matches!(o, Object::Integer(_))
        && write_to_object_stream(ctx, w, obj_nr, gen_nr, &o, cancel)?
    {
        return Ok(());
    }
    if matches!(o, Object::String(_) | Object::Hex(_)) {
        encrypt_strings(ctx, w, obj_nr, gen_nr, &mut o, grown, cancel)?;
    }
    let (s, _text) = o.pdf_string_charged(ctx.xref.mem())?;
    write_object(ctx, w, obj_nr, gen_nr, &s)
}

/// `sigDict`: a signature dictionary, by its type or by its `/Contents` with a `/ByteRange`.
pub(super) fn sig_dict(d: &Dict) -> bool {
    if let Some(Object::Name(n)) = d.0.get(b"Type".as_slice())
        && n.as_slice() == b"Sig"
    {
        return true;
    }
    d.0.contains_key(b"Contents".as_slice()) && d.0.contains_key(b"ByteRange".as_slice())
}

/// The signer entries of a signature dictionary that `sigDictPDFString` copies, unless it is a
/// time stamp.
const SIGNER_ENTRIES: [&[u8]; 5] = [b"M", b"Name", b"ContactInfo", b"Location", b"Reason"];

/// `sigDictPDFString`: the text of a signature dictionary in the fixed layout that keeps
/// `/ByteRange` at a fixed width, so that it can be patched after signing.
pub(super) fn sig_dict_pdf_string(d: &Dict) -> Result<Vec<u8>, Error> {
    for (key, o) in d.iter() {
        if matches!(o, Object::Ref(_)) {
            return Err(Error::Corrupt(format!(
                "signature dict entry {}: value must be direct",
                lossy(key)
            )));
        }
    }

    // Go calls `PDFString` on the map value, which panics for a missing entry.
    let must = |key: &[u8]| -> Result<Vec<u8>, Error> {
        match d.0.get(key) {
            Some(o) if !o.is_null() => Ok(o.pdf_string()),
            _ => Err(nil("nil pointer dereference")),
        }
    };

    // Worst case [0 0000000000 0000000000 0000000000].
    let mut byte_range = must(b"ByteRange")?;
    while byte_range.len() < 36 {
        byte_range.push(b' ');
    }
    let mut s = b"<<".to_vec();
    s.extend_from_slice(b"/ByteRange");
    s.extend_from_slice(&byte_range);
    s.extend_from_slice(b"/Contents");
    s.extend_from_slice(&must(b"Contents")?);
    s.extend_from_slice(b"/Type");
    s.extend_from_slice(&must(b"Type")?);
    s.extend_from_slice(b"/Filter");
    s.extend_from_slice(&must(b"Filter")?);
    s.extend_from_slice(b"/SubFilter");
    s.extend_from_slice(&must(b"SubFilter")?);

    let Some(sub_filter) = d.name_entry(b"SubFilter") else {
        return Err(Error::Corrupt(
            "signature dict: missing or invalid direct SubFilter".into(),
        ));
    };
    if sub_filter != b"ETSI.RFC3161" {
        for key in SIGNER_ENTRIES {
            if let Some(o) = d.0.get(key)
                && !o.is_null()
            {
                s.push(b'/');
                s.extend_from_slice(key);
                s.extend_from_slice(&o.pdf_string());
            }
        }
    }
    s.extend_from_slice(b">>");
    Ok(s)
}

/// `writeDictObject` and `writeArrayObject`: `o` is a dictionary or an array. Its strings are
/// encrypted in place if the document is encrypted (an object in an object stream is not: the
/// stream is).
fn write_container_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    o: &mut Object,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if write_to_object_stream(ctx, w, obj_nr, gen_nr, o, cancel)? {
        return Ok(());
    }
    encrypt_strings(ctx, w, obj_nr, gen_nr, o, grown, cancel)?;

    let (s, _text) = if let Object::Dict(d) = &*o
        && sig_dict(d)
    {
        // The text grows by doubling (up to twice its bound, the `/ByteRange` padding included)
        // while the text of one entry is live beside it.
        let held = ctx
            .xref
            .mem()
            .charge(dict_text_bound(d).saturating_add(36).saturating_mul(3))?;
        (sig_dict_pdf_string(d)?, held)
    } else {
        o.pdf_string_charged(ctx.xref.mem())?
    };
    write_object(ctx, w, obj_nr, gen_nr, &s)
}

/// `writeDictObject`: `d` is changed by the encryption of its strings.
pub(super) fn write_dict_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    d: &mut Dict,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let mut grown = ctx.xref.mem().hold();
    with_object(d, |o| {
        write_container_object(ctx, w, obj_nr, gen_nr, o, &mut grown, cancel)
    })
}

// -------------------------------------------------------------------------------------------------
// Streams

/// `writeStream`: `stream`, the raw bytes and `endstream`. Returns the bytes written, which are
/// those of the markers and of the stream.
fn write_stream(w: &mut WriteContext, sd: &StreamDict) -> Result<i64, Error> {
    let fail = |e: Error| e.context("failed to write raw content");
    let eol = w.eol;
    let b = w
        .write(format!("{eol}stream{eol}").as_bytes())
        .map_err(fail)?;
    let raw: &[u8] = sd.raw.as_deref().unwrap_or(&[]);
    let c = w.write(raw).map_err(fail)?;
    let length = sd.stream_length.ok_or_else(|| nil("nil stream length"))?;
    if i64::try_from(c).ok() != Some(length) {
        return Err(Error::Corrupt(format!(
            "failed to write raw content: {c} bytes written - streamlength:{length}"
        )));
    }
    let e = w
        .write(format!("{eol}endstream").as_bytes())
        .map_err(fail)?;
    Ok(i64::try_from(b + e)
        .unwrap_or(i64::MAX)
        .saturating_add(length))
}

/// `handleIndirectLength`: writes the object that a stream's `/Length` refers to, unless it is
/// written already.
fn handle_indirect_length(
    ctx: &mut Context,
    w: &mut WriteContext,
    ir: Ref,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if w.has_write_offset(ir.num) {
        return Ok(());
    }
    if let Some(length) = ctx.xref.dereference_integer(&Object::Ref(ir))? {
        let mut grown = ctx.xref.mem().hold();
        write_simple_object(
            ctx,
            w,
            ir.num,
            ir.generation,
            Object::Integer(length),
            &mut grown,
            cancel,
        )?;
    }
    Ok(())
}

/// `writeStreamDictObject`: the stream object `obj_nr`, encrypted if the document is. `sd` is the
/// writer's own copy; the table keeps the stream it came from.
///
/// # Errors
/// [`Error::Corrupt`] if the raw bytes are not as long as the stream says; the errors of the
/// encryption; [`Error::Panic`] where Go dereferences a nil `/Length`.
pub(super) fn write_stream_dict_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
    mut sd: StreamDict,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    // A stream is written outside of an object stream.
    let in_obj_stream = w.write_to_object_stream;
    if in_obj_stream {
        w.write_to_object_stream = false;
    }

    // Sometimes a stream dictionary's length is a reference.
    if let Some(ir) = sd.dict.indirect_ref_entry(b"Length") {
        handle_indirect_length(ctx, w, ir, cancel)?;
    }

    // Use the stream's explicit crypt filter, if any, rather than the default stream filter.
    let is_xref_stream_dict = sd.dict.type_() == Some(b"XRef".as_slice());
    if !ctx.xref.enc_key.is_empty() && !is_xref_stream_dict {
        let (encrypted, aes) = stream_crypt_method(&mut ctx.xref, &sd)?;
        if encrypted {
            let r = encryption_revision(ctx)?;
            let raw: &[u8] = sd.raw.as_deref().unwrap_or(&[]);
            let enc = encrypt_stream(
                raw,
                obj_nr,
                gen_nr,
                &ctx.xref.enc_key,
                aes,
                r,
                w.env.random,
                ctx.xref.mem(),
            )?;
            let l = i64::try_from(enc.len()).unwrap_or(i64::MAX);
            sd.raw = Some(enc);
            sd.stream_length = Some(l);
            sd.dict.update(b"Length", Object::Integer(l));
        }
    }

    w.set_write_offset(obj_nr);

    let (pdf_string, _text) = sd.dict.pdf_string_charged(ctx.xref.mem())?;

    let eol = w.eol;
    let h = w.write(object_header(obj_nr, gen_nr, eol).as_bytes())?;
    // Lines that are not part of stream object data are limited to no more than 255 characters.
    w.write(&pdf_string)?;
    let b = write_stream(w, &sd)?;
    let t = w.write(format!("{eol}endobj{eol}").as_bytes())?;

    let written = b.saturating_add(i64::try_from(h + pdf_string.len() + t).unwrap_or(i64::MAX));
    w.offset = w.offset.saturating_add(written);

    if in_obj_stream {
        w.write_to_object_stream = true;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// The walk

/// `writeNullObject`: an indirect reference to nil is a corner case; it is still an object that
/// is written.
fn write_null_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    obj_nr: i64,
    gen_nr: i64,
) -> Result<(), Error> {
    write_object(ctx, w, obj_nr, gen_nr, b"null")?;
    // Ensure no entry in the free list.
    undelete_object(&mut ctx.xref, obj_nr)
}

/// What `writingAnnotationEntry` and `checkWrittenAnnotation` read of a dictionary: its raw
/// `/Type` and `/Subtype`. Only kept for PDF 2.0.
struct Ann {
    typ: Option<Object>,
    subtype: Option<Object>,
}

/// The annotation context of a dictionary, for PDF 2.0.
fn ann_of(ctx: &Context, d: &Dict) -> Result<Option<Rc<Ann>>, Error> {
    if !ctx.xref.pdf20()? {
        return Ok(None);
    }
    Ok(Some(Rc::new(Ann {
        typ: d.0.get(b"Type".as_slice()).cloned(),
        subtype: d.0.get(b"Subtype".as_slice()).cloned(),
    })))
}

/// Whether `ctx.Dereference(o)` of a dictionary entry that may be missing (Go's nil) is the name
/// `name`.
fn deref_is_name(ctx: &mut Context, o: Option<&Object>, name: &[u8]) -> Result<bool, Error> {
    match o {
        None => Ok(false),
        Some(o) => Ok(is_name(ctx.xref.dereference(o)?, name)),
    }
}

fn is_name(o: &Object, name: &[u8]) -> bool {
    matches!(o, Object::Name(n) if n.as_slice() == name)
}

/// `writingAnnotationEntry`: whether the entry `key` of a dictionary leads to an annotation.
fn writing_annotation_entry(ctx: &mut Context, ann: &Ann, key: &[u8]) -> Result<bool, Error> {
    if key == b"Annots" {
        return deref_is_name(ctx, ann.typ.as_ref(), b"Page");
    }
    if !ctx.writing_annotation {
        return Ok(false);
    }
    if key == b"Parent" {
        return deref_is_name(ctx, ann.subtype.as_ref(), b"Popup");
    }
    Ok(key == b"Popup" || key == b"IRT")
}

/// `checkWrittenAnnotation` of what `o` dereferences to: `None` unless that is a dictionary, else
/// whether it is an annotation (or a widget), checking its appearance if it is. Only `/Type` and
/// `/Subtype` are copied, and an indirect dictionary only for the appearance check; each copy is
/// charged before it is made.
fn check_written_annotation(
    ctx: &mut Context,
    w: &WriteContext,
    o: &Object,
) -> Result<Option<bool>, Error> {
    let mem = ctx.xref.mem().clone();
    let writing = ctx.writing_annotation;
    let entries = match ctx.xref.dereference(o)? {
        Object::Dict(_) if writing => None,
        Object::Dict(d) => {
            let typ = d.0.get(b"Type".as_slice());
            let subtype = d.0.get(b"Subtype".as_slice());
            let held = mem.charge(
                typ.map_or(0, Object::copy_bytes) + subtype.map_or(0, Object::copy_bytes),
            )?;
            Some((typ.cloned(), subtype.cloned(), held))
        }
        _ => return Ok(None),
    };
    let annotation = match entries {
        None => true,
        Some((typ, subtype, _held)) => {
            // Type is optional on annotations; widgets can also be reached through the field tree.
            let annot = deref_is_name(ctx, typ.as_ref(), b"Annot")?;
            let widget = deref_is_name(ctx, subtype.as_ref(), b"Widget")?;
            annot || widget
        }
    };
    if annotation {
        let _copy;
        let owned;
        let d = match o {
            Object::Dict(d) => d,
            _ => match ctx.xref.dereference(o)? {
                Object::Dict(d) => {
                    _copy = mem.charge(dict_copy_bytes(d))?;
                    owned = d.clone();
                    &owned
                }
                _ => return Ok(Some(true)),
            },
        };
        (w.hooks.check_annotation_appearance)(&mut ctx.xref, d)?;
    }
    Ok(Some(annotation))
}

/// One step of the walk that Go's recursive writers make.
enum Work {
    /// `writeDeepObject`.
    Deep(Object),
    /// `writeDeepObjectUnchecked`.
    Unchecked(Object),
    /// `writeAnnotationAwareEntry`: the entry `key` with the value `obj`.
    AwareEntry {
        ann: Option<Rc<Ann>>,
        key: Vec<u8>,
        obj: Object,
    },
    /// The rest of a dictionary's entries (`writeDirectDict`, and the loop of `writeDeepDict`).
    /// `copy` is the charge of the writer's copy of an indirect object, until its rest drops.
    Entries {
        it: btree_map::IntoIter<Vec<u8>, Object>,
        ann: Option<Rc<Ann>>,
        copy: Option<Held>,
    },
    /// The rest of a stream dictionary's values (the loop of `writeDeepStreamDict`), and the
    /// charge of the copy.
    Values(btree_map::IntoIter<Vec<u8>, Object>, Held),
    /// The rest of an array's elements; `first` while the element at index 0 is still to come.
    Elems {
        it: std::vec::IntoIter<Object>,
        first: bool,
        copy: Option<Held>,
    },
    /// Go's deferred restore of `ctx.WritingAnnotation`.
    RestoreAnnotation(bool),
    /// Go's `ctx.Dest = false` after an entry.
    ClearDest,
}

/// The stack of the walk. Each item is charged to the ledger while it is on it.
struct Stack {
    v: Vec<Work>,
    held: Held,
}

impl Stack {
    fn push(&mut self, work: Work) -> Result<(), Error> {
        self.held.grow_by(std::mem::size_of::<Work>() as u64)?;
        self.v.try_reserve(1)?;
        self.v.push(work);
        Ok(())
    }

    fn pop(&mut self) -> Option<Work> {
        let work = self.v.pop()?;
        let left = self
            .held
            .bytes()
            .saturating_sub(std::mem::size_of::<Work>() as u64);
        self.held.shrink_to(left);
        Some(work)
    }
}

/// `writeIndirectObject`: writes the object `ir` refers to, unless it is written already, and
/// pushes what the walk does next.
fn write_indirect_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    ir: Ref,
    st: &mut Stack,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    if w.has_write_offset(ir.num) {
        return Ok(());
    }

    // Decode compressed objects so their references are traversed and their strings can be
    // encrypted.
    let mem = ctx.xref.mem().clone();
    let r = Object::Ref(ir);
    let o = ctx
        .xref
        .deref_c(&r, cancel)
        .map_err(|e| e.context(format!("unable to dereference indirect object #{}", ir.num)))?;
    let copy = mem.charge(o.copy_bytes())?;
    let o = o.clone();
    write_object_generic(ctx, w, o, ir.num, ir.generation, st, copy, cancel)
}

/// `writeObjectGeneric`. `copy` is the charge of `o`, the writer's copy.
fn write_object_generic(
    ctx: &mut Context,
    w: &mut WriteContext,
    o: Object,
    obj_nr: i64,
    gen_nr: i64,
    st: &mut Stack,
    mut copy: Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    match o {
        Object::Null => write_null_object(ctx, w, obj_nr, gen_nr),
        Object::Dict(d) => write_deep_dict(ctx, w, d, obj_nr, gen_nr, st, copy, cancel),
        Object::Stream(sd) => write_deep_stream_dict(ctx, w, *sd, obj_nr, gen_nr, st, copy, cancel),
        Object::Array(a) => write_deep_array(ctx, w, a, obj_nr, gen_nr, st, copy, cancel),
        Object::Ref(_) => Err(Error::Corrupt(format!(
            "writeIndirectObject: undefined PDF object #{obj_nr} {}",
            type_name(&o)
        ))),
        simple => write_simple_object(ctx, w, obj_nr, gen_nr, simple, &mut copy, cancel),
    }
}

/// `writeDeepDict`: writes the dictionary, then pushes its entries.
fn write_deep_dict(
    ctx: &mut Context,
    w: &mut WriteContext,
    mut d: Dict,
    obj_nr: i64,
    gen_nr: i64,
    st: &mut Stack,
    mut copy: Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let (dict_type, _) = ctx
        .xref
        .dereference_name_entry(&d, b"Type")
        .map_err(|e| e.context(format!("write dict obj#{obj_nr} gen#{gen_nr} Type")))?;
    if dict_type.as_deref() == Some(b"Page".as_slice()) {
        let valid = ctx.xref.is_obj_valid(obj_nr, gen_nr).map_err(|e| {
            e.context(format!(
                "write page dict obj#{obj_nr} gen#{gen_nr}: check valid"
            ))
        })?;
        if !valid {
            return Ok(());
        }
    }

    with_object(&mut d, |o| {
        write_container_object(ctx, w, obj_nr, gen_nr, o, &mut copy, cancel)
    })?;

    // Go encrypts the table's own map, so `writeEncryptDict` writes the encryption dictionary
    // again with the strings encrypted here.
    if ctx.xref.encrypt.is_some_and(|e| e.num == obj_nr) {
        // The clone is live beside the table's dictionary until that drops.
        let _copy = ctx.xref.mem().charge(dict_copy_bytes(&d))?;
        if let Some(Entry {
            object: Some(EntryObject::Obj(o)),
            ..
        }) = ctx.xref.table.get_mut(&obj_nr)
        {
            *o = Object::Dict(d.clone());
        }
    }

    let ann = ann_of(ctx, &d)?;
    st.push(Work::Entries {
        it: d.0.into_iter(),
        ann,
        copy: Some(copy),
    })
}

/// `writeDeepStreamDict`: writes the stream, then pushes the values of its dictionary.
fn write_deep_stream_dict(
    ctx: &mut Context,
    w: &mut WriteContext,
    mut sd: StreamDict,
    obj_nr: i64,
    gen_nr: i64,
    st: &mut Stack,
    mut copy: Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    with_object(&mut sd.dict, |o| {
        encrypt_strings(ctx, w, obj_nr, gen_nr, o, &mut copy, cancel)
    })?;
    copy.grow_by(dict_copy_bytes(&sd.dict))?;
    let dict = sd.dict.clone();
    write_stream_dict_object(ctx, w, obj_nr, gen_nr, sd, cancel)?;
    st.push(Work::Values(dict.0.into_iter(), copy))
}

/// `writeDeepArray`: writes the array, then pushes its elements.
fn write_deep_array(
    ctx: &mut Context,
    w: &mut WriteContext,
    a: Vec<Object>,
    obj_nr: i64,
    gen_nr: i64,
    st: &mut Stack,
    mut copy: Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let mut o = Object::Array(a);
    write_container_object(ctx, w, obj_nr, gen_nr, &mut o, &mut copy, cancel)?;
    if let Object::Array(a) = o {
        st.push(Work::Elems {
            it: a.into_iter(),
            first: true,
            copy: Some(copy),
        })?;
    }
    Ok(())
}

/// `writeDeepObject`: in a PDF 2.0 file a dictionary that is an annotation is written in an
/// annotation context.
fn deep(
    ctx: &mut Context,
    w: &WriteContext,
    o: Object,
    st: &mut Stack,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    if ctx.xref.pdf20()?
        && let Some(annotation) = check_written_annotation(ctx, w, &o)?
    {
        let previous = ctx.writing_annotation;
        ctx.writing_annotation = annotation;
        st.push(Work::RestoreAnnotation(previous))?;
    }
    st.push(Work::Unchecked(o))
}

/// `writeDeepObjectUnchecked` and `writeDirectObject`.
fn unchecked(
    ctx: &mut Context,
    w: &mut WriteContext,
    o: Object,
    st: &mut Stack,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    match o {
        Object::Ref(ir) => write_indirect_object(ctx, w, ir, st, cancel),
        Object::Dict(d) => {
            let ann = ann_of(ctx, &d)?;
            st.push(Work::Entries {
                it: d.0.into_iter(),
                ann,
                copy: None,
            })
        }
        Object::Array(a) => st.push(Work::Elems {
            it: a.into_iter(),
            first: true,
            copy: None,
        }),
        // A direct object that is not a container: nothing to write.
        _ => Ok(()),
    }
}

/// `writeAnnotationAwareEntry`.
fn aware_entry(
    ctx: &mut Context,
    ann: Option<Rc<Ann>>,
    key: &[u8],
    obj: Object,
    st: &mut Stack,
) -> Result<(), Error> {
    if let Some(ann) = ann {
        let annotation = writing_annotation_entry(ctx, &ann, key)?;
        let previous = ctx.writing_annotation;
        ctx.writing_annotation = annotation;
        st.push(Work::RestoreAnnotation(previous))?;
    }
    st.push(Work::Deep(obj))
}

/// One pop of the walk.
fn step(
    ctx: &mut Context,
    w: &mut WriteContext,
    work: Work,
    st: &mut Stack,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    match work {
        Work::Deep(o) => deep(ctx, w, o, st, cancel),
        Work::Unchecked(o) => unchecked(ctx, w, o, st, cancel),
        Work::AwareEntry { ann, key, obj } => aware_entry(ctx, ann, &key, obj, st),
        Work::Entries { mut it, ann, copy } => {
            let Some((key, value)) = it.next() else {
                return Ok(());
            };
            check(cancel)?;
            st.push(Work::Entries {
                it,
                ann: ann.clone(),
                copy,
            })?;
            if ctx.writing_pages && matches!(key.as_slice(), b"Dest" | b"D") {
                ctx.dest = true;
            }
            st.push(Work::ClearDest)?;
            st.push(Work::AwareEntry {
                ann,
                key,
                obj: value,
            })
        }
        Work::Values(mut it, copy) => {
            let Some((_, value)) = it.next() else {
                return Ok(());
            };
            check(cancel)?;
            st.push(Work::Values(it, copy))?;
            st.push(Work::Deep(value))
        }
        Work::Elems {
            mut it,
            first,
            copy,
        } => {
            let Some(value) = it.next() else {
                return Ok(());
            };
            check(cancel)?;
            // The first element of a destination array is a page reference, which is not followed.
            let skip = ctx.dest && first;
            st.push(Work::Elems {
                it,
                first: false,
                copy,
            })?;
            if !skip {
                st.push(Work::Deep(value))?;
            }
            Ok(())
        }
        Work::RestoreAnnotation(previous) => {
            ctx.writing_annotation = previous;
            Ok(())
        }
        Work::ClearDest => {
            ctx.dest = false;
            Ok(())
        }
    }
}

/// Runs the walk that starts with `start` to its end.
fn run(
    ctx: &mut Context,
    w: &mut WriteContext,
    start: Work,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let held = ctx.xref.mem().charge(0)?;
    let mut st = Stack {
        v: Vec::new(),
        held,
    };
    st.push(start)?;
    while let Some(work) = st.pop() {
        if let Err(e) = step(ctx, w, work, &mut st, cancel) {
            // Go's deferred calls restore the annotation context on the way out.
            while let Some(rest) = st.pop() {
                if let Work::RestoreAnnotation(previous) = rest {
                    ctx.writing_annotation = previous;
                }
            }
            return Err(e);
        }
    }
    Ok(())
}

/// `writeDeepObject`: writes `o` and everything it refers to that is not written yet.
///
/// # Errors
/// The errors of the object writers, [`Error::Cancelled`].
pub(super) fn write_deep_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    o: Object,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    run(ctx, w, Work::Deep(o), cancel)
}

/// `writeDirectDict`: writes everything the entries of `d` refer to.
pub(super) fn write_direct_dict(
    ctx: &mut Context,
    w: &mut WriteContext,
    d: Dict,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let ann = ann_of(ctx, &d)?;
    run(
        ctx,
        w,
        Work::Entries {
            it: d.0.into_iter(),
            ann,
            copy: None,
        },
        cancel,
    )
}

/// `writeEntry`: writes what the entry `name` of `d` refers to. A missing or null entry is
/// nothing.
pub(super) fn write_entry(
    ctx: &mut Context,
    w: &mut WriteContext,
    d: &Dict,
    name: &[u8],
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let Some(o) = d.find(name).filter(|o| !o.is_null()) else {
        return Ok(());
    };
    let ann = ann_of(ctx, d)?;
    let _copy = ctx.xref.mem().charge(o.copy_bytes())?;
    run(
        ctx,
        w,
        Work::AwareEntry {
            ann,
            key: name.to_vec(),
            obj: o.clone(),
        },
        cancel,
    )
}
