//! The PDF writer (Go: pkg/pdfcpu/write.go, writeObjects.go, writePages.go; docs/design.md
//! section 15, work package 25). It writes a [`Context`] as a whole new PDF file into memory:
//! the header, the catalog and the page tree, the document information dictionary, the objects
//! they refer to (compressed into object streams where the configuration says so), the encryption
//! dictionary, and the cross-reference table or stream with the trailer.
//!
//! * [`write_context`] is Go's `WriteContext`. The caller makes a [`WriteContext`] for the output
//!   and takes the bytes with [`WriteContext::into_bytes`].
//! * `objects` writes objects and everything they refer to; `pages` writes the page tree.
//!
//! The parts of the writer that belong to work package 26 (the info dictionary, the PDF 2.0
//! checks and the core font metrics) are supplied through [`WriteHooks`].
//!
//! What this writer does not do, because byblos never reaches it (docs/scope.md section 6):
//! `WriteIncrement` and `writeFlatObject`, the selected-pages and page-removal modes of
//! `writeSelectedPage`, the reduced feature set of split and trim, building or changing an
//! encryption dictionary (`setupEncryption`, `updateEncryption`), stats and logging. Go's
//! `writeAdditionalStreams` writes nothing: it hands a `*types.Array` to `writeDeepObject`,
//! whose type switch only knows `types.Array`. That is kept.
//!
//! Output order and numbers follow Go's: a dictionary's entries are visited in key order (the
//! determinism patch of the parity oracle sorts Go's map loops), and new objects (object streams,
//! the xref stream, name tree nodes) take numbers from the free list and the table size exactly as
//! Go's inserts do.

mod corefont;
mod corefont_data;
mod objects;
mod pages;
mod pdf20;

pub use objects::OBJECT_STREAM_MAX_OBJECTS;
pub use pdf20::hooks;

use crate::env::WriteEnv;
use crate::error::Error;
use crate::filter::FLATE;
use crate::ledger::{Bytes, Held, Mem};
use crate::model::{Context, Entry, EntryObject, Version, XRefTable};
use crate::object::{Dict, Object, PdfFilter, StreamDict, XRefStreamDict, dict_copy_bytes};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// `contextutil.Check`.
pub(crate) fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// A nil pointer that Go dereferences.
fn nil(what: &'static str) -> Error {
    Error::Panic(what)
}

/// What the parts of the writer that belong to work package 26 supply.
#[derive(Debug, Clone, Copy)]
pub struct WriteHooks {
    /// `ensureInfoDict` (info.go:110): makes sure there is an info dictionary and updates its
    /// producer and dates from `env.now`. Called for files before PDF 2.0.
    pub ensure_info_dict: fn(&mut Context, &WriteEnv) -> Result<(), Error>,
    /// `prepareCoreFontsForWriting` (writeFont.go:77): adds explicit metrics to the standard
    /// fonts. Called for PDF 2.0 files.
    pub prepare_core_fonts: fn(&mut Context, &AtomicBool) -> Result<(), Error>,
    /// `XRefTable.CheckAnnotationAppearance` (model/annotation.go:368): called, for PDF 2.0
    /// files, with every annotation dictionary the writer meets.
    pub check_annotation_appearance: fn(&mut XRefTable, &Dict) -> Result<(), Error>,
}

/// The sets of Go's `OptimizationContext` that the writer reads and prunes: the objects that
/// optimize found to be duplicates of others. A document that was read has one (possibly empty);
/// a document that was created has none.
#[derive(Debug, Clone, Default)]
pub struct RedundantObjs {
    /// `DuplicateFontObjs`.
    pub duplicate_font_objs: BTreeSet<i64>,
    /// `DuplicateImageObjs`.
    pub duplicate_image_objs: BTreeSet<i64>,
    /// `DuplicateInfoObjects`.
    pub duplicate_info_objs: BTreeSet<i64>,
}

/// `WriteContext`: the state of writing one PDF file, and the file so far.
#[derive(Debug)]
pub struct WriteContext {
    out: Vec<u8>,
    /// The charge of `out`'s capacity.
    held: Option<Held>,
    mem: Mem,
    /// The write offset of the next object. Go keeps it by hand, and so does this: the writers
    /// that Go does not count (the cross-reference section, the trailer) leave it where it is.
    pub offset: i64,
    /// The end-of-line sequence (`Configuration.Eol`, `\n`).
    pub eol: &'static str,
    /// The write offset of every object written, by object number (`Table`).
    pub table: BTreeMap<i64, i64>,
    /// True while objects are compressed into object streams.
    pub write_to_object_stream: bool,
    /// The number of the object stream that gets the next compressed object.
    pub current_obj_stream: Option<i64>,
    /// The charge of the capacity of that object stream's body.
    obj_stream_held: Option<Held>,
    /// `Configuration.WriteObjectStream`: compress objects into object streams. A document that
    /// is encrypted and was not read from xref streams gives it up.
    pub write_object_stream: bool,
    /// `Configuration.WriteXRefStream`: write an xref stream, not a table.
    pub write_xref_stream: bool,
    /// Go's `ctx.Optimize`, `None` for a created document.
    pub optimize: Option<RedundantObjs>,
    /// The clock and the random source.
    pub env: WriteEnv,
    pub hooks: WriteHooks,
}

/// Makes room in `v` for `n` more bytes, doubling, and charges the growth of its capacity to
/// `held` (from `mem` when there is none yet) first.
fn reserve_charged(
    v: &mut Vec<u8>,
    held: &mut Option<Held>,
    mem: &Mem,
    n: usize,
) -> Result<(), Error> {
    let need = v.len().checked_add(n).ok_or(Error::OutOfMemory)?;
    let cap = v.capacity();
    if need <= cap {
        return Ok(());
    }
    let new_cap = need.max(cap.saturating_mul(2));
    let extra = u64::try_from(new_cap - cap).map_err(|_| Error::OutOfMemory)?;
    match held.as_mut() {
        Some(h) => h.grow_by(extra)?,
        None => *held = Some(mem.charge(extra)?),
    }
    v.try_reserve_exact(new_cap - v.len())?;
    Ok(())
}

impl WriteContext {
    /// `NewWriteContext`: the configuration of the fixed Go settings (object streams and an xref
    /// stream on, `\n` line ends). `mem` is the ledger of the document being written; the output
    /// is charged to it.
    pub fn new(env: WriteEnv, hooks: WriteHooks, mem: Mem) -> WriteContext {
        WriteContext {
            out: Vec::new(),
            held: None,
            mem,
            offset: 0,
            eol: "\n",
            table: BTreeMap::new(),
            write_to_object_stream: false,
            current_obj_stream: None,
            obj_stream_held: None,
            write_object_stream: true,
            write_xref_stream: true,
            optimize: None,
            env,
            hooks,
        }
    }

    /// `SetWriteOffset`: records the current write offset for the object `obj_nr`.
    pub fn set_write_offset(&mut self, obj_nr: i64) {
        self.table.insert(obj_nr, self.offset);
    }

    /// `HasWriteOffset`: whether the object `obj_nr` is written already.
    pub fn has_write_offset(&self, obj_nr: i64) -> bool {
        self.table.contains_key(&obj_nr)
    }

    /// Moves the write offset on by `n` bytes.
    pub(crate) fn advance(&mut self, n: usize) {
        self.offset = self
            .offset
            .saturating_add(i64::try_from(n).unwrap_or(i64::MAX));
    }

    /// Makes room for `n` more bytes, charging the growth to the ledger first.
    fn reserve(&mut self, n: usize) -> Result<(), Error> {
        reserve_charged(&mut self.out, &mut self.held, &self.mem, n)
    }

    /// `Write`: appends `b` to the output and returns how many bytes it wrote.
    ///
    /// # Errors
    /// [`Error::Alloc`] and [`Error::OutOfMemory`].
    pub fn write(&mut self, b: &[u8]) -> Result<usize, Error> {
        self.reserve(b.len())?;
        self.out.extend_from_slice(b);
        Ok(b.len())
    }

    /// `WriteString`.
    ///
    /// # Errors
    /// As [`WriteContext::write`].
    pub fn write_str(&mut self, s: &str) -> Result<usize, Error> {
        self.write(s.as_bytes())
    }

    /// `WriteEol`.
    ///
    /// # Errors
    /// As [`WriteContext::write`].
    pub fn write_eol(&mut self) -> Result<(), Error> {
        let eol = self.eol;
        self.write_str(eol)?;
        Ok(())
    }

    /// The bytes written so far.
    pub fn bytes(&self) -> &[u8] {
        &self.out
    }

    /// The file, once written. Its charge is released.
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

// -------------------------------------------------------------------------------------------------
// The free list

/// `UndeleteObject`: ensures an object is not recorded in the free list (e.g. after an indirect
/// reference to a free object in the original file).
///
/// # Errors
/// The errors of `XRefTable::free`; [`Error::Panic`] where Go dereferences a nil offset;
/// [`Error::Corrupt`] for a free list that runs in a circle.
fn undelete_object(x: &mut XRefTable, obj_nr: i64) -> Result<(), Error> {
    if x.free(0)?.is_none() {
        return Err(nil("nil free list head"));
    }
    // The entry whose offset is the next free object, starting with the head.
    let mut f = 0;
    // The list can not be longer than the table; a longer walk is a circle (Go loops for ever).
    for _ in 0..=x.table.len() {
        let next = x
            .table
            .get(&f)
            .and_then(|e| e.offset)
            .ok_or_else(|| nil("nil free list offset"))?;
        // Until we have found the last free object, which should point to object 0.
        if next == 0 {
            return Ok(());
        }
        let Some(entry) = x.free(next)? else {
            return Err(nil("nil free list entry"));
        };
        if next == obj_nr {
            let after = entry.offset.ok_or_else(|| nil("nil free list offset"))?;
            let generation = entry
                .generation
                .ok_or_else(|| nil("nil free list generation"))?;
            if let Some(prev) = x.table.get_mut(&f) {
                prev.offset = Some(after);
            }
            if let Some(e) = x.table.get_mut(&next) {
                e.offset = None;
                if generation > 0 {
                    e.generation = Some(generation - 1);
                }
                e.free = false;
            }
            return Ok(());
        }
        f = next;
    }
    Err(Error::Corrupt("free list is circular".into()))
}

// -------------------------------------------------------------------------------------------------
// The document

/// `writeRootAttrsBatch1`: the catalog entries written before the structure tree root.
const ROOT_ENTRIES_1: &[&[u8]] = &[
    b"Extensions",
    b"PageLabels",
    b"Names",
    b"Dests",
    b"ViewerPreferences",
    b"PageLayout",
    b"PageMode",
    b"Outlines",
    b"Threads",
    b"OpenAction",
    b"AA",
    b"URI",
    b"AcroForm",
    b"Metadata",
];

/// `writeRootAttrsBatch2`: the catalog entries written after the structure tree root.
const ROOT_ENTRIES_2: &[&[u8]] = &[
    b"MarkInfo",
    b"Lang",
    b"SpiderInfo",
    b"OutputIntents",
    b"PieceInfo",
    b"OCProperties",
    b"Perms",
    b"Legal",
    b"Requirements",
    b"Collection",
    b"NeedsRendering",
    b"DSS",
    b"AF",
    b"DPartRoot",
];

/// `writeRootObject`: the catalog, then the page tree, then the entries of the catalog.
fn write_root_object(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    // => 7.7.2 Document Catalog
    let catalog = ctx.xref.root.ok_or_else(|| nil("nil Root"))?;

    // Ensure corresponding and accurate name tree object graphs.
    ctx.xref
        .bind_name_trees()
        .map_err(|e| e.context("write root: bind name trees"))?;

    // A repaired catalog (read.go:1760) is Go's table map itself, so the changes made through
    // `catalog_mut` (the snapshot here) show in Go's dereference. The copy is charged until the
    // catalog is written.
    let repaired = ctx.xref.root_dict.is_some();
    let mem = ctx.xref.mem().clone();
    let mut _copy = mem.hold();
    let mut d = match ctx.xref.dereference_dict_ref(catalog)? {
        Some(_) if repaired => Dict::default(),
        Some(d) => {
            _copy = mem.charge(dict_copy_bytes(d))?;
            d.clone()
        }
        None => return Err(Error::Corrupt("unable to dereference root dict".into())),
    };
    if let Some(rd) = &ctx.xref.root_dict {
        _copy = mem.charge(dict_copy_bytes(rd))?;
        d = rd.clone();
    }

    objects::write_dict_object(ctx, w, catalog.num, catalog.generation, &mut d, cancel)?;

    objects::write_entry(ctx, w, &d, b"Version", cancel)?;
    pages::write_pages(ctx, w, &d, cancel)?;

    for name in ROOT_ENTRIES_1 {
        check(cancel)?;
        objects::write_entry(ctx, w, &d, name, cancel)?;
    }

    // writeRootEntryToObjStream
    w.write_to_object_stream = true;
    objects::write_entry(ctx, w, &d, b"StructTreeRoot", cancel)?;
    objects::stop_object_stream(ctx, w, cancel)?;

    for name in ROOT_ENTRIES_2 {
        check(cancel)?;
        objects::write_entry(ctx, w, &d, name, cancel)?;
    }

    // Preserve the remaining catalog references, including developer extensions. Objects that are
    // written already are skipped, which keeps the specialised page tree output.
    objects::write_direct_dict(ctx, w, d, cancel)
}

/// `writeDocumentInfoDict`: the document information dictionary and what it refers to.
fn write_document_info_dict(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    // The document info object is optional.
    let Some(info) = ctx.xref.info else {
        return Ok(());
    };
    if ctx.xref.dereference_dict_ref(info)?.is_none() {
        return Ok(());
    }
    objects::write_deep_object(ctx, w, Object::Ref(info), cancel)
}

/// `writeAdditionalStreams`. Go hands a `*types.Array` to `writeDeepObject`, which does nothing
/// with it, so nothing is written.
fn write_additional_streams(cancel: &AtomicBool) -> Result<(), Error> {
    check(cancel)
}

/// `writeEncryptDict`: the encryption dictionary, as it is, if the output is encrypted.
fn write_encrypt_dict(ctx: &mut Context, w: &mut WriteContext) -> Result<(), Error> {
    // Bail out unless we really have to write encrypted.
    let Some(r) = ctx.xref.encrypt else {
        return Ok(());
    };
    if ctx.xref.enc_key.is_empty() {
        return Ok(());
    }
    let s = ctx
        .xref
        .encrypt_dict()
        .map_err(|e| {
            crate::crypto::classify_encryption_dictionary_error(e)
                .context(format!("encryption dictionary obj#{}: dereference", r.num))
        })?
        .pdf_string();
    objects::write_object(ctx, w, r.num, r.generation, &s)
        .map_err(|e| e.context(format!("encryption dictionary obj#{}: write", r.num)))
}

/// `writeObjects`.
fn write_objects(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    // Write the root object (the document catalog) and the page tree.
    write_root_object(ctx, w, cancel)?;
    // Write the document information dictionary.
    write_document_info_dict(ctx, w, cancel)?;
    // Write the off-spec additional streams as declared in the PDF trailer.
    write_additional_streams(cancel)?;
    write_encrypt_dict(ctx, w)
}

// -------------------------------------------------------------------------------------------------
// Preparation

/// The values of the info dictionary, dereferenced, in key order: what `fileID` hashes.
fn info_values(x: &mut XRefTable) -> Result<Vec<Object>, Error> {
    let info = x.info.ok_or_else(|| nil("nil Info"))?;
    let raw: Vec<Object> = match x.dereference_dict_ref(info)? {
        Some(d) => d.iter().map(|(_, v)| v.clone()).collect(),
        None => return Ok(Vec::new()),
    };
    let mut values = Vec::new();
    values.try_reserve(raw.len())?;
    for v in &raw {
        let o = x.dereference(v)?;
        if o.is_null() {
            // Go calls `String()` on the nil object.
            return Err(nil("nil pointer dereference"));
        }
        values.push(o.clone());
    }
    Ok(values)
}

/// `ensureFileID`: a new file identifier; the second half of an existing `/ID` is replaced by it.
pub(crate) fn ensure_file_id(ctx: &mut Context, w: &WriteContext) -> Result<(), Error> {
    let values = if ctx.xref.version()? < Version::V20 {
        info_values(&mut ctx.xref)?
    } else {
        Vec::new()
    };
    let size = i64::try_from(ctx.read_file_size()).unwrap_or(i64::MAX);
    let fid = crate::crypto::file_id(&w.env, size, &values);

    match ctx.xref.id.as_mut() {
        None => ctx.xref.id = Some(vec![fid.clone(), fid.clone()]),
        Some(a) => {
            if a.len() != 2 {
                return Err(Error::Corrupt("id must be an array with 2 elements".into()));
            }
            a[1] = fid.clone();
        }
    }
    // Go's `ID` is the array of the xref stream dictionary it was read from, which changes too.
    if let Some(n) = ctx.xref.id_xref_stm
        && let Some(Entry {
            object: Some(EntryObject::XRefStm(x)),
            ..
        }) = ctx.xref.table.get_mut(&n)
        && let Some(Object::Array(a)) = x.stream.dict.0.get_mut(b"ID".as_slice())
        && a.len() == 2
    {
        a[1] = fid;
    }
    Ok(())
}

/// `ensureInfoDictAndFileID`.
fn ensure_info_dict_and_file_id(ctx: &mut Context, w: &WriteContext) -> Result<(), Error> {
    if ctx.xref.version()? < Version::V20 {
        (w.hooks.ensure_info_dict)(ctx, &w.env)?;
    }
    ensure_file_id(ctx, w)
}

/// `handleEncryption`, for the commands that are reached (it only has the default branch): the
/// output of an encrypted document that was not read from xref streams has none either.
fn handle_encryption(ctx: &Context, w: &mut WriteContext) -> Result<(), Error> {
    if ctx.xref.encrypt.is_some() && !ctx.xref.enc_key.is_empty() {
        let read = ctx.read.as_ref().ok_or_else(|| nil("nil read context"))?;
        if !read.using_xref_streams {
            w.write_object_stream = false;
            w.write_xref_stream = false;
        }
    }
    Ok(())
}

/// `prepareContextForWriting`.
fn prepare_context_for_writing(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    ensure_info_dict_and_file_id(ctx, w)?;
    handle_encryption(ctx, w).map_err(|e| e.context("encryption"))?;
    if ctx.xref.pdf20()? {
        (w.hooks.prepare_core_fonts)(ctx, cancel)?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Redundant objects

/// `detectLinearizationObjs`: a stream that sits where the primary or the overflow hint table of
/// the linearized source was is a linearization object.
fn detect_linearization_objs(x: &mut XRefTable, obj_nr: i64) -> Result<(), Error> {
    let Some(entry) = x.table.get(&obj_nr) else {
        return Ok(());
    };
    let is_stream = matches!(&entry.object, Some(EntryObject::Obj(Object::Stream(_))));
    let offset = entry.offset;
    if !is_stream {
        return Ok(());
    }
    let offset = offset.ok_or_else(|| nil("nil offset"))?;
    let primary = x
        .offset_primary_hint_table
        .ok_or_else(|| nil("nil primary hint table offset"))?;
    if offset == primary {
        x.linearization_objs.insert(obj_nr);
    }
    if x.offset_overflow_hint_table == Some(offset) {
        x.linearization_objs.insert(obj_nr);
    }
    Ok(())
}

/// `deleteRedundantObject`: frees the object `obj_nr` if it is a duplicate, a linearization
/// object or an object stream of the source. Go ignores the errors of `FreeObject`.
fn delete_redundant_object(
    ctx: &mut Context,
    redundant: &RedundantObjs,
    obj_nr: i64,
) -> Result<(), Error> {
    // The selected pages are always empty.
    if redundant.duplicate_font_objs.contains(&obj_nr)
        || redundant.duplicate_image_objs.contains(&obj_nr)
    {
        let _ = ctx.xref.free_object(obj_nr);
    }

    let free = ctx.xref.linearization_objs.contains(&obj_nr)
        || redundant.duplicate_info_objs.contains(&obj_nr)
        || ctx
            .read
            .as_ref()
            .ok_or_else(|| nil("nil read context"))?
            .is_object_stream_object(obj_nr);
    if free {
        let _ = ctx.xref.free_object(obj_nr);
    }
    Ok(())
}

/// `deleteRedundantObjects`: marks the objects that were not written and are redundant (duplicate
/// resources, compressed objects, linearization dictionaries, ...) as free.
fn delete_redundant_objects(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let Some(mut redundant) = w.optimize.take() else {
        return Ok(());
    };
    let r = delete_redundant_objects_in(ctx, w, &mut redundant, cancel);
    w.optimize = Some(redundant);
    r
}

fn delete_redundant_objects_in(
    ctx: &mut Context,
    w: &WriteContext,
    redundant: &mut RedundantObjs,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let size = ctx.xref.size.ok_or_else(|| nil("nil Size"))?;
    // Go loops over every number below `Size`, but only the numbers in the table do anything and
    // the loop adds no entries, so those are visited alone: a repaired `Size` is the highest object
    // number plus one, which a corrupt `obj` line can put in the trillions.
    let mut next = 0;
    while let Some(i) = ctx
        .xref
        .table
        .range(next..size.max(next))
        .next()
        .map(|(&k, _)| k)
    {
        next = i + 1;
        check(cancel)?;

        // A missing object remains missing, and so does a free one.
        let Some(entry) = ctx.xref.find(i) else {
            continue;
        };
        if entry.free {
            continue;
        }
        let has_offset = entry.offset.is_some();

        // An object that is written. Resources may be cross referenced from different objects,
        // e.g. font descriptors may be shared by different font dicts: try to remove this object
        // from the sets of potential duplicate objects.
        if w.has_write_offset(i) {
            redundant.duplicate_font_objs.remove(&i);
            redundant.duplicate_image_objs.remove(&i);
            redundant.duplicate_info_objs.remove(&i);
            continue;
        }

        // An object that is not written.
        let linearized = ctx
            .read
            .as_ref()
            .ok_or_else(|| nil("nil read context"))?
            .linearized;
        if linearized && has_offset {
            // This applies to pre-existing objects only. There is no type entry for the stream
            // dictionaries of a linearization dictionary, so every stream that was not written is
            // checked.
            detect_linearization_objs(&mut ctx.xref, i)?;
        }

        delete_redundant_object(ctx, redundant, i)?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// The cross-reference section

/// `sortedWritableKeys`: the numbers of the free objects and of the objects written, ascending,
/// and their charge to the ledger (8 per table entry, made before the list).
fn sorted_writable_keys(
    ctx: &Context,
    w: &WriteContext,
    cancel: &AtomicBool,
) -> Result<(Vec<i64>, Held), Error> {
    let held = ctx
        .xref
        .mem()
        .charge((ctx.xref.table.len() as u64).saturating_mul(8))?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(ctx.xref.table.len())?;
    for (i, e) in &ctx.xref.table {
        check(cancel)?;
        if e.free || w.has_write_offset(*i) {
            keys.push(*i);
        }
    }
    Ok((keys, held))
}

/// `writeXRefSubsection`: the entries `start..start + size` of a cross-reference table.
fn write_xref_subsection(
    ctx: &Context,
    w: &mut WriteContext,
    start: i64,
    size: i64,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let eol = w.eol;
    w.write_str(&format!("{start} {size}{eol}"))?;

    for i in start..start.saturating_add(size) {
        check(cancel)?;

        let entry = ctx
            .xref
            .table
            .get(&i)
            .ok_or_else(|| nil("nil xref entry"))?;
        if entry.compressed {
            return Err(Error::Corrupt("compressed entries present".into()));
        }

        let generation = entry.generation.ok_or_else(|| nil("nil generation"))?;
        let s = if entry.free {
            let offset = entry.offset.ok_or_else(|| nil("nil offset"))?;
            format!("{offset:010} {generation:05} f{eol:>2}")
        } else {
            let offset = w.table.get(&i).copied().unwrap_or(0);
            format!("{offset:010} {generation:05} n{eol:>2}")
        };
        w.write_str(&s)?;
    }
    Ok(())
}

/// `writeTrailerDict`: `trailer` and the trailer dictionary.
fn write_trailer_dict(ctx: &Context, w: &mut WriteContext) -> Result<(), Error> {
    w.write_str("trailer")?;
    w.write_eol()?;

    let x = &ctx.xref;
    let mut d = Dict::default();
    d.insert_int(b"Size", x.size.ok_or_else(|| nil("nil Size"))?);
    d.insert(b"Root", Object::Ref(x.root.ok_or_else(|| nil("nil Root"))?));
    if let Some(info) = x.info {
        d.insert(b"Info", Object::Ref(info));
    }
    if let Some(encrypt) = x.encrypt
        && !x.enc_key.is_empty()
    {
        d.insert(b"Encrypt", Object::Ref(encrypt));
    }
    if let Some(id) = &x.id {
        d.insert(b"ID", Object::Array(id.clone()));
    }
    w.write(&d.pdf_string())?;
    Ok(())
}

/// `writeXRefTable`: after the last object, the cross-reference table, the trailer and the
/// offset of the table.
fn write_xref_table(ctx: &Context, w: &mut WriteContext, cancel: &AtomicBool) -> Result<(), Error> {
    let (keys, _keys_held) = sorted_writable_keys(ctx, w, cancel)?;

    w.write_str("xref")?;
    w.write_eol()?;

    let Some(&first) = keys.first() else {
        return Err(nil("index out of range"));
    };
    let mut start = first;
    let mut size: i64 = 1;
    for pair in keys.windows(2) {
        check(cancel)?;
        if pair[1].saturating_sub(pair[0]) > 1 {
            write_xref_subsection(ctx, w, start, size, cancel)?;
            start = pair[1];
            size = 1;
            continue;
        }
        size += 1;
    }
    write_xref_subsection(ctx, w, start, size, cancel)?;

    write_trailer_dict(ctx, w)?;
    w.write_eol()?;
    w.write_str("startxref")?;
    w.write_eol()?;
    let offset = w.offset.to_string();
    w.write_str(&offset)?;
    w.write_eol()
}

/// `int64ToBuf`: `i` as big-endian bytes, left-padded with zeros to `byte_count` (and longer if
/// `i` does not fit).
fn int64_to_buf(i: i64, byte_count: usize) -> Vec<u8> {
    let mut b = Vec::new();
    let mut k = i;
    while k > 0 {
        b.push((k & 0xff) as u8);
        k >>= 8;
    }
    b.reverse();
    if b.len() < byte_count {
        let mut buf = vec![0; byte_count - b.len()];
        buf.extend_from_slice(&b);
        buf
    } else {
        b
    }
}

/// `createXRefStream`: the content of the xref stream for the objects `obj_nrs`, with the `W`
/// widths, and its `/Index` array.
fn create_xref_stream(
    ctx: &Context,
    w: &WriteContext,
    widths: [usize; 3],
    obj_nrs: &[i64],
    cancel: &AtomicBool,
) -> Result<(Bytes, Vec<Object>), Error> {
    let [i1, i2, i3] = widths;
    let mut held = ctx.xref.mem().charge(0)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut a: Vec<Object> = Vec::new();

    let mut start = *obj_nrs.first().ok_or_else(|| nil("index out of range"))?;
    let mut size: i64 = 0;

    for (i, &j) in obj_nrs.iter().enumerate() {
        check(cancel)?;

        let entry = ctx
            .xref
            .table
            .get(&j)
            .ok_or_else(|| nil("nil xref entry"))?;
        let (s1, s2, s3) = if entry.free {
            // Unused.
            let next = entry.offset.ok_or_else(|| nil("nil offset"))?;
            let generation = entry.generation.ok_or_else(|| nil("nil generation"))?;
            (
                int64_to_buf(0, i1),
                int64_to_buf(next, i2),
                int64_to_buf(generation, i3),
            )
        } else if entry.compressed {
            // In use, compressed into an object stream.
            let stm = entry
                .object_stream
                .ok_or_else(|| nil("nil object stream"))?;
            let ind = entry
                .object_stream_ind
                .ok_or_else(|| nil("nil object stream index"))?;
            (
                int64_to_buf(2, i1),
                int64_to_buf(stm, i2),
                int64_to_buf(ind, i3),
            )
        } else {
            // In use, uncompressed. Go names the index in the loop, not the object, here.
            let off = w
                .table
                .get(&j)
                .copied()
                .ok_or_else(|| Error::Corrupt(format!("missing write offset for obj #{i}")))?;
            let generation = entry.generation.ok_or_else(|| nil("nil generation"))?;
            (
                int64_to_buf(1, i1),
                int64_to_buf(off, i2),
                int64_to_buf(generation, i3),
            )
        };

        let n = s1.len() + s2.len() + s3.len();
        held.grow_by(n as u64)?;
        buf.try_reserve(n)?;
        buf.extend_from_slice(&s1);
        buf.extend_from_slice(&s2);
        buf.extend_from_slice(&s3);

        if i > 0 && obj_nrs[i].saturating_sub(obj_nrs[i - 1]) > 1 {
            a.try_reserve(2)?;
            a.push(Object::Integer(start));
            a.push(Object::Integer(size));
            start = obj_nrs[i];
            size = 1;
            continue;
        }
        size += 1;
    }

    a.try_reserve(2)?;
    a.push(Object::Integer(start));
    a.push(Object::Integer(size));

    let len = buf.len();
    Ok((Bytes::new(buf, len, Some(held)), a))
}

/// `newXRefStreamDict`.
fn new_xref_stream_dict(ctx: &Context) -> Result<StreamDict, Error> {
    let x = &ctx.xref;
    let mut dict = Dict::default();
    dict.insert_name(b"Type", b"XRef");
    dict.insert_name(b"Filter", FLATE);
    dict.insert(b"Root", Object::Ref(x.root.ok_or_else(|| nil("nil Root"))?));
    if let Some(info) = x.info {
        dict.insert(b"Info", Object::Ref(info));
    }
    if let Some(id) = &x.id {
        dict.insert(b"ID", Object::Array(id.clone()));
    }
    if let Some(encrypt) = x.encrypt
        && !x.enc_key.is_empty()
    {
        dict.insert(b"Encrypt", Object::Ref(encrypt));
    }
    Ok(StreamDict {
        dict,
        filter_pipeline: vec![PdfFilter {
            name: FLATE.to_vec(),
            decode_parms: None,
        }],
        ..StreamDict::default()
    })
}

/// The number of bytes of the big-endian form of `i`.
fn byte_len(mut i: i64) -> usize {
    let mut n = 0;
    while i > 0 {
        i >>= 8;
        n += 1;
    }
    n
}

/// `writeXRefStream`: the cross-reference stream, which has an entry of its own, and the offset
/// of it.
fn write_xref_stream(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let mem = ctx.xref.mem().clone();
    let mut sd = new_xref_stream_dict(ctx)?;

    // The table keeps the dictionary as it is before `Size`, `W` and `Index` go in.
    let snapshot = XRefStreamDict {
        stream: sd.clone(),
        ..XRefStreamDict::default()
    };
    let entry = Entry {
        generation: Some(0),
        object: Some(EntryObject::XRefStm(Box::new(snapshot))),
        ..Entry::default()
    };
    // Reuse free objects (including recycled objects from this run).
    let obj_number = ctx.xref.insert_and_use_recycled(entry)?;

    let size = ctx.xref.size.ok_or_else(|| nil("nil Size"))?;
    sd.dict.insert_int(b"Size", size);

    // Include the xref stream dict object within the xref stream.
    let offset = w.offset;
    w.set_write_offset(obj_number);
    let (obj_nrs, _obj_nrs_held) = sorted_writable_keys(ctx, w, cancel)?;

    let i2_base = size.max(offset);
    let i1 = 1; // 0, 1 or 2 always fit into 1 byte.
    let i2 = byte_len(i2_base);
    let i3 = 2; // scale for the max object stream index <= 0xffff

    sd.dict.insert(
        b"W",
        Object::Array(vec![
            Object::Integer(i1 as i64),
            Object::Integer(i2 as i64),
            Object::Integer(i3 as i64),
        ]),
    );

    // Generate the content: the xref entries.
    let (content, index) = create_xref_stream(ctx, w, [i1, i2, i3], &obj_nrs, cancel)?;
    sd.content = Some(content);
    sd.dict.insert(b"Index", Object::Array(index));

    // Encode the content -> raw.
    sd.encode(&mem, cancel)?;

    objects::write_stream_dict_object(ctx, w, obj_number, 0, sd, cancel)?;

    w.write_str("startxref")?;
    w.write_eol()?;
    let offset = offset.to_string();
    w.write_str(&offset)?;
    w.write_eol()
}

/// `writeXRef`: the cross-reference stream or table.
fn write_xref(ctx: &mut Context, w: &mut WriteContext, cancel: &AtomicBool) -> Result<(), Error> {
    check(cancel)?;
    if w.write_xref_stream {
        // Write the cross reference stream, and generate the object streams.
        return write_xref_stream(ctx, w, cancel);
    }
    // Write the cross reference table section.
    write_xref_table(ctx, w, cancel)
}

// -------------------------------------------------------------------------------------------------
// The file

/// `runWritePhase`: a phase of the write that does not start once the cancel flag is set.
fn phase<T>(cancel: &AtomicBool, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    check(cancel)?;
    f()
}

/// `WriteContext`: writes the document as a PDF file into `w`.
///
/// The file is the header (`%PDF-1.7`, or `%PDF-2.0` for a PDF 2.0 document), the catalog, the
/// page tree and the objects they refer to, the document information dictionary, the encryption
/// dictionary, the cross-reference section and `%%EOF`. Objects are compressed into object
/// streams of at most [`OBJECT_STREAM_MAX_OBJECTS`] objects if `w.write_object_stream` and
/// `w.write_xref_stream` are set (they are, unless an encrypted document was not read from xref
/// streams). Everything is charged to the document's ledger.
///
/// The document is changed as Go's writer changes it: the file identifier and the info
/// dictionary are updated, new objects (object streams, the xref stream) are inserted, objects
/// that were written compressed are marked as such, and redundant objects are freed.
///
/// # Errors
/// [`Error::Cancelled`] (checked before every phase and at every object), the errors of the
/// ledger, `write PDF: <phase>: ...` around the failure of a phase, and the errors of the
/// document: a missing catalog or page tree, a stream whose raw bytes are not as long as its
/// `/Length`, the encryption errors.
pub fn write_context(
    ctx: &mut Context,
    w: &mut WriteContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;

    phase(cancel, || prepare_context_for_writing(ctx, w, cancel))
        .map_err(|e| e.context("write PDF: prepare context"))?;

    // We support PDF Collections (since V1.7) for file attachments.
    let v = if ctx.xref.pdf20()? {
        Version::V20
    } else {
        Version::V17
    };

    phase(cancel, || objects::write_header(w, &v.to_string()))
        .map_err(|e| e.context("write PDF: header"))?;

    // Ensure there is no root version.
    if ctx.xref.root_version.is_some()
        && let Ok(root) = ctx.xref.catalog_mut()
    {
        root.delete(b"Version");
    }

    phase(cancel, || write_objects(ctx, w, cancel)).map_err(|e| e.context("write PDF: objects"))?;

    // Mark redundant objects as free, e.g. duplicate resources, compressed objects,
    // linearization dicts.
    phase(cancel, || delete_redundant_objects(ctx, w, cancel))?;

    phase(cancel, || write_xref(ctx, w, cancel)).map_err(|e| e.context("write PDF: xref"))?;

    // Write the PDF trailer.
    phase(cancel, || objects::write_trailer(w)).map_err(|e| e.context("write PDF: trailer"))?;

    check(cancel)
}

#[cfg(test)]
mod tests;
