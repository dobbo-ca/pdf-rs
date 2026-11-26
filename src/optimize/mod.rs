//! Optimization (docs/design.md section 2; Go: pkg/pdfcpu/optimize.go, model/context.go
//! `OptimizationContext`, model/resource.go `FontObject` and `ImageObject`).
//!
//! [`optimize_xref_table`] is `OptimizeXRefTable`. It runs these phases, each after a cancel
//! check:
//!
//! 1. `fixReferencesToFreeObjects`: a reference to a free object becomes a reference to one
//!    null object. The walk keeps an explicit stack (`frame.rs`).
//! 2. `optimizeResourceDicts`: [`XRefTable::consolidate_page_resources`], only for the command
//!    `Validate` (`shouldOptimizeResourceDicts`).
//! 3. `optimizeFontAndImages`: walks the page tree and finds the fonts, images and form
//!    XObjects that are duplicates of others, in the page resources, the form resources and the
//!    soft masks of the graphics states. A reference to a duplicate is replaced by one to the
//!    original, and the duplicates are recorded in the [`OptimizationContext`] for the writer,
//!    which frees them.
//! 4. `ensureDirectWidthForXObjs`: the `Width` of the images that pages use becomes direct.
//! 5. The `PieceInfo` of the catalog, the pages and the form XObjects is deleted.
//!
//! [`cache_form_fonts`] is `CacheFormFonts`, which `ReadValidateAndOptimize` calls afterwards.
//!
//! Differences from Go:
//!
//! * Go shares dictionaries by reference, so every write through an alias persists. Here a
//!   dictionary is named by a `Loc` (see `frame.rs`) and read or changed through the table.
//!   What Go keeps as an alias in the [`OptimizationContext`] (the dictionary of a registered
//!   font, the stream of a registered image, the entries of the form cache, the font
//!   dictionaries that are duplicates) is kept as the object number and read from the table
//!   when it is used, which is what the alias shows. A font resource dictionary that is
//!   corrupt is kept as its `Loc`.
//! * Go's `Context.Optimize` is `Context::optimize` (a `write::RedundantObjs`, which WP 26
//!   added) between calls. [`optimize_xref_table`] builds the [`OptimizationContext`] from it,
//!   fills in the sets, and puts the three sets back; the other registries live for the one
//!   call, because nothing that is reached reads them afterwards. So [`cache_form_fonts`]
//!   registers the fonts of the form in a context of its own, which it drops.
//! * Not ported, because nothing observable depends on them (docs/design.md section 1, row
//!   13): the content-stream deduplication (optimize.go:46-247, off by configuration:
//!   `OptimizeDuplicateContentStreams=false`, so `ContentStreamCache` is absent), the stats
//!   (optimize.go:1365-1590, `checkForEmbeddedFont` is false without stats and for the
//!   commands byblos uses, so a [`FontObject`] has no `Embedded`, `Data` or `Extension`), the
//!   `FontDict` and `ImageDict` fields, `NonReferencedObjs`, and Go's logging.
//! * `ImageObjectHashes` starts empty and gets an entry per image that was found not to be a
//!   duplicate, as in Go (where `newOptimizationContext` creates the map, so Go's lazy build of
//!   it from `ImageObjects` never runs).
//! * `PageFonts` and `PageImages` are maps by page number that hold the pages that have a
//!   set; `page_slots` is the length of Go's slices, and a page number beyond it is Go's
//!   index-out-of-range panic ([`Error::Panic`]).
//! * The resource walk is bound by the recursion depth limit (`max_recursion_depth`), which Go
//!   does not have: a chain of form XObjects, each with the resources of the next, would grow
//!   Go's stack without limit and would overflow this one. So does the walk of the page tree.
//! * `visited` (Go's `[]types.Object` slice that is copied by value down the calls) is a
//!   `Visited` set that a call truncates back when it returns, which gives the callee the
//!   caller's entries and the caller none of the callee's, as the slice does, in O(log n).
//! * The message of `corrupt "Kids" entry` has no dump of the dictionary.
//! * Cancellation: Go checks its context where `contextutil.Check` is; the same places check
//!   the cancel flag here. `CacheFormFonts` and `OptimizeContext` have no way to be
//!   interrupted in Go either, but `optimize_xref_table` and the API call take the flag.
//! * Every map of Go that is ranged over is visited in sorted order (docs/design.md section
//!   10).
//! * The registries of the [`OptimizationContext`] are charged to the ledger as they grow
//!   (`NR_BYTES` per object number, the names and records by their size): the sets that outlive
//!   the pass (`DuplicateFontObjs`, `DuplicateImageObjs`) to the table's charge, the rest to the
//!   pass's, which drops with it. So are the stack of the free-reference walk and `visited`.

mod frame;

pub mod font_name;

use crate::config::Cmd;
use crate::crypto::sha256;
use crate::error::Error;
use crate::ledger::{Held, Mem};
use crate::model::deref::type_name;
use crate::model::equal::equal_objects;
use crate::model::{Context, EntryObject, XRefTable};
use crate::object::{Dict, Object, Ref, StreamDict, array_copy_bytes};
use crate::write::RedundantObjs;
use frame::{Cont, Loc, ObjectStack, Peek, Step, container, error_path};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicBool, Ordering};

/// The ledger estimate of one page set and one number in it (the page's map entry, the B-tree
/// node share of the number).
const PAGE_SET_BYTES: u64 = 64;

/// The ledger estimate of one object number in a set, a map or a list of the
/// [`OptimizationContext`]: a key and value slot pair of a half-full B-tree node and its share of
/// the internal nodes, or a list slot and its doubling.
const NR_BYTES: u64 = 48;

/// The ledger estimate of a name kept in a record, a list or as a map key, beside its length:
/// two slots of a half-full B-tree node and 16 for the internal nodes, as `entry_copy_bytes`.
const NAME_BYTES: u64 = 2 * size_of::<Vec<u8>>() as u64 + 16;

/// The ledger estimate of one reference in [`Visited`]: its set slot (as [`NAME_BYTES`]) and its
/// list slot with doubling.
const VISITED_BYTES: u64 = 4 * size_of::<Ref>() as u64 + 16;

/// The ledger estimate of a registered font: its map slot pair and its names.
fn font_object_bytes(fo: &FontObject) -> u64 {
    let names = fo.resource_names.iter().chain([
        &fo.prefix,
        &fo.font_name,
        &fo.subtype_name,
        &fo.encoding_name,
    ]);
    2 * (8 + size_of::<FontObject>() as u64)
        + 16
        + names.map(|n| NAME_BYTES + n.len() as u64).sum::<u64>()
}

/// The ledger estimate of a resource name kept in an [`ImageObject`]: its map slot pair, its
/// share of the internal nodes, and the name.
fn image_name_bytes(name: &[u8]) -> u64 {
    2 * (8 + size_of::<Vec<u8>>() as u64) + 16 + name.len() as u64
}

/// The ledger estimate of a kept [`Loc`]: its list slot and doubling, and its path.
fn loc_bytes(loc: &Loc) -> u64 {
    let keys: usize = loc
        .path
        .iter()
        .map(|s| match s {
            Step::Key(k) => k.len(),
            Step::Index(_) => 0,
        })
        .sum();
    2 * size_of::<Loc>() as u64 + (size_of_val(loc.path.as_slice()) + keys) as u64
}

/// `FontObject`: a font used in a PDF file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontObject {
    pub resource_names: Vec<Vec<u8>>,
    pub prefix: Vec<u8>,
    pub font_name: Vec<u8>,
    pub subtype_name: Vec<u8>,
    pub encoding_name: Vec<u8>,
}

impl FontObject {
    /// `AddResourceName`: adds a resource name that refers to this font.
    pub fn add_resource_name(&mut self, name: &[u8]) {
        if !self.resource_names.iter().any(|n| n == name) {
            self.resource_names.push(name.to_vec());
        }
    }
}

/// `ImageObject`: an image used in a PDF file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageObject {
    /// The resource name of the image by page number.
    pub resource_names: BTreeMap<i64, Vec<u8>>,
}

impl ImageObject {
    /// `AddResourceName`.
    pub fn add_resource_name(&mut self, page_nr: i64, name: &[u8]) {
        self.resource_names.insert(page_nr, name.to_vec());
    }
}

/// `OptimizationContext`: the state of the optimization of one document. Every set of object
/// numbers is Go's `IntSet`, whose members are all true.
#[derive(Debug, Clone, Default)]
pub struct OptimizationContext {
    /// The number of pages that `PageFonts` and `PageImages` have slots for (Go's slice length).
    pub page_slots: i64,
    /// `PageFonts`: for each page the numbers of its font objects.
    pub page_fonts: BTreeMap<i64, BTreeSet<i64>>,
    /// `FontObjects`: the registered fonts by object number.
    pub font_objects: BTreeMap<i64, FontObject>,
    /// `FormFontObjects`: the fonts of the interactive form by object number.
    pub form_font_objects: BTreeMap<i64, FontObject>,
    /// `Fonts`: all font object numbers registered for a font name.
    pub fonts: BTreeMap<Vec<u8>, Vec<i64>>,
    /// `DuplicateFonts`: the numbers of the font dictionaries that duplicate another one.
    pub duplicate_fonts: BTreeSet<i64>,
    /// `DuplicateFontObjs`: the union of the object graphs of all duplicate font dictionaries.
    pub duplicate_font_objs: BTreeSet<i64>,
    /// `CorruptFontResDicts`: the font resource dictionaries with a null entry.
    pub(crate) corrupt_font_res_dicts: Vec<Loc>,
    /// `PageImages`: for each page the numbers of its image objects.
    pub page_images: BTreeMap<i64, BTreeSet<i64>>,
    /// `ImageObjects`: the registered images by object number.
    pub image_objects: BTreeMap<i64, ImageObject>,
    /// `ImageObjectHashes`: image object numbers by the SHA-256 of their encoded stream.
    pub image_object_hashes: BTreeMap<[u8; 32], Vec<i64>>,
    /// `DuplicateImages`: the numbers of the image streams that duplicate another one, and the
    /// number of the one that is kept (`NewObjNr`).
    pub duplicate_images: BTreeMap<i64, i64>,
    /// `DuplicateImageObjs`: the union of the object graphs of all duplicate image streams.
    pub duplicate_image_objs: BTreeSet<i64>,
    /// `FormStreamCache`: the form XObjects that were seen, for duplicate detection.
    pub form_stream_cache: BTreeSet<i64>,
    /// `FormResourceCache`: the form XObjects whose resources were processed, by page number.
    pub form_resource_cache: BTreeMap<i64, BTreeSet<i64>>,
    /// `DuplicateInfoObjects`: a possible result of a manual change of the info dictionary.
    pub duplicate_info_objs: BTreeSet<i64>,
    /// `Cache`: the objects visited during the optimization.
    pub cache: BTreeSet<i64>,
    /// `NullObjNr`: the number of a regular null object, which fixes references to free
    /// objects.
    pub null_obj_nr: Option<i64>,
}

impl OptimizationContext {
    /// `newOptimizationContext`.
    pub fn new() -> OptimizationContext {
        OptimizationContext::default()
    }

    /// The context of a document whose redundant objects are `r` (`Context::optimize`).
    pub fn from_redundant(r: RedundantObjs) -> OptimizationContext {
        OptimizationContext {
            duplicate_font_objs: r.duplicate_font_objs,
            duplicate_image_objs: r.duplicate_image_objs,
            duplicate_info_objs: r.duplicate_info_objs,
            ..OptimizationContext::default()
        }
    }

    /// The sets that the writer reads and prunes (`write::RedundantObjs`), which is all that
    /// outlives the optimization.
    pub fn into_redundant(self) -> RedundantObjs {
        RedundantObjs {
            duplicate_font_objs: self.duplicate_font_objs,
            duplicate_image_objs: self.duplicate_image_objs,
            duplicate_info_objs: self.duplicate_info_objs,
        }
    }
}

/// The `visited` slice of Go's resource walk: the references on the way down, and the ones that
/// earlier entries of the dictionaries on the way added. Every push of a new reference is
/// charged ([`VISITED_BYTES`]) until the walk of the page drops it; a page walks each resource
/// dictionary once (`formResourcesVisited`), so that is at most one push per entry.
#[derive(Debug)]
struct Visited {
    set: BTreeSet<Ref>,
    order: Vec<Ref>,
    held: Held,
}

impl Visited {
    fn new(mem: &Mem) -> Visited {
        Visited {
            set: BTreeSet::new(),
            order: Vec::new(),
            held: mem.hold(),
        }
    }

    fn contains(&self, r: Ref) -> bool {
        self.set.contains(&r)
    }

    fn push(&mut self, r: Ref) -> Result<(), Error> {
        if !self.set.contains(&r) {
            self.held.grow_by(VISITED_BYTES)?;
            self.set.insert(r);
            self.order.try_reserve(1)?;
            self.order.push(r);
        }
        Ok(())
    }

    /// The length to give back to [`Visited::reset`].
    fn mark(&self) -> usize {
        self.order.len()
    }

    /// Drops what was added since `mark`.
    fn reset(&mut self, mark: usize) {
        while self.order.len() > mark {
            if let Some(r) = self.order.pop() {
                self.set.remove(&r);
            }
        }
    }
}

fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `qualifiedRName`.
fn qualified_rname(prefix: &[u8], r_name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return r_name.to_vec();
    }
    let mut v = prefix.to_vec();
    v.push(b'.');
    v.extend_from_slice(r_name);
    v
}

/// `registerFontDictObjNr`, charged to `held` first.
fn register_font_dict_obj_nr(
    o: &mut OptimizationContext,
    held: &mut Held,
    f_name: &[u8],
    obj_nr: i64,
) -> Result<(), Error> {
    let key = if o.fonts.contains_key(f_name) {
        0
    } else {
        NAME_BYTES + size_of::<Vec<i64>>() as u64 + f_name.len() as u64
    };
    held.grow_by(key + NR_BYTES)?;
    o.fonts.entry(f_name.to_vec()).or_default().push(obj_nr);
    Ok(())
}

/// `newFontObject`. The lookups of the subtype and the encoding can decode a lazy object, which
/// is why they are made, though their errors are not used.
fn new_font_object(
    x: &mut XRefTable,
    font_dict: &Dict,
    resource_name: &[u8],
    prefix: &[u8],
    font_name: &[u8],
) -> FontObject {
    // font_name::name has already resolved and validated Subtype for this dictionary.
    let subtype = match x.dereference_name_entry(font_dict, b"Subtype") {
        Ok((Some(s), _)) => s,
        _ => Vec::new(),
    };

    let mut encoding_name = b"Built-in".to_vec();
    if font_dict.find(b"Encoding").is_some() {
        encoding_name = b"Custom".to_vec();
        if let Ok((Some(enc), _)) = x.dereference_name_entry(font_dict, b"Encoding") {
            encoding_name = enc;
        }
    }

    FontObject {
        resource_names: vec![resource_name.to_vec()],
        prefix: prefix.to_vec(),
        font_name: font_name.to_vec(),
        subtype_name: subtype,
        encoding_name,
    }
}

/// `CacheFormFonts`: caches the fonts of the default resources of the interactive form.
///
/// # Errors
/// The errors of the form font resources, with the context `form font resources`; for a font,
/// the errors of the dereference or of the font name, with the context `form font obj#N`.
pub fn cache_form_fonts(ctx: &mut Context) -> Result<(), Error> {
    cache_form_fonts_in(&mut ctx.xref, &mut OptimizationContext::new())
}

/// `CacheFormFonts` with the optimization context given.
fn cache_form_fonts_in(xref: &mut XRefTable, o: &mut OptimizationContext) -> Result<(), Error> {
    let d = font_name::form_font_res_dict(xref)
        .map_err(|e| e.context("form font resources"))?
        .unwrap_or_default();

    // The registries are charged until the context drops.
    let mut held = xref.mem().hold();

    // Iterate over the font resource dictionary.
    for (r_name, v) in d.iter() {
        let Object::Ref(ir) = v else {
            continue;
        };
        let obj_nr = ir.num;

        let font_dict = match xref.dereference_font_dict(*ir) {
            Ok(Some(fd)) => fd.clone(),
            Ok(None) => continue,
            Err(e) => {
                return Err(e.context(format!("form font obj#{obj_nr}: dereference font dict")));
            }
        };

        // Get the unique font name.
        let (prefix, f_name) = font_name::name(xref, &font_dict, obj_nr)
            .map_err(|e| e.context(format!("form font obj#{obj_nr}: parse font name")))?;

        register_font_dict_obj_nr(o, &mut held, &f_name, obj_nr)?;

        let font_obj = new_font_object(xref, &font_dict, r_name, &prefix, &f_name);
        held.grow_by(font_object_bytes(&font_obj))?;
        o.form_font_objects.insert(obj_nr, font_obj);
    }
    Ok(())
}

/// `OptimizeXRefTable`: optimizes the table by locating redundant embedded fonts and images. A
/// document with no pages is left as it is.
///
/// # Errors
/// [`Error::Cancelled`]; the errors of the phases, each with its context (`fix references to
/// free objects`, `optimize resources`, `optimize fonts and images`, `resolve image widths`,
/// `delete root PieceInfo`).
pub fn optimize_xref_table(ctx: &mut Context, cancel: &AtomicBool) -> Result<(), Error> {
    check(cancel)?;
    if ctx.xref.page_count == 0 {
        return Ok(());
    }
    let mut o = OptimizationContext::from_redundant(ctx.optimize.take().unwrap_or_default());
    // The working-copy reserve of the pass (docs/limits.md).
    let result = ctx.xref.start_reserve().and_then(|reserve| {
        let r = optimize_xref_table_in(&mut ctx.xref, &mut o, cancel);
        ctx.xref.end_reserve(reserve);
        r
    });
    ctx.optimize = Some(o.into_redundant());
    result
}

/// `OptimizeXRefTable` with the optimization context given.
fn optimize_xref_table_in(
    xref: &mut XRefTable,
    o: &mut OptimizationContext,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let mut op = Optimizer::new(xref, o, cancel);

    // Sometimes free objects are used although they are part of the free object list. Replace
    // references to free xref table entries with a reference to a NULL object.
    check(cancel)?;
    op.fix_references_to_free_objects()
        .map_err(|e| e.context("fix references to free objects"))?;

    if op.x.conf.cmd == Cmd::Validate {
        // Extra step with potential for performance hit when processing large files.
        check(cancel)?;
        op.x.consolidate_page_resources(cancel)
            .map_err(|e| e.context("optimize resources"))?;
    }

    // Get rid of duplicate embedded fonts and images.
    check(cancel)?;
    op.optimize_font_and_images()
        .map_err(|e| e.context("optimize fonts and images"))?;

    check(cancel)?;
    op.ensure_direct_width_for_xobjs()
        .map_err(|e| e.context("resolve image widths"))?;

    // Get rid of the PieceInfo dictionary of the root.
    check(cancel)?;
    op.delete_dict_entry_at(&Loc::root(), b"PieceInfo")
        .map_err(|e| e.context("delete root PieceInfo"))?;

    check(cancel)?;
    op.x.optimized = true;
    Ok(())
}

/// The state of one optimization.
struct Optimizer<'a> {
    x: &'a mut XRefTable,
    o: &'a mut OptimizationContext,
    cancel: &'a AtomicBool,
    /// The depth of the resource walk.
    depth: u64,
    /// The charge of the registries of `o` that live for the pass.
    held: Held,
    /// The charge of `o.cache`, given back when it is cleared.
    cache_held: Held,
}

impl<'a> Optimizer<'a> {
    fn new(
        x: &'a mut XRefTable,
        o: &'a mut OptimizationContext,
        cancel: &'a AtomicBool,
    ) -> Optimizer<'a> {
        let held = x.mem().hold();
        let cache_held = x.mem().hold();
        Optimizer {
            x,
            o,
            cancel,
            depth: 0,
            held,
            cache_held,
        }
    }
}

impl Optimizer<'_> {
    fn check(&self) -> Result<(), Error> {
        check(self.cancel)
    }

    // -----------------------------------------------------------------------------------------
    // Access to the containers

    /// The stored keys of the dictionary at `loc`, in order, and their charge to the ledger. A
    /// dictionary that is gone has none.
    fn keys(&mut self, loc: &Loc) -> Result<(Vec<Vec<u8>>, Held), Error> {
        let mem = self.x.mem().clone();
        let mut keys = Vec::new();
        let mut held = mem.charge(0)?;
        if let Some(Cont::Dict(d)) = container(self.x, loc) {
            let n: usize = d.0.keys().map(|k| size_of::<Vec<u8>>() + k.len()).sum();
            held.grow_by(n as u64)?;
            keys.try_reserve_exact(d.len())?;
            keys.extend(d.0.keys().cloned());
        }
        Ok((keys, held))
    }

    /// What the entry stored under exactly `key` is.
    fn entry(&mut self, loc: &Loc, key: &[u8]) -> Option<Peek> {
        match container(self.x, loc) {
            Some(Cont::Dict(d)) => d.0.get(key).map(Peek::of),
            _ => None,
        }
    }

    /// `Find`: what the entry for `key` is (the key found as its decoded form).
    fn peek_find(&mut self, loc: &Loc, key: &[u8]) -> Option<Peek> {
        match container(self.x, loc) {
            Some(Cont::Dict(d)) => d.find(key).map(Peek::of),
            _ => None,
        }
    }

    /// `Find`, with a copy of the entry.
    fn find(&mut self, loc: &Loc, key: &[u8]) -> Option<Object> {
        match container(self.x, loc) {
            Some(Cont::Dict(d)) => d.find(key).cloned(),
            _ => None,
        }
    }

    /// `d[key] = o`. A dictionary that is gone takes the write, as the freed alias of Go does.
    fn set(&mut self, loc: &Loc, key: &[u8], o: Object) {
        if let Some(Cont::Dict(d)) = container(self.x, loc) {
            d.set(key, o);
        }
    }

    /// `DereferenceDict` of the entry `key` of the dictionary at `parent`: the location of the
    /// dictionary, `None` for a missing or null entry or one that refers to null.
    ///
    /// # Errors
    /// The errors of the dereference, including a type that is not a dictionary.
    fn sub_dict(&mut self, parent: &Loc, key: &[u8]) -> Result<Option<Loc>, Error> {
        match self.peek_find(parent, key) {
            None | Some(Peek::Null) => Ok(None),
            Some(Peek::Ref(r)) => {
                if self.x.dereference_dict_ref(r)?.is_some() {
                    Ok(Some(Loc::indirect(r.num)))
                } else {
                    Ok(None)
                }
            }
            Some(Peek::Dict) => Ok(Some(parent.key(key))),
            Some(_) => {
                // The error of a type that is not a dictionary.
                if let Some(o) = self.find(parent, key) {
                    self.x.dereference_dict(&o)?;
                }
                Ok(None)
            }
        }
    }

    /// The dictionary of the table entry `nr`.
    fn table_dict(&self, nr: i64) -> Option<Dict> {
        match &self.x.table.get(&nr)?.object {
            Some(EntryObject::Obj(Object::Dict(d))) => Some(d.clone()),
            _ => None,
        }
    }

    /// The stream of the table entry `nr`, sharing its buffers.
    fn table_stream(&self, nr: i64) -> Option<StreamDict> {
        match &self.x.table.get(&nr)?.object {
            Some(EntryObject::Obj(Object::Stream(sd))) => Some((**sd).clone()),
            _ => None,
        }
    }

    /// `keptObjectIndRef`: a reference to the object that stays, with its generation (7.3.10).
    fn kept_ref(&self, obj_nr: i64) -> Ref {
        let generation = self.x.find(obj_nr).and_then(|e| e.generation).unwrap_or(0);
        Ref {
            num: obj_nr,
            generation,
        }
    }

    /// `DeleteDictEntry` for the dictionary at `loc`: deletes the object graph the entry refers
    /// to, and the entry.
    ///
    /// # Errors
    /// [`Error::Cancelled`] and the errors of [`XRefTable::delete_object`].
    fn delete_dict_entry_at(&mut self, loc: &Loc, key: &[u8]) -> Result<(), Error> {
        self.check()?;
        let Some(o) = self.find(loc, key) else {
            return Ok(());
        };
        self.x.delete_object(&o, self.cancel)?;
        if let Some(Cont::Dict(d)) = container(self.x, loc) {
            d.delete(key);
        }
        Ok(())
    }

    /// `PageFonts[page_nr]`, made if there is none. Every call charges [`PAGE_SET_BYTES`] to the
    /// table's ledger charge, as a caller adds at most one number after it.
    fn page_fonts(&mut self, page_nr: i64) -> Result<&mut BTreeSet<i64>, Error> {
        if page_nr < 0 || page_nr >= self.o.page_slots {
            return Err(Error::Panic("index out of range"));
        }
        self.x.held.grow_by(PAGE_SET_BYTES)?;
        Ok(self.o.page_fonts.entry(page_nr).or_default())
    }

    /// `PageImages[page_nr]`, made if there is none; charged as [`Optimizer::page_fonts`].
    fn page_images(&mut self, page_nr: i64) -> Result<&mut BTreeSet<i64>, Error> {
        if page_nr < 0 || page_nr >= self.o.page_slots {
            return Err(Error::Panic("index out of range"));
        }
        self.x.held.grow_by(PAGE_SET_BYTES)?;
        Ok(self.o.page_images.entry(page_nr).or_default())
    }

    // -----------------------------------------------------------------------------------------
    // References to free objects

    /// `freeRefFix.nullObjNr`.
    fn null_obj_nr(&mut self, obj_nr: i64) -> Result<i64, Error> {
        if let Some(nr) = self.o.null_obj_nr {
            return Ok(nr);
        }
        let nr = self
            .x
            .insert_object(Object::Null)
            .map_err(|e| e.context(format!("insert null object for free obj#{obj_nr}")))?;
        self.o.null_obj_nr = Some(nr);
        Ok(nr)
    }

    /// `freeRefFix.fixValue`: repairs a reference to a free object (the new reference is the
    /// result) and schedules the entries of the object a reference resolves to, or of a direct
    /// dictionary or array.
    fn fix_value(
        &mut self,
        stack: &mut ObjectStack,
        i: usize,
        item: Peek,
    ) -> Result<Option<Object>, Error> {
        match item {
            Peek::Ref(ir) => {
                let obj_nr = ir.num;
                if self.o.cache.contains(&obj_nr) {
                    return Ok(None);
                }
                self.cache_held.grow_by(NR_BYTES)?;
                self.o.cache.insert(obj_nr);

                let Some(entry) = self.x.find(obj_nr) else {
                    return Ok(None);
                };
                if entry.free {
                    // This is a reference to a free object that needs to be fixed.
                    let nr = self.null_obj_nr(obj_nr)?;
                    return Ok(Some(Object::Ref(Ref {
                        num: nr,
                        generation: ir.generation,
                    })));
                }
                if matches!(
                    entry.object,
                    Some(EntryObject::Obj(
                        Object::Stream(_) | Object::Dict(_) | Object::Array(_)
                    ))
                ) {
                    stack.push(self.x, Loc::indirect(obj_nr), obj_nr)?;
                }
                Ok(None)
            }
            Peek::Dict | Peek::Array => {
                if let Some(loc) = stack.entry_loc(i) {
                    stack.push(self.x, loc, 0)?;
                }
                Ok(None)
            }
            Peek::Null | Peek::Other => Ok(None),
        }
    }

    /// `freeRefFix.run`.
    fn run_free_ref_fix(&mut self, stack: &mut ObjectStack) -> Result<(), Error> {
        while !stack.empty() {
            self.check()?;
            let Some((i, item)) = stack.next(self.x) else {
                continue;
            };
            match self.fix_value(stack, i, item) {
                Ok(Some(o)) => stack.replace(self.x, i, o),
                Ok(None) => {}
                Err(e) => {
                    // The path from the root to the entry of frame i that failed.
                    let path = error_path(i + 1, |j| stack.label(j));
                    return Err(e.context(path));
                }
            }
        }
        Ok(())
    }

    /// `fixReferencesToFreeObjects`: replaces references to free objects reachable from the root
    /// with references to one null object.
    fn fix_references_to_free_objects(&mut self) -> Result<(), Error> {
        self.check()?;
        let mut stack = ObjectStack::new(self.x.mem().clone());
        stack.push(self.x, Loc::root(), 0)?;
        self.run_free_ref_fix(&mut stack)
            .map_err(|e| e.context("root dict"))
    }

    // -----------------------------------------------------------------------------------------
    // Fonts

    /// `handleDuplicateFontObject`: the number of the registered font that `font_dict` (object
    /// `obj_nr`) duplicates, if there is one.
    fn handle_duplicate_font_object(
        &mut self,
        font_dict: &Dict,
        f_name: &[u8],
        r_name: &[u8],
        obj_nr: i64,
        page_nr: i64,
    ) -> Result<Option<i64>, Error> {
        // The registered fonts of this name, read in place: the loop does not register any.
        if !self.o.fonts.contains_key(f_name) {
            return Ok(None);
        }
        let this = Object::Dict(font_dict.clone());

        // Check if this font dict matches the font dict of each registered font object.
        for i in 0.. {
            let Some(&font_obj_nr) = self.o.fonts.get(f_name).and_then(|nrs| nrs.get(i)) else {
                break;
            };
            if font_obj_nr == obj_nr {
                continue;
            }
            if !self.o.font_objects.contains_key(&font_obj_nr) {
                continue;
            }
            let Some(registered) = self.table_dict(font_obj_nr) else {
                continue;
            };

            let same = equal_objects(self.x, &Object::Dict(registered), &this, &[], self.cancel)
                .map_err(|e| {
                    e.context(format!("compare font obj#{obj_nr} with obj#{font_obj_nr}"))
                })?;
            if !same {
                continue;
            }

            // We have detected a redundant font dict! The font for font object number is used
            // instead of obj_nr.
            self.page_fonts(page_nr)?.insert(font_obj_nr);

            // Add the resource name of this duplicate font to the registered resource names.
            self.held
                .grow_by(NAME_BYTES + r_name.len() as u64 + NR_BYTES)?;
            if let Some(fo) = self.o.font_objects.get_mut(&font_obj_nr) {
                fo.add_resource_name(r_name);
            }

            // Register the font dict as duplicate.
            self.o.duplicate_fonts.insert(obj_nr);
            return Ok(Some(font_obj_nr));
        }
        Ok(None)
    }

    /// `optimizeFontResourcesDict`: gets rid of the redundant fonts of the font resource
    /// dictionary at `rdict`.
    fn optimize_font_resources_dict(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        r_name_prefix: &[u8],
    ) -> Result<(), Error> {
        self.page_fonts(page_nr)?;
        let mut recorded_corrupt = false;

        // Iterate over the font resource dict.
        let (keys, _copy) = self.keys(rdict)?;
        for r_name in keys {
            self.check()?;
            let Some(v) = self.entry(rdict, &r_name) else {
                continue;
            };
            let ir = match v {
                Peek::Null => {
                    // A font id with a missing font dict reference.
                    if !recorded_corrupt {
                        self.held.grow_by(loc_bytes(rdict))?;
                        self.o.corrupt_font_res_dicts.push(rdict.clone());
                    }
                    recorded_corrupt = true;
                    continue;
                }
                Peek::Ref(ir) => ir,
                _ => continue,
            };
            let obj_nr = ir.num;
            let q_name = qualified_rname(r_name_prefix, &r_name);

            if self.o.font_objects.contains_key(&obj_nr) {
                // This font has already been registered.
                self.page_fonts(page_nr)?.insert(obj_nr);
                continue;
            }

            // We are dealing with a new font. Relaxed validation takes a font dictionary that
            // does not resolve as no font.
            let font_dict = match self.x.dereference_font_dict(ir) {
                Ok(Some(d)) => d.clone(),
                _ => continue,
            };

            // Get the unique font name.
            let (prefix, f_name) = font_name::name(self.x, &font_dict, obj_nr).map_err(|e| {
                e.context(format!(
                    "font resource {} obj#{obj_nr}: parse font name",
                    lossy(&q_name)
                ))
            })?;

            // Check if the font dict is a duplicate and if so get the number of the original.
            let original = self
                .handle_duplicate_font_object(&font_dict, &f_name, &q_name, obj_nr, page_nr)
                .map_err(|e| {
                    e.context(format!(
                        "font resource {} obj#{obj_nr}: duplicate check",
                        lossy(&q_name)
                    ))
                })?;

            if let Some(original) = original {
                // We have identified a redundant font dict! Update the font resource dict so
                // that r_name points to the original.
                let kept = self.kept_ref(original);
                self.set(rdict, &r_name, Object::Ref(kept));
                self.x.increment_ref_count(kept);
                continue;
            }

            register_font_dict_obj_nr(self.o, &mut self.held, &f_name, obj_nr)?;
            let font_obj = new_font_object(self.x, &font_dict, &q_name, &prefix, &f_name);
            self.held.grow_by(font_object_bytes(&font_obj))?;
            self.o.font_objects.insert(obj_nr, font_obj);
            self.page_fonts(page_nr)?.insert(obj_nr);
        }
        Ok(())
    }

    /// `fixCorruptFontResDicts`: a font resource with a null value gets the first registered
    /// Arial font.
    fn fix_corrupt_font_res_dicts(&mut self) -> Result<(), Error> {
        // One location is copied at a time; the loop adds none.
        for i in 0..self.o.corrupt_font_res_dicts.len() {
            let Some(loc) = self.o.corrupt_font_res_dicts.get(i).cloned() else {
                break;
            };
            let (keys, _copy) = self.keys(&loc)?;
            for k in keys {
                if self.entry(&loc, &k) != Some(Peek::Null) {
                    continue;
                }
                let found = self
                    .o
                    .fonts
                    .iter()
                    .find(|(name, _)| {
                        name.starts_with(b"Arial") && name.get(5).is_none_or(|&c| c != b'-')
                    })
                    .map(|(_, nrs)| nrs.first().copied());
                if let Some(first) = found {
                    let nr = first.ok_or(Error::Panic("index out of range"))?;
                    let kept = self.kept_ref(nr);
                    self.set(&loc, &k, Object::Ref(kept));
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Images

    /// `handleDuplicateImageObject`: the number of the registered image that `image_dict`
    /// (object `obj_nr`) duplicates, if there is one, and whether it is registered already.
    fn handle_duplicate_image_object(
        &mut self,
        image_dict: &StreamDict,
        resource_name: &[u8],
        obj_nr: i64,
        page_nr: i64,
    ) -> Result<(Option<i64>, bool), Error> {
        self.page_images(page_nr)?;

        if let Some(&new_obj_nr) = self.o.duplicate_images.get(&obj_nr) {
            // We have detected a redundant image dict. The image for new_obj_nr is used instead
            // of obj_nr.
            self.page_images(page_nr)?.insert(new_obj_nr);

            // Add the resource name of this duplicate image to the registered resource names.
            self.held.grow_by(image_name_bytes(resource_name))?;
            let io = self
                .o
                .image_objects
                .get_mut(&new_obj_nr)
                .ok_or(Error::Panic("nil image object"))?;
            io.add_resource_name(page_nr, resource_name);
            return Ok((Some(new_obj_nr), false));
        }

        if self.o.image_objects.contains_key(&obj_nr) {
            self.held.grow_by(image_name_bytes(resource_name))?;
        }
        if let Some(io) = self.o.image_objects.get_mut(&obj_nr) {
            io.add_resource_name(page_nr, resource_name);
            return Ok((None, true));
        }

        // Process the image dict, check if this is a duplicate.
        let h = sha256(image_dict.raw.as_deref().unwrap_or(&[]));
        let this = Object::Stream(Box::new(image_dict.clone()));
        // The candidates are read in place: the loop does not register any.
        for i in 0.. {
            let Some(&image_obj_nr) = self.o.image_object_hashes.get(&h).and_then(|v| v.get(i))
            else {
                break;
            };
            if !self.o.image_objects.contains_key(&image_obj_nr) {
                return Err(Error::Panic("nil image object"));
            }
            let Some(registered) = self.table_stream(image_obj_nr) else {
                continue;
            };

            // Check if the input image dict matches the image dict of this image object.
            let same = equal_objects(
                self.x,
                &Object::Stream(Box::new(registered)),
                &this,
                &[],
                self.cancel,
            )
            .map_err(|e| {
                e.context(format!(
                    "compare image obj#{obj_nr} with obj#{image_obj_nr}"
                ))
            })?;
            if !same {
                continue;
            }

            // We have detected a redundant image dict. The image for image_obj_nr is used
            // instead of obj_nr.
            self.page_images(page_nr)?.insert(image_obj_nr);
            self.held
                .grow_by(image_name_bytes(resource_name) + NR_BYTES)?;
            if let Some(io) = self.o.image_objects.get_mut(&image_obj_nr) {
                io.add_resource_name(page_nr, resource_name);
            }

            // Register the image dict as duplicate.
            self.o.duplicate_images.insert(obj_nr, image_obj_nr);
            return Ok((Some(image_obj_nr), false));
        }

        let key = if self.o.image_object_hashes.contains_key(&h) {
            0
        } else {
            2 * (32 + size_of::<Vec<i64>>() as u64) + 16
        };
        self.held.grow_by(key + NR_BYTES)?;
        self.o
            .image_object_hashes
            .entry(h)
            .or_default()
            .push(obj_nr);
        Ok((None, false))
    }

    /// `optimizeXObjectImage`.
    fn optimize_xobject_image(
        &mut self,
        sd: &StreamDict,
        r_name_prefix: &[u8],
        r_name: &[u8],
        rdict: &Loc,
        obj_nr: i64,
        page_nr: i64,
    ) -> Result<(), Error> {
        let q_name = qualified_rname(r_name_prefix, r_name);

        // Check if the image is a duplicate and if so get the number of the original.
        let (original, already_duplicate) = self
            .handle_duplicate_image_object(sd, &q_name, obj_nr, page_nr)
            .map_err(|e| {
                e.context(format!(
                    "image resource {} obj#{obj_nr}: duplicate check",
                    lossy(&q_name)
                ))
            })?;

        if let Some(original) = original {
            // We have identified a redundant image! Update the xobject resource dict so that
            // r_name points to the original.
            let kept = self.kept_ref(original);
            self.x.increment_ref_count(kept);
            self.set(rdict, r_name, Object::Ref(kept));
            return Ok(());
        }

        if !already_duplicate {
            // Register the new image dict.
            self.held.grow_by(
                2 * (8 + size_of::<ImageObject>() as u64) + 16 + image_name_bytes(&q_name),
            )?;
            self.o.image_objects.insert(
                obj_nr,
                ImageObject {
                    resource_names: BTreeMap::from([(page_nr, q_name)]),
                },
            );
        }

        self.page_images(page_nr)?.insert(obj_nr);
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Forms

    /// `optimizeXObjectForm`: the reference to the form XObject that the form `obj_nr`
    /// duplicates, if there is one, which gets one more reference.
    fn optimize_xobject_form(&mut self, obj_nr: i64) -> Result<Option<Ref>, Error> {
        if self.o.form_stream_cache.is_empty() {
            self.held.grow_by(NR_BYTES)?;
            self.o.form_stream_cache.insert(obj_nr);
            return Ok(None);
        }

        if self.o.form_stream_cache.contains(&obj_nr) {
            return Ok(None);
        }

        let length = |op: &Optimizer<'_>, nr: i64| op.table_stream(nr).map(|sd| sd.stream_length);
        let Some(this_length) = length(&*self, obj_nr) else {
            return Ok(None);
        };
        let this_length = this_length.ok_or(Error::Panic("nil StreamLength"))?;

        // The cached forms of the same length, in object number order, charged for all of them.
        let n = self.o.form_stream_cache.len();
        let _cached = self.x.mem().charge((n as u64).saturating_mul(8))?;
        let mut cached = Vec::new();
        cached.try_reserve_exact(n)?;
        for &nr in &self.o.form_stream_cache {
            let Some(l) = length(&*self, nr) else {
                continue;
            };
            if l.ok_or(Error::Panic("nil StreamLength"))? == this_length {
                cached.push(nr);
            }
        }
        if cached.is_empty() {
            self.held.grow_by(NR_BYTES)?;
            self.o.form_stream_cache.insert(obj_nr);
            return Ok(None);
        }

        for nr in cached {
            let (Some(a), Some(b)) = (self.table_stream(obj_nr), self.table_stream(nr)) else {
                continue;
            };
            let same = equal_objects(
                self.x,
                &Object::Stream(Box::new(a)),
                &Object::Stream(Box::new(b)),
                &[],
                self.cancel,
            )
            .map_err(|e| e.context(format!("compare form XObject obj#{obj_nr} with obj#{nr}")))?;
            if same {
                let kept = self.kept_ref(nr);
                self.x.increment_ref_count(kept);
                return Ok(Some(kept));
            }
        }

        self.held.grow_by(NR_BYTES)?;
        self.o.form_stream_cache.insert(obj_nr);
        Ok(None)
    }

    /// `formResourcesVisited`: whether the resources of the form `obj_nr` were processed for
    /// the page; marks them as processed. Every call charges a page set and a number.
    fn form_resources_visited(&mut self, page_nr: i64, obj_nr: i64) -> Result<bool, Error> {
        self.held.grow_by(PAGE_SET_BYTES + NR_BYTES)?;
        Ok(!self
            .o
            .form_resource_cache
            .entry(page_nr)
            .or_default()
            .insert(obj_nr))
    }

    /// `optimizeFormResources`: optimizes the resources of the form at `form`.
    fn optimize_form_resources(
        &mut self,
        form: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        let d = self.sub_dict(form, b"Resources").map_err(|e| {
            e.context(format!(
                "form resource {}: dereference resources",
                lossy(r_name)
            ))
        })?;
        if let Some(d) = d {
            // Optimize image and font resources.
            self.optimize_resources(&d, page_nr, page_obj_nr, r_name, vis)
                .map_err(|e| {
                    e.context(format!(
                        "form resource {}: optimize resources",
                        lossy(r_name)
                    ))
                })?;
        }
        Ok(())
    }

    /// `optimizeForm`: the form XObject `obj_nr`, which `r_name` of the dictionary at `rdict`
    /// refers to.
    fn optimize_form(
        &mut self,
        obj_nr: i64,
        r_name_prefix: &[u8],
        r_name: &[u8],
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        vis: &mut Visited,
    ) -> Result<(), Error> {
        self.check()?;
        let q_name = qualified_rname(r_name_prefix, r_name);
        let duplicate = self.optimize_xobject_form(obj_nr).map_err(|e| {
            e.context(format!(
                "form XObject {} obj#{obj_nr}: optimize usage",
                lossy(&q_name)
            ))
        })?;

        if let Some(kept) = duplicate {
            self.set(rdict, r_name, Object::Ref(kept));
            return Ok(());
        }

        if self.form_resources_visited(page_nr, obj_nr)? {
            return Ok(());
        }

        let form = Loc::indirect(obj_nr);
        let Some(res) = self.peek_find(&form, b"Resources") else {
            return Ok(());
        };

        let mark = vis.mark();
        if let Peek::Ref(r) = res {
            if vis.contains(r) {
                return Ok(());
            }
            vis.push(r)?;
        }
        let result = self.optimize_form_resources(&form, page_nr, page_obj_nr, &q_name, vis);
        vis.reset(mark);
        result
    }

    // -----------------------------------------------------------------------------------------
    // Graphics states

    /// `optimizeSMaskResources`: the soft mask `dict` of a graphics state. The image or form
    /// that its `G` refers to is optimized, and a duplicate is entered as `G` into the graphics
    /// state dictionary at `rdict`, as Go does.
    fn optimize_smask_resources(
        &mut self,
        dict: &Dict,
        vis: &mut Visited,
        r_name_prefix: &[u8],
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
    ) -> Result<(), Error> {
        self.check()?;
        let Some(ir) = dict.indirect_ref_entry(b"G") else {
            return Ok(());
        };

        if vis.contains(ir) {
            return Ok(());
        }

        // Go appends a pointer to the reference here. No lookup compares equal to a pointer, so
        // nothing is added.

        let obj_nr = ir.num;
        let Some(sd) = self
            .x
            .dereference_xobject_dict(ir)
            .map_err(|e| e.context(format!("SMask G obj#{obj_nr}: dereference XObject")))?
        else {
            return Ok(());
        };

        let (subtype, _) = self
            .x
            .dereference_name_entry(&sd.dict, b"Subtype")
            .map_err(|e| e.context(format!("SMask G obj#{obj_nr}: Subtype")))?;
        let subtype = match subtype {
            Some(s) if !s.is_empty() => s,
            // An unclassifiable XObject.
            _ => return Ok(()),
        };

        if subtype == b"Image" {
            self.optimize_xobject_image(&sd, r_name_prefix, b"G", rdict, obj_nr, page_nr)
                .map_err(|e| e.context("SMask G image"))?;
        }

        if subtype == b"Form" {
            self.optimize_form(
                obj_nr,
                r_name_prefix,
                b"G",
                rdict,
                page_nr,
                page_obj_nr,
                vis,
            )
            .map_err(|e| e.context("SMask G form"))?;
        }

        Ok(())
    }

    /// `optimizeExtGStateResources`: the graphics state dictionary at `rdict`.
    fn optimize_ext_g_state_resources(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        self.page_images(page_nr)?;

        if self.peek_find(rdict, b"SMask") == Some(Peek::Dict)
            && let Some(Object::Dict(dict)) = self.find(rdict, b"SMask")
        {
            self.optimize_smask_resources(&dict, vis, r_name_prefix, rdict, page_nr, page_obj_nr)
                .map_err(|e| e.context("SMask"))?;
        }
        Ok(())
    }

    /// `optimizeExtGStateResourcesDict`: the `ExtGState` resource dictionary at `rdict`.
    fn optimize_ext_g_state_resources_dict(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        let mark = vis.mark();
        let (keys, _copy) = self.keys(rdict)?;
        for r_name in keys {
            self.check()?;
            let Some(Peek::Ref(ir)) = self.entry(rdict, &r_name) else {
                continue;
            };

            if vis.contains(ir) {
                continue;
            }
            vis.push(ir)?;

            let obj_nr = ir.num;
            let q_name = qualified_rname(r_name_prefix, &r_name);

            let state = self
                .x
                .dereference_dict_ref(ir)
                .map_err(|e| {
                    e.context(format!(
                        "ExtGState resource {} obj#{obj_nr}: dereference dict",
                        lossy(&q_name)
                    ))
                })?
                .is_some();
            if !state {
                continue;
            }

            self.optimize_ext_g_state_resources(
                &Loc::indirect(obj_nr),
                page_nr,
                page_obj_nr,
                &q_name,
                vis,
            )
            .map_err(|e| {
                e.context(format!(
                    "ExtGState resource {} obj#{obj_nr}: optimize resources",
                    lossy(&q_name)
                ))
            })?;
        }
        vis.reset(mark);
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // XObjects

    /// `optimizeXObjectResource`: the XObject `sd` (object `obj_nr`) that `r_name` refers to.
    fn optimize_xobject_resource(
        &mut self,
        sd: &StreamDict,
        rdict: &Loc,
        r_name_prefix: &[u8],
        r_name: &[u8],
        q_name: &[u8],
        obj_nr: i64,
        page_nr: i64,
        page_obj_nr: i64,
        vis: &mut Visited,
    ) -> Result<(), Error> {
        let (subtype, _) = self
            .x
            .dereference_name_entry(&sd.dict, b"Subtype")
            .map_err(|e| {
                e.context(format!(
                    "XObject resource {} obj#{obj_nr}: Subtype",
                    lossy(q_name)
                ))
            })?;
        let subtype = match subtype {
            Some(s) if !s.is_empty() => s,
            // An unclassifiable XObject.
            _ => return Ok(()),
        };

        if subtype == b"Image" {
            self.optimize_xobject_image(sd, r_name_prefix, r_name, rdict, obj_nr, page_nr)
                .map_err(|e| {
                    e.context(format!(
                        "XObject resource {} obj#{obj_nr}: image",
                        lossy(q_name)
                    ))
                })?;
        }

        if subtype == b"Form" {
            // Get rid of the PieceInfo dict of form XObjects.
            self.delete_dict_entry_at(&Loc::indirect(obj_nr), b"PieceInfo")
                .map_err(|e| {
                    e.context(format!(
                        "XObject resource {} obj#{obj_nr}: delete PieceInfo",
                        lossy(q_name)
                    ))
                })?;
            self.optimize_form(
                obj_nr,
                r_name_prefix,
                r_name,
                rdict,
                page_nr,
                page_obj_nr,
                vis,
            )
            .map_err(|e| {
                e.context(format!(
                    "XObject resource {} obj#{obj_nr}: form",
                    lossy(q_name)
                ))
            })?;
        }

        Ok(())
    }

    /// `optimizeXObjectResourcesDict`: the `XObject` resource dictionary at `rdict`.
    fn optimize_xobject_resources_dict(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        self.page_images(page_nr)?;

        let mark = vis.mark();
        let (keys, _copy) = self.keys(rdict)?;
        for r_name in keys {
            self.check()?;
            let Some(Peek::Ref(ir)) = self.entry(rdict, &r_name) else {
                continue;
            };

            if vis.contains(ir) {
                continue;
            }
            vis.push(ir)?;

            let obj_nr = ir.num;
            let q_name = qualified_rname(r_name_prefix, &r_name);

            let sd = self.x.dereference_xobject_dict(ir).map_err(|e| {
                e.context(format!(
                    "XObject resource {} obj#{obj_nr}: dereference XObject",
                    lossy(&q_name)
                ))
            })?;
            let Some(sd) = sd else {
                continue;
            };

            self.optimize_xobject_resource(
                &sd,
                rdict,
                r_name_prefix,
                &r_name,
                &q_name,
                obj_nr,
                page_nr,
                page_obj_nr,
                vis,
            )?;
        }
        vis.reset(mark);
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Resources

    /// `processFontResources`.
    fn process_font_resources(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
    ) -> Result<(), Error> {
        let d = self
            .sub_dict(rdict, b"Font")
            .map_err(|e| e.context("font resources: dereference dict"))?;
        let Some(d) = d else {
            return Err(Error::Corrupt(format!(
                "font resource dict is null for page {page_nr} pageObj {page_obj_nr}"
            )));
        };
        self.optimize_font_resources_dict(&d, page_nr, r_name_prefix)
            .map_err(|e| e.context("font resources: optimize dict"))
    }

    /// `processXObjectResources`.
    fn process_xobject_resources(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        let d = self
            .sub_dict(rdict, b"XObject")
            .map_err(|e| e.context("XObject resources: dereference dict"))?;
        let Some(d) = d else {
            return Err(Error::Corrupt(format!(
                "xObject resource dict is null for page {page_nr} pageObj {page_obj_nr}"
            )));
        };
        self.optimize_xobject_resources_dict(&d, page_nr, page_obj_nr, r_name_prefix, vis)
            .map_err(|e| e.context("XObject resources: optimize dict"))
    }

    /// `processExtGStateResources`.
    fn process_ext_g_state_resources(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        let d = self
            .sub_dict(rdict, b"ExtGState")
            .map_err(|e| e.context("ExtGState resources: dereference dict"))?;
        let Some(d) = d else {
            return Err(match self.peek_find(rdict, b"ExtGState") {
                Some(Peek::Ref(r)) => {
                    Error::Corrupt(format!("resource dict obj#{} is null", r.num))
                }
                _ => Error::Corrupt("resource dict is null".to_string()),
            });
        };
        self.optimize_ext_g_state_resources_dict(&d, page_nr, page_obj_nr, r_name_prefix, vis)
            .map_err(|e| e.context("ExtGState resources: optimize dict"))
    }

    /// `optimizeResources`: optimizes the resource dictionary at `rdict` by removing redundant
    /// fonts and images.
    fn optimize_resources(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        self.check()?;
        self.depth = self.depth.saturating_add(1);
        let result = self
            .x
            .check_recursion_depth("optimize resources", self.depth)
            .and_then(|()| {
                self.optimize_resources_at_depth(rdict, page_nr, page_obj_nr, r_name_prefix, vis)
            });
        self.depth = self.depth.saturating_sub(1);
        result
    }

    fn optimize_resources_at_depth(
        &mut self,
        rdict: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
        r_name_prefix: &[u8],
        vis: &mut Visited,
    ) -> Result<(), Error> {
        if self.peek_find(rdict, b"Font").is_some() {
            // Process the Font resource dict, get rid of redundant fonts.
            self.process_font_resources(rdict, page_nr, page_obj_nr, r_name_prefix)
                .map_err(|e| e.context("Font"))?;
        }

        if self.peek_find(rdict, b"XObject").is_some() {
            // Process the XObject resource dict, get rid of redundant images.
            self.process_xobject_resources(rdict, page_nr, page_obj_nr, r_name_prefix, vis)
                .map_err(|e| e.context("XObject"))?;
        }

        if self.peek_find(rdict, b"ExtGState").is_some() {
            // An ExtGState resource dict may contain binary content in the entries SMask, HT.
            self.process_ext_g_state_resources(rdict, page_nr, page_obj_nr, r_name_prefix, vis)
                .map_err(|e| e.context("ExtGState"))?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // The page tree

    /// `parseResourcesDict`: processes the resources dictionary of the page at `page`.
    fn parse_resources_dict(
        &mut self,
        page: &Loc,
        page_nr: i64,
        page_obj_nr: i64,
    ) -> Result<(), Error> {
        if self.o.cache.contains(&page_obj_nr) {
            return Ok(());
        }
        self.cache_held.grow_by(NR_BYTES)?;
        self.o.cache.insert(page_obj_nr);

        // The logical page number is page_nr+1. The dictionary is none for inherited resources.
        let logical = page_nr.saturating_add(1);
        let d = self
            .sub_dict(page, b"Resources")
            .map_err(|e| e.context(format!("page {logical} obj#{page_obj_nr}: resources dict")))?;

        if let Some(d) = d {
            // Optimize image and font resources.
            let mut vis = Visited::new(self.x.mem());
            self.optimize_resources(&d, page_nr, page_obj_nr, b"", &mut vis)
                .map_err(|e| {
                    e.context(format!(
                        "page {logical} obj#{page_obj_nr}: optimize resources"
                    ))
                })?;
        }
        Ok(())
    }

    /// `parsePageTreeKid`: returns the number of the next page.
    fn parse_page_tree_kid(
        &mut self,
        v: &Object,
        kid_nr: usize,
        page_nr: i64,
        depth: u64,
    ) -> Result<i64, Error> {
        self.check()?;
        if v.is_null() {
            return Ok(page_nr);
        }

        let Object::Ref(ir) = v else {
            return Err(Error::Corrupt(format!(
                "kid {kid_nr}: expected indirect reference, got {}",
                type_name(v)
            )));
        };
        let obj_nr = ir.num;

        self.x
            .dereference_page_node_dict(*ir)
            .map_err(|e| e.context(format!("kid {kid_nr} obj#{obj_nr}: dereference page node")))?;
        let node = Loc::indirect(obj_nr);

        let page_type = self.find(&node, b"Type");
        let (page_type, _) = self
            .x
            .name_from_value(b"Type", page_type.as_ref())
            .map_err(|e| e.context(format!("kid {kid_nr} obj#{obj_nr}: page node Type")))?;
        if matches!(page_type.as_deref(), Some(b"Pages")) {
            return self
                .parse_pages_dict(&node, page_nr, depth.saturating_add(1))
                .map_err(|e| e.context(format!("kid {kid_nr} pages obj#{obj_nr}")));
        }

        let logical = page_nr.saturating_add(1);
        self.delete_dict_entry_at(&node, b"PieceInfo")
            .map_err(|e| e.context(format!("page {logical} obj#{obj_nr}: delete PieceInfo")))?;

        self.parse_resources_dict(&node, page_nr, obj_nr)?;

        Ok(page_nr.saturating_add(1))
    }

    /// `parsePagesDict`: iterates over all pages below the page tree node at `pages` and
    /// optimizes their resources. Returns the number of the next page.
    fn parse_pages_dict(&mut self, pages: &Loc, page_nr: i64, depth: u64) -> Result<i64, Error> {
        self.check()?;
        self.x.check_recursion_depth("page tree", depth)?;

        if self.peek_find(pages, b"Count").is_none() {
            return Err(Error::Corrupt("parsePagesDict: missing Count".to_string()));
        }

        self.o.cache.clear();
        self.cache_held.shrink_to(0);

        // Iterate over the page tree. Both copies of the kids are charged to the ledger.
        let mem = self.x.mem().clone();
        let n = match container(self.x, pages) {
            Some(Cont::Dict(d)) => d.find(b"Kids").map_or(0, Object::copy_bytes),
            _ => 0,
        };
        let _entry_copy = mem.charge(n)?;
        let Some(o) = self.find(pages, b"Kids") else {
            return Err(Error::Corrupt("corrupt \"Kids\" entry".to_string()));
        };

        let (kids, _copy) = match self.x.dereference_array(&o) {
            Err(e) => return Err(e.context("dereference Kids")),
            Ok(None) => return Err(Error::Corrupt("corrupt \"Kids\" entry".to_string())),
            Ok(Some(a)) => {
                let held = mem.charge(array_copy_bytes(a))?;
                (a.clone(), held)
            }
        };

        let mut page_nr = page_nr;
        for (i, v) in kids.iter().enumerate() {
            page_nr = self.parse_page_tree_kid(v, i.saturating_add(1), page_nr, depth)?;
        }
        Ok(page_nr)
    }

    // -----------------------------------------------------------------------------------------
    // Redundant objects

    /// `calcRedundantObjects`: identifies and marks all potential duplicate objects.
    fn calc_redundant_objects(&mut self) -> Result<(), Error> {
        self.check()?;

        // The sets are walked in place, one number after the other; the loops add to neither.
        // What the persisting sets gain is charged to the table's charge.
        let mut next = self.o.duplicate_fonts.first().copied();
        while let Some(i) = next {
            next = self
                .o
                .duplicate_fonts
                .range((Excluded(i), Unbounded))
                .next()
                .copied();
            self.check()?;
            if !self.o.duplicate_font_objs.contains(&i) {
                self.x.held.grow_by(NR_BYTES)?;
            }
            self.o.duplicate_font_objs.insert(i);
            // Identify and mark all involved potential duplicate objects for a redundant font.
            let Some(font_dict) = self.table_dict(i) else {
                continue;
            };
            DuplicateTraversal {
                x: self.x,
                objects: &mut self.o.duplicate_font_objs,
                cancel: self.cancel,
            }
            .traverse(&Object::Dict(font_dict), 0)
            .map_err(|e| e.context(format!("duplicate font obj#{i}: traverse object graph")))?;
        }

        let mut next = self.o.duplicate_images.keys().next().copied();
        while let Some(i) = next {
            next = self
                .o
                .duplicate_images
                .range((Excluded(i), Unbounded))
                .next()
                .map(|(&k, _)| k);
            self.check()?;
            if !self.o.duplicate_image_objs.contains(&i) {
                self.x.held.grow_by(NR_BYTES)?;
            }
            self.o.duplicate_image_objs.insert(i);
            // Identify and mark all involved potential duplicate objects for a redundant image.
            let Some(image_dict) = self.table_stream(i) else {
                continue;
            };
            DuplicateTraversal {
                x: self.x,
                objects: &mut self.o.duplicate_image_objs,
                cancel: self.cancel,
            }
            .traverse(&Object::Stream(Box::new(image_dict)), 0)
            .map_err(|e| e.context(format!("duplicate image obj#{i}: traverse object graph")))?;
        }
        Ok(())
    }

    /// `optimizeFontAndImages`: iterates over all pages and optimizes the resources, to get rid
    /// of duplicate embedded fonts and images.
    fn optimize_font_and_images(&mut self) -> Result<(), Error> {
        self.check()?;

        // The reference to the page tree root dict.
        let pages = self.x.pages().map_err(|e| e.context("page tree root"))?;

        // Dereference the page tree root dict.
        self.x.dereference_dict_ref(pages).map_err(|e| {
            e.context(format!(
                "page tree root obj#{}: dereference dict",
                pages.num
            ))
        })?;

        // Prepare the optimization environment.
        self.o.page_slots = self.x.page_count;
        self.o.page_fonts.clear();
        self.o.page_images.clear();
        self.o.form_resource_cache.clear();

        // Iterate over the page dicts and optimize the resources.
        self.parse_pages_dict(&Loc::indirect(pages.num), 0, 0)
            .map_err(|e| e.context("page tree"))?;

        self.fix_corrupt_font_res_dicts()
            .map_err(|e| e.context("fix corrupt font resources"))?;
        self.check()?;

        self.o.form_stream_cache.clear();

        // Identify all duplicate objects.
        self.calc_redundant_objects()
            .map_err(|e| e.context("calculate redundant objects"))
    }

    /// `ensureDirectWidthForXObjs`: the `Width` of every image that a page uses becomes direct.
    fn ensure_direct_width_for_xobjs(&mut self) -> Result<(), Error> {
        let n: usize = self.o.page_images.values().map(BTreeSet::len).sum();
        let _nrs = self.x.mem().charge((n as u64).saturating_mul(8))?;
        let mut nrs = Vec::new();
        nrs.try_reserve_exact(n)?;
        for imgs in self.o.page_images.values() {
            nrs.extend(imgs.iter().copied());
        }
        for obj_nr in nrs {
            if !self.o.image_objects.contains_key(&obj_nr) {
                return Err(Error::Panic("nil image object"));
            }
            self.resolve_width(obj_nr)
                .map_err(|e| e.context(format!("image obj#{obj_nr}: resolve width")))?;
        }
        Ok(())
    }

    /// `resolveWidth`: the `Width` of the image stream `obj_nr`, made direct.
    fn resolve_width(&mut self, obj_nr: i64) -> Result<(), Error> {
        let loc = Loc::indirect(obj_nr);
        if let Some(o) = self.find(&loc, b"Width") {
            let w = self
                .x
                .dereference_number(&o)
                .map_err(|e| e.context("dereference Width"))?;
            self.set(&loc, b"Width", Object::Integer(w as i64));
        }
        Ok(())
    }
}

/// `duplicateObjectTraversal`: marks the objects that an object graph reaches.
struct DuplicateTraversal<'a> {
    x: &'a mut XRefTable,
    /// The objects marked so far, which the traversal does not enter again.
    objects: &'a mut BTreeSet<i64>,
    cancel: &'a AtomicBool,
}

impl DuplicateTraversal<'_> {
    /// `traverseIndirect`.
    fn traverse_indirect(&mut self, r: Ref, depth: u64) -> Result<(), Error> {
        let obj_nr = r.num;
        if self.objects.contains(&obj_nr) {
            return Ok(());
        }
        let depth = depth.saturating_add(1);
        self.x
            .check_recursion_depth("duplicate object graph", depth)
            .map_err(|e| e.context(format!("obj#{obj_nr}")))?;
        self.x.held.grow_by(NR_BYTES)?;
        self.objects.insert(obj_nr);
        let target = Object::Ref(r);
        let o = self
            .x
            .deref_c(&target, self.cancel)
            .map_err(|e| e.context(format!("obj#{obj_nr}: dereference duplicate graph object")))?
            .clone();
        self.walk(&o, depth)
            .map_err(|e| e.context(format!("obj#{obj_nr}: traverse duplicate graph object")))
    }

    /// `traverse`.
    fn traverse(&mut self, value: &Object, depth: u64) -> Result<(), Error> {
        check(self.cancel)?;
        match value {
            Object::Ref(r) => self.traverse_indirect(*r, depth),
            Object::Dict(_) | Object::Stream(_) | Object::Array(_) => self.walk(value, depth),
            _ => Ok(()),
        }
    }

    /// `walk`.
    fn walk(&mut self, obj: &Object, depth: u64) -> Result<(), Error> {
        check(self.cancel)?;
        match obj {
            Object::Dict(d) => self
                .traverse_dict(d, depth)
                .map_err(|e| e.context("dict entry")),
            Object::Stream(sd) => self
                .traverse_dict(&sd.dict, depth)
                .map_err(|e| e.context("stream dict entry")),
            Object::Array(a) => {
                for (i, value) in a.iter().enumerate() {
                    self.traverse(value, depth)
                        .map_err(|e| e.context(format!("array[{i}]")))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `traverseDict`: the entries in key order.
    fn traverse_dict(&mut self, d: &Dict, depth: u64) -> Result<(), Error> {
        for (key, value) in d.iter() {
            self.traverse(value, depth)
                .map_err(|e| e.context(format!("entry {}", lossy(key))))?;
        }
        check(self.cancel)
    }
}

#[cfg(test)]
mod tests;
