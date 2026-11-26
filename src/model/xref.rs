//! The cross-reference table (Go: pkg/pdfcpu/model/xreftable.go:1-1000, :1205-1340, :2180-2345):
//! the entries, find and insert (with the free list), the validity flags, the catalog accessors,
//! the limit charges and the cumulative decode budget.
//!
//! Go's `XRefTable` holds `*XRefTableEntry` pointers and `types.Dict` maps, so what a caller
//! writes through them persists. Here the table owns its entries; the `*_mut` accessors give
//! the same write access, and `dereference_stream_dict` returns a clone that shares its
//! buffers, as Go's struct copy does.
//!
//! Go's validation mode is always relaxed here (docs/design.md section 6), so
//! `XRefTable.ValidationMode` is not a field and every branch on it is taken as Go takes it in
//! relaxed mode.

use super::context::Version;
use super::deref::{NEVER, go_v, type_name};
use super::names::Node;
use crate::config::{Config, Limits};
use crate::error::{Error, LimitKind, Sentinel};
use crate::ledger::{Bytes, Held, Mem};
use crate::object::{
    Array, Dict, FREE_HEAD_GENERATION, LazyRef, Object, ObjectStreamDict, PdfFilter, Ref,
    StreamDict, XRefStreamDict, entry_copy_bytes,
};
use crate::parse::ParsedCharge;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// The filter name of `FlateDecode` (pkg/filter/filter.go:34).
const FLATE: &[u8] = b"FlateDecode";

/// Go dereferences a nil pointer here and panics; Go's callers recover it.
fn nil_deref() -> Error {
    Error::Panic("nil pointer dereference")
}

/// An `int64` limit from an unsigned field, clamped to Go's maximum.
fn lim(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// What an [`Entry`] holds: Go's `XRefTableEntry.Object`, which is an interface.
#[derive(Debug, Clone)]
pub enum EntryObject {
    /// Any object but the two stream variants below.
    Obj(Object),
    /// A `types.ObjectStreamDict`.
    ObjStm(Box<ObjectStreamDict>),
    /// A `types.XRefStreamDict`.
    XRefStm(Box<XRefStreamDict>),
    /// A `types.LazyObjectStreamObject`: an object of an object stream, not parsed yet. The
    /// stream is the `ObjStm` entry of `LazyRef::stm`.
    Lazy(LazyRef),
}

/// An entry of the cross-reference table (Go `XRefTableEntry`): a free object, a compressed
/// object or an in-use object.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub free: bool,
    /// Go's `*int64`; `None` is nil.
    pub offset: Option<i64>,
    /// Go's `*int`; `None` is nil.
    pub generation: Option<i64>,
    pub incr: i64,
    pub ref_count: i64,
    /// `None` is Go's nil object: not loaded yet.
    pub object: Option<EntryObject>,
    pub compressed: bool,
    pub object_stream: Option<i64>,
    pub object_stream_ind: Option<i64>,
    pub valid: bool,
    pub being_validated: bool,
}

impl Entry {
    /// `NewXRefTableEntryGen0`.
    pub fn gen0(obj: Object) -> Entry {
        Entry {
            generation: Some(0),
            object: Some(EntryObject::Obj(obj)),
            ..Entry::default()
        }
    }

    /// `NewFreeHeadXRefTableEntry`: object 0, the head of the free list.
    pub fn free_head() -> Entry {
        Entry {
            free: true,
            generation: Some(FREE_HEAD_GENERATION),
            offset: Some(0),
            ..Entry::default()
        }
    }
}

/// `Enc`: all defined encryption attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Enc {
    pub o: Vec<u8>,
    pub u: Vec<u8>,
    pub oe: Vec<u8>,
    pub ue: Vec<u8>,
    pub perms: Vec<u8>,
    pub l: i64,
    pub p: i64,
    pub r: i64,
    pub v: i64,
    /// Encrypt metadata.
    pub emd: bool,
    pub id: Vec<u8>,
}

/// `decodedStreamBudget`: the running totals of one parse.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodedBudget {
    /// Go capacity of the decoded streams charged so far.
    pub used: i64,
    /// Object stream bytes, counted apart from `used` unless `max_total_decode_bytes` is set.
    pub obj_stm: i64,
    /// Object stream entries declared so far.
    pub entries: i64,
    /// Estimated heap of the array elements and dictionary entries parsed so far.
    pub parsed: i64,
    /// Encoded stream bytes read so far.
    pub streams: i64,
}

/// The ledger estimate of one table entry: its key and its [`Entry`], and the object stream
/// number a compressed entry adds to the read context, twice over because ascending inserts leave
/// the B-tree nodes about half full.
pub(crate) const ENTRY_BYTES: u64 = 2 * (2 * size_of::<i64>() + size_of::<Entry>()) as u64;

/// The copies of one nesting level of an object that a walk frame holds at once: the raw entry,
/// the resolved one and a conversion of it (docs/limits.md, "Working-copy reserve").
pub(crate) const WORKING_COPIES: u64 = 3;

/// [`Object::copy_bytes`] of `o`, and the sum of it and of the copy bytes of the parts nested in
/// `o` along its heaviest path: what a walk holds when each level copies the part it enters.
fn path_copy_bytes(o: &Object) -> (u64, u64) {
    let (copy, inner) = match o {
        Object::Array(a) => a.iter().map(path_copy_bytes).fold(
            (std::mem::size_of_val(a.as_slice()) as u64, 0),
            |(c, w), (ci, wi)| (c + ci, w.max(wi)),
        ),
        Object::Dict(d) => dict_path_copy_bytes(d),
        Object::Stream(sd) => (o.copy_bytes(), dict_path_copy_bytes(&sd.dict).1),
        _ => (o.copy_bytes(), 0),
    };
    (copy, copy.saturating_add(inner))
}

/// [`path_copy_bytes`] of a dictionary: its copy bytes and the heaviest path of its values.
fn dict_path_copy_bytes(d: &Dict) -> (u64, u64) {
    d.iter().fold((0, 0), |(c, w), (k, v)| {
        let (cv, wv) = path_copy_bytes(v);
        (c + entry_copy_bytes(k, &Object::Null) + cv, w.max(wv))
    })
}

/// What the object parser charges what it parses to: the table's `MaxParsedBytes` counter
/// (Go's `chargeParsed`), when it is on, and always the document ledger, with the same estimate.
/// Get one from [`XRefTable::parsed_charge`].
#[derive(Debug)]
pub struct ParsedCounter<'a> {
    parsed: &'a mut i64,
    max: i64,
    held: &'a mut Held,
}

impl ParsedCharge for ParsedCounter<'_> {
    fn charge_parsed(&mut self, n: i64) -> Result<(), Error> {
        if self.max > 0 {
            *self.parsed = self.parsed.saturating_add(n);
            if *self.parsed > self.max {
                return Err(Error::Limit(LimitKind::Parsed).context(format!(
                    "an estimated {} bytes, limit {}",
                    *self.parsed, self.max
                )));
            }
        }
        self.held.grow_by(u64::try_from(n).unwrap_or(0))
    }

    fn charge_ledger(&mut self, n: i64) -> Result<(), Error> {
        self.held.grow_by(u64::try_from(n).unwrap_or(0))
    }
}

/// `XRefTable`: a PDF cross reference table plus the document state that goes with it.
///
/// Fields that only the validate, optimize or write packages use (name trees, annotation
/// caches, signatures, viewer preferences) are added by those packages when they need them.
#[derive(Debug)]
pub struct XRefTable {
    pub table: BTreeMap<i64, Entry>,
    /// `Size` of the trailer dictionary; `None` is nil.
    pub size: Option<i64>,
    /// After reading in all objects from the xref table.
    pub max_obj_nr: i64,
    pub page_count: i64,
    /// The reference to the catalog.
    pub root: Option<Ref>,
    /// A snapshot of the catalog that takes precedence over `root`. Go caches the resolved
    /// catalog here; pdf-rs does not (docs/design.md section 3), and only the repair case of
    /// read.go:1759-1762 sets it.
    pub root_dict: Option<Dict>,
    pub encrypt: Option<Ref>,
    pub e: Option<Enc>,
    pub enc_key: Vec<u8>,
    pub aes4_strings: bool,
    pub aes4_streams: bool,
    pub aes4_embedded_streams: bool,
    /// The version the source claims in its header.
    pub header_version: Option<Version>,
    /// The optional catalog version, which takes precedence when it is later.
    pub root_version: Option<Version>,
    /// The trailer's `ID`.
    pub id: Option<Array>,
    /// The xref stream whose dictionary holds the `ID` array: Go's `ID` is that very slice
    /// (read.go:1220, 999-1001), so `ensureFileID` changes the dictionary too.
    pub id_xref_stm: Option<i64>,
    pub info: Option<Ref>,
    /// A direct trailer info dictionary, pending version identification.
    pub direct_info_dict: Option<Dict>,
    pub title: String,
    pub subject: String,
    pub author: String,
    pub creator: String,
    pub producer: String,
    pub creation_date: String,
    pub mod_date: String,
    pub keywords: String,
    pub keyword_list: BTreeSet<String>,
    pub properties: BTreeMap<String, String>,
    pub offset_primary_hint_table: Option<i64>,
    pub offset_overflow_hint_table: Option<i64>,
    pub linearization_objs: BTreeSet<i64>,
    /// The trailer's additional streams (an off-spec array of references).
    pub additional_streams: Option<Array>,
    pub tagged: bool,
    pub custom_extensions: bool,
    /// The current page during validation.
    pub cur_page: i64,
    /// Whether the file validated successfully against ISO 32000.
    pub valid: bool,
    pub optimized: bool,
    pub form: Option<Dict>,
    pub outlines: Option<Dict>,
    pub signature_exist: bool,
    pub append_only: bool,
    /// The cache of the name trees found in the catalog, by tree name (`Names`).
    pub names: BTreeMap<String, Node>,
    /// What validation records on the document (`Dests`, `CatalogXMPMeta`).
    pub vstate: crate::validate::state::ValidateState,
    /// The one configuration of the document.
    pub conf: Config,
    pub(crate) budget: DecodedBudget,
    pub(crate) mem: Mem,
    /// The ledger charge of the table entries, the parsed objects, the object lists of the
    /// cross-reference streams and the optimizer's page sets. It only grows, and is released when
    /// the table drops.
    pub(crate) held: Held,
    /// The outcome of the first full decode of each object stream (Go's `sync.Once` in
    /// `objectStreamDecodeFunc`): a failure is remembered, and the stream is not decoded again.
    pub(crate) objstm_state: BTreeMap<i64, Result<(), Error>>,
    /// The working-copy reserve of the pass in progress: the heaviest copy path it covers and
    /// its charge, [`WORKING_COPIES`] times that (docs/limits.md, "Working-copy reserve").
    pub(crate) reserve: Option<(u64, Held)>,
}

impl XRefTable {
    /// `newXRefTable`: an empty table with a fresh ledger of `conf.limits.max_alloc` bytes.
    pub fn new(conf: Config) -> XRefTable {
        let mem = Mem::new(&conf.limits);
        Self::new_with_mem(conf, mem)
    }

    /// As [`XRefTable::new`], charging the caller's ledger `mem` (shared with other documents
    /// of one call) instead of a fresh one; `conf.limits.max_alloc` is then not used.
    pub fn new_with_mem(conf: Config, mem: Mem) -> XRefTable {
        XRefTable {
            table: BTreeMap::new(),
            size: None,
            max_obj_nr: 0,
            page_count: 0,
            root: None,
            root_dict: None,
            encrypt: None,
            e: None,
            enc_key: Vec::new(),
            aes4_strings: false,
            aes4_streams: false,
            aes4_embedded_streams: false,
            header_version: None,
            root_version: None,
            id: None,
            id_xref_stm: None,
            info: None,
            direct_info_dict: None,
            title: String::new(),
            subject: String::new(),
            author: String::new(),
            creator: String::new(),
            producer: String::new(),
            creation_date: String::new(),
            mod_date: String::new(),
            keywords: String::new(),
            keyword_list: BTreeSet::new(),
            properties: BTreeMap::new(),
            offset_primary_hint_table: None,
            offset_overflow_hint_table: None,
            linearization_objs: BTreeSet::new(),
            additional_streams: None,
            tagged: false,
            custom_extensions: false,
            cur_page: 0,
            valid: false,
            optimized: false,
            form: None,
            outlines: None,
            signature_exist: false,
            append_only: false,
            names: BTreeMap::new(),
            vstate: crate::validate::state::ValidateState::default(),
            held: mem.hold(),
            mem,
            conf,
            budget: DecodedBudget::default(),
            objstm_state: BTreeMap::new(),
            reserve: None,
        }
    }

    /// The ledger handle of this document.
    pub fn mem(&self) -> &Mem {
        &self.mem
    }

    /// The running decode, entry, parse and stream totals.
    pub fn budget(&self) -> &DecodedBudget {
        &self.budget
    }

    // -----------------------------------------------------------------------------------------
    // Version

    /// `Version`: the effective PDF version. A catalog version takes precedence only when it is
    /// later than the header version (7.7.2).
    ///
    /// # Errors
    /// [`Error::Panic`] where Go dereferences a nil header version.
    pub fn version(&self) -> Result<Version, Error> {
        match (self.root_version, self.header_version) {
            (Some(r), h) if h.is_none_or(|h| r > h) => Ok(r),
            (_, Some(h)) => Ok(h),
            _ => Err(nil_deref()),
        }
    }

    /// `PDF20`.
    ///
    /// # Errors
    /// As [`XRefTable::version`].
    pub fn pdf20(&self) -> Result<bool, Error> {
        Ok(self.version()? == Version::V20)
    }

    /// `VersionString`.
    ///
    /// # Errors
    /// As [`XRefTable::version`].
    pub fn version_string(&self) -> Result<String, Error> {
        Ok(self.version()?.to_string())
    }

    /// `ValidateVersion`: the effective version must be at least `since`. Go's
    /// `VersionRequirementError` is an [`Error::Validation`] with Go's message.
    ///
    /// # Errors
    /// [`Error::Validation`] if the version is lower, [`Error::Panic`] as
    /// [`XRefTable::version`].
    pub fn validate_version(&self, element: &str, since: Version) -> Result<(), Error> {
        let actual = self.version()?;
        if actual < since {
            return Err(Error::Validation(format!(
                "{element}: unsupported in version {actual}, requires version {since}"
            )));
        }
        Ok(())
    }

    /// `ParseRootVersion`: the optional `Version` name of the catalog, which overrides the
    /// header version. An unresolved catalog has no override.
    ///
    /// # Errors
    /// The catalog errors of [`XRefTable::catalog`] but a missing object, and the entry errors
    /// of `dereference_name_entry`.
    pub fn parse_root_version(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let Some(root_dict) = self.catalog_dict(&NEVER)? else {
            return Ok(None);
        };
        let version = root_dict.find(b"Version").cloned();
        let (name, _) = self.name_from_value(b"Version", version.as_ref())?;
        Ok(name)
    }

    // -----------------------------------------------------------------------------------------
    // Find

    /// `Exists`.
    pub fn exists(&self, obj_nr: i64) -> bool {
        self.table.contains_key(&obj_nr)
    }

    /// `Find`.
    pub fn find(&self, obj_nr: i64) -> Option<&Entry> {
        self.table.get(&obj_nr)
    }

    /// The entry for `obj_nr`, to change it (Go hands out the entry pointer).
    pub fn entry_mut(&mut self, obj_nr: i64) -> Option<&mut Entry> {
        self.table.get_mut(&obj_nr)
    }

    /// `FindTableEntry`: the generation is ignored (xreftable.go:400-405).
    pub fn find_table_entry(&self, obj_nr: i64, _generation: i64) -> Option<&Entry> {
        self.find(obj_nr)
    }

    /// `FindTableEntryForIndRef`.
    pub fn find_table_entry_for_ind_ref(&self, r: Ref) -> Option<&Entry> {
        self.find_table_entry(r.num, r.generation)
    }

    /// `Free`: the entry of a free object, `None` if there is none.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the object is in use.
    pub fn free(&self, obj_nr: i64) -> Result<Option<&Entry>, Error> {
        match self.table.get(&obj_nr) {
            None => Ok(None),
            Some(e) if !e.free => Err(Error::Corrupt(format!(
                "free: object #{obj_nr} found, but not free"
            ))),
            Some(e) => Ok(Some(e)),
        }
    }

    /// `IncrementRefCount`.
    pub fn increment_ref_count(&mut self, r: Ref) {
        if let Some(e) = self.table.get_mut(&r.num) {
            e.ref_count += 1;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Insert

    /// `InsertNew`: adds `entry` at the next object number, which is `size`, and grows `size`.
    ///
    /// # Errors
    /// [`Error::Panic`] where Go dereferences a nil `Size`.
    pub fn insert_new(&mut self, entry: Entry) -> Result<i64, Error> {
        let obj_nr = self.size.ok_or_else(nil_deref)?;
        self.table.insert(obj_nr, entry);
        self.size = Some(obj_nr.saturating_add(1));
        Ok(obj_nr)
    }

    /// `InsertAndUseRecycled`: adds `entry`, reusing the first object of the free list. A
    /// recycled object always gets generation 0, as Go does.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a free list that points at a missing object, [`Error::Panic`]
    /// where Go dereferences a nil.
    pub fn insert_and_use_recycled(&mut self, mut entry: Entry) -> Result<i64, Error> {
        let head_offset = match self.free(0)? {
            Some(h) => h.offset.ok_or_else(nil_deref)?,
            None => return Err(nil_deref()),
        };
        // No free object: a new one.
        if head_offset == 0 {
            entry.ref_count = 1;
            return self.insert_new(entry);
        }

        // Recycle the free object, and update the free list.
        let obj_nr = head_offset;
        let Some(old) = self.table.get_mut(&obj_nr) else {
            return Err(Error::Corrupt(format!(
                "insertAndRecycle: no entry for obj #{obj_nr}"
            )));
        };
        // The new free list head becomes the old head's successor; the old entry is garbage.
        let next = old.offset;
        old.free = false;
        old.offset = None;
        if let Some(h) = self.table.get_mut(&0) {
            h.offset = next;
        }
        entry.ref_count = 1;
        self.table.insert(obj_nr, entry);
        Ok(obj_nr)
    }

    /// `InsertObject`: a new in-use entry of generation 0 and reference count 1.
    ///
    /// # Errors
    /// As [`XRefTable::insert_new`].
    pub fn insert_object(&mut self, obj: Object) -> Result<i64, Error> {
        let mut entry = Entry::gen0(obj);
        entry.ref_count = 1;
        self.insert_new(entry)
    }

    /// `IndRefForObject`: puts `obj` at `obj_nr` and returns a reference to it.
    pub fn ind_ref_for_object(&mut self, obj_nr: i64, obj: Object) -> Ref {
        self.table.insert(obj_nr, Entry::gen0(obj));
        Ref {
            num: obj_nr,
            generation: 0,
        }
    }

    /// `IndRefForNewObject`: inserts `obj`, using the free list, and returns a reference to it.
    ///
    /// # Errors
    /// As [`XRefTable::insert_and_use_recycled`].
    pub fn ind_ref_for_new_object(&mut self, obj: Object) -> Result<Ref, Error> {
        let num = self.insert_and_use_recycled(Entry::gen0(obj))?;
        Ok(Ref { num, generation: 0 })
    }

    /// `NewStreamDictForBuf`: a stream dictionary whose decoded content is `buf` (already
    /// charged by the caller), to be encoded with Flate.
    pub fn new_stream_dict_for_buf(&self, buf: Bytes) -> StreamDict {
        let mut dict = Dict::default();
        dict.insert_name(b"Filter", FLATE);
        StreamDict {
            dict,
            content: Some(buf),
            filter_pipeline: vec![PdfFilter {
                name: FLATE.to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        }
    }

    // -----------------------------------------------------------------------------------------
    // Free list

    /// `freeObjects`: the numbers of the free objects but object 0.
    fn free_objects(&self) -> BTreeSet<i64> {
        self.table
            .iter()
            .filter(|(k, e)| e.free && **k > 0)
            .map(|(k, _)| *k)
            .collect()
    }

    /// `handleDanglingFree`: puts the free objects that are not on the list behind the head,
    /// unless they are forever deleted (generation 65535), which point to object 0.
    fn handle_dangling_free(&mut self, m: &BTreeSet<i64>) -> Result<(), Error> {
        for &i in m {
            let head_offset = self.table.get(&0).ok_or_else(nil_deref)?.offset;
            let Some(entry) = self.table.get_mut(&i) else {
                return Err(Error::Corrupt(format!(
                    "handleDanglingFree: no xref entry found for obj #{i}"
                )));
            };
            if !entry.free {
                return Err(Error::Corrupt(format!(
                    "handleDanglingFree: xref entry is not free for obj #{i}"
                )));
            }
            if entry.generation.ok_or_else(nil_deref)? == FREE_HEAD_GENERATION {
                entry.offset = Some(0);
                continue;
            }
            entry.offset = head_offset;
            if let Some(h) = self.table.get_mut(&0) {
                h.offset = Some(i);
            }
        }
        Ok(())
    }

    /// `validateFreeList`: follows the list from `f`, starting with the entry `e`. Returns the
    /// entry after which the list continues with the lowest unlisted free object (if the list
    /// had to be cut), and that object's number.
    fn validate_free_list(
        &mut self,
        mut f: i64,
        m: &mut BTreeSet<i64>,
        mut e: i64,
    ) -> Result<(Option<i64>, i64), Error> {
        let mut last_valid: Option<i64> = None;
        let mut next_free = 0;

        while f != 0 {
            // Is obj f one of the free objects recorded?
            if !m.contains(&f) {
                if !m.is_empty() && last_valid.is_none() {
                    last_valid = Some(e);
                    f = m.first().copied().unwrap_or(-1);
                    next_free = f;
                    continue;
                }
                // Repair the last entry.
                let last = self.table.get_mut(&e).ok_or_else(nil_deref)?;
                if last.offset.is_none() {
                    return Err(nil_deref());
                }
                last.offset = Some(0);
                break;
            }

            m.remove(&f);

            match self.free(f)? {
                None => {
                    return Err(Error::Corrupt(format!(
                        "ensureValidFreeList: no xref entry found for obj #{f}"
                    )));
                }
                Some(entry) => {
                    let next = entry.offset.ok_or_else(nil_deref)?;
                    e = f;
                    f = next;
                }
            }
        }

        Ok((last_valid, next_free))
    }

    /// `EnsureValidFreeList`: repairs the free list so that it links exactly the recorded free
    /// objects (7.5.4).
    ///
    /// # Errors
    /// [`Error::Corrupt`] for entries that are missing or not free, [`Error::Panic`] where Go
    /// dereferences a nil.
    pub fn ensure_valid_free_list(&mut self) -> Result<(), Error> {
        let mut m = self.free_objects();

        // Verify free object 0 as the head of the free list.
        let head = self.table.entry(0).or_insert_with(Entry::free_head);

        // Fix the generation of obj 0.
        if head.generation.ok_or_else(nil_deref)? != FREE_HEAD_GENERATION {
            head.generation = Some(FREE_HEAD_GENERATION);
        }

        // No free object other than 0: repair the head if necessary.
        if m.is_empty() {
            if head.offset.ok_or_else(nil_deref)? != 0 {
                head.offset = Some(0);
            }
            return Ok(());
        }

        let f = head.offset.ok_or_else(nil_deref)?;
        let (last_valid, next_free) = self.validate_free_list(f, &mut m, 0)?;

        if let Some(lv) = last_valid {
            let e = self.table.get_mut(&lv).ok_or_else(nil_deref)?;
            if e.offset.is_none() {
                return Err(nil_deref());
            }
            e.offset = Some(next_free);
        }

        if m.is_empty() {
            return Ok(());
        }

        // Insert the remaining free objects into the verified linked list.
        self.handle_dangling_free(&m)
    }

    // -----------------------------------------------------------------------------------------
    // Validity flags

    /// `IsObjValid`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a negative number, a missing entry or a free entry.
    pub fn is_obj_valid(&self, obj_nr: i64, gen_nr: i64) -> Result<bool, Error> {
        validate_obj_nr_and_gen_nr(obj_nr, gen_nr)?;
        let Some(entry) = self.find_table_entry(obj_nr, gen_nr) else {
            return Err(Error::Corrupt(format!(
                "missing xref entry for obj#{obj_nr} gen#{gen_nr}"
            )));
        };
        if entry.free {
            return Err(Error::Corrupt(format!(
                "unexpected free xref entry for obj#{obj_nr} gen#{gen_nr}"
            )));
        }
        Ok(entry.valid)
    }

    /// `IsValid`.
    ///
    /// # Errors
    /// As [`XRefTable::is_obj_valid`].
    pub fn is_valid(&self, r: Ref) -> Result<bool, Error> {
        validate_obj_nr_and_gen_nr(r.num, r.generation).map_err(|e| {
            e.context(format!(
                "invalid indirect reference {} {} R",
                r.num, r.generation
            ))
        })?;
        self.is_obj_valid(r.num, r.generation)
    }

    /// `IsObjBeingValidated`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a missing or a free entry.
    pub fn is_obj_being_validated(&self, obj_nr: i64, gen_nr: i64) -> Result<bool, Error> {
        let Some(entry) = self.find_table_entry(obj_nr, gen_nr) else {
            return Err(Error::Corrupt(format!(
                "isObjBeingValidated: no entry for obj#{obj_nr}"
            )));
        };
        if entry.free {
            return Err(Error::Corrupt(format!(
                "isObjBeingValidated: unexpected free entry for obj#{obj_nr}"
            )));
        }
        Ok(entry.being_validated)
    }

    /// `IsBeingValidated`.
    ///
    /// # Errors
    /// As [`XRefTable::is_obj_being_validated`].
    pub fn is_being_validated(&self, r: Ref) -> Result<bool, Error> {
        self.is_obj_being_validated(r.num, r.generation)
    }

    /// `SetValid`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a missing or a free entry.
    pub fn set_valid(&mut self, r: Ref) -> Result<(), Error> {
        match self.table.get_mut(&r.num) {
            None => Err(Error::Corrupt(format!(
                "setValid: no entry for obj#{}",
                r.num
            ))),
            Some(e) if e.free => Err(Error::Corrupt(format!(
                "setValid: unexpected free entry for obj#{}",
                r.num
            ))),
            Some(e) => {
                e.valid = true;
                e.being_validated = false;
                Ok(())
            }
        }
    }

    /// `SetBeingValidated`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a missing or a free entry.
    pub fn set_being_validated(&mut self, r: Ref) -> Result<(), Error> {
        match self.table.get_mut(&r.num) {
            None => Err(Error::Corrupt(format!(
                "setBeingValidated: no entry for obj#{}",
                r.num
            ))),
            Some(e) if e.free => Err(Error::Corrupt(format!(
                "setBeingValidated: unexpected free entry for obj#{}",
                r.num
            ))),
            Some(e) => {
                e.being_validated = true;
                e.valid = false;
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Stream dictionaries

    /// `DereferenceStreamDict`: the stream dictionary `o` is or refers to, as a clone that
    /// shares its buffers (changes to `content` do not persist, as in Go), and whether the
    /// entry was already marked valid. A reference to an undefined or free object, or to an
    /// object not loaded yet, is the null object: `None` (7.3.10). Marks the entry valid.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the object is not a stream dictionary.
    pub fn dereference_stream_dict(
        &mut self,
        o: &Object,
    ) -> Result<(Option<StreamDict>, bool), Error> {
        let r = match o {
            Object::Ref(r) => *r,
            Object::Stream(sd) => return Ok((Some((**sd).clone()), false)),
            other => {
                return Err(Error::Corrupt(format!(
                    "dereferenceStreamDict: wrong type <{}> {}",
                    go_v(other),
                    type_name(other)
                )));
            }
        };
        let Some(entry) = self.table.get_mut(&r.num) else {
            return Ok((None, false));
        };
        // Go parses `null` to nil, so an entry holding the null object is `entry.Object == nil`.
        if entry.free || matches!(entry.object, None | Some(EntryObject::Obj(Object::Null))) {
            return Ok((None, false));
        }
        let ev = entry.valid;
        entry.valid = true;
        match &entry.object {
            Some(EntryObject::Obj(Object::Stream(sd))) => Ok((Some((**sd).clone()), ev)),
            Some(EntryObject::Obj(other)) => Err(Error::Corrupt(format!(
                "dereferenceStreamDict: wrong type <{}> {}",
                go_v(o),
                type_name(other)
            ))),
            other => Err(Error::Corrupt(format!(
                "dereferenceStreamDict: wrong type <{}> {}",
                go_v(o),
                match other {
                    Some(EntryObject::ObjStm(_)) => "types.ObjectStreamDict",
                    Some(EntryObject::XRefStm(_)) => "types.XRefStreamDict",
                    _ => "types.LazyObjectStreamObject",
                }
            ))),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Catalog

    /// `catalogDict`: the catalog, preserving PDF null semantics: a root that does not resolve
    /// is `None`. A decode error of a lazy catalog is returned as it is.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a missing root reference or a catalog that is no dictionary.
    pub(crate) fn catalog_dict(&mut self, cancel: &AtomicBool) -> Result<Option<&Dict>, Error> {
        if self.root_dict.is_some() {
            return Ok(self.root_dict.as_ref());
        }
        let Some(root) = self.root else {
            return Err(Error::Corrupt("missing root dict".to_string()));
        };
        let (o, _) = self.ind_ref_to_object(root, true, cancel)?;
        match o {
            Object::Null => Ok(None),
            Object::Dict(d) => Ok(Some(d)),
            _ => Err(Error::Corrupt("corrupt root dict".to_string())),
        }
    }

    /// `Catalog`: the document catalog dictionary.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with `missing root dict` or `corrupt root dict`, or the decode error
    /// of a lazy catalog.
    pub fn catalog(&mut self) -> Result<&Dict, Error> {
        self.catalog_dict(&NEVER)?
            .ok_or_else(|| Error::Corrupt("missing root dict".to_string()))
    }

    /// The catalog, to change it (Go's `Catalog` returns the stored map).
    ///
    /// # Errors
    /// As [`XRefTable::catalog`].
    pub fn catalog_mut(&mut self) -> Result<&mut Dict, Error> {
        self.catalog()?;
        if self.root_dict.is_some() {
            return self
                .root_dict
                .as_mut()
                .ok_or_else(|| Error::Corrupt("missing root dict".to_string()));
        }
        let root = self
            .root
            .ok_or_else(|| Error::Corrupt("missing root dict".to_string()))?;
        match self.table.get_mut(&root.num) {
            Some(Entry {
                object: Some(EntryObject::Obj(Object::Dict(d))),
                ..
            }) => Ok(d),
            _ => Err(Error::Corrupt("corrupt root dict".to_string())),
        }
    }

    /// `EncryptDict`: the document encryption dictionary.
    ///
    /// # Errors
    /// [`Sentinel::MissingEncryptDictObject`] and [`Sentinel::WrongTypeEncryptDictObject`], or
    /// the decode error of a lazy object.
    pub fn encrypt_dict(&mut self) -> Result<&Dict, Error> {
        let Some(enc) = self.encrypt else {
            return Err(Error::Sentinel(Sentinel::MissingEncryptDictObject));
        };
        let (o, _) = self.ind_ref_to_object(enc, true, &NEVER)?;
        match o {
            Object::Null => Err(Error::Sentinel(Sentinel::MissingEncryptDictObject)),
            Object::Dict(d) => Ok(d),
            other => {
                Err(Error::Sentinel(Sentinel::WrongTypeEncryptDictObject).context(type_name(other)))
            }
        }
    }

    /// `CatalogHasPieceInfo`.
    ///
    /// # Errors
    /// As [`XRefTable::catalog`].
    pub fn catalog_has_piece_info(&mut self) -> Result<bool, Error> {
        let root = self.catalog()?;
        Ok(root.find(b"PieceInfo").is_some_and(|o| !o.is_null()))
    }

    /// `Pages`: the reference to the page tree root in the catalog.
    ///
    /// # Errors
    /// [`Error::Corrupt`] with `missing pages root` or `corrupt pages root`, or the errors of
    /// [`XRefTable::catalog`].
    pub fn pages(&mut self) -> Result<Ref, Error> {
        let root = self.catalog()?;
        match root.find(b"Pages") {
            None => Err(Error::Corrupt("missing pages root".to_string())),
            Some(Object::Ref(r)) => Ok(*r),
            Some(_) => Err(Error::Corrupt("corrupt pages root".to_string())),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Limit charges

    /// `ChargeObjectStreamEntries`: adds the `n` entries of another object stream to the total
    /// and fails once `max_object_stream_total_count` is passed.
    ///
    /// # Errors
    /// [`LimitKind::ObjectStreamEntries`].
    pub fn charge_object_stream_entries(&mut self, n: i64) -> Result<(), Error> {
        let max = lim(self.conf.limits.max_object_stream_total_count);
        self.budget.entries = self.budget.entries.saturating_add(n);
        if max > 0 && self.budget.entries > max {
            return Err(
                Error::Limit(LimitKind::ObjectStreamEntries).context(format!(
                    "object stream entries {} exceed MaxObjectStreamTotalCount {max}",
                    self.budget.entries
                )),
            );
        }
        Ok(())
    }

    /// `ChargeStreamBytes`: counts `n` encoded stream bytes against `max_stream_total_bytes`;
    /// called before they are read when the length is known and after otherwise. 0 = off.
    ///
    /// # Errors
    /// [`LimitKind::StreamTotal`].
    pub fn charge_stream_bytes(&mut self, n: i64) -> Result<(), Error> {
        let max = lim(self.conf.limits.max_stream_total_bytes);
        if max <= 0 {
            return Ok(());
        }
        let streams = self.budget.streams;
        if n > max.saturating_sub(streams) {
            return Err(Error::Limit(LimitKind::StreamTotal)
                .context(format!("{streams} bytes read, {n} more, limit {max}")));
        }
        self.budget.streams = streams.saturating_add(n);
        Ok(())
    }

    /// `WithParsedCharge`: the counter the object parser charges what it parses to. It counts
    /// against `max_parsed_bytes` only when that is on (Go attaches it only then), and charges
    /// the ledger always. Pass it to `ParseCtx::with_charge`.
    pub fn parsed_charge(&mut self) -> ParsedCounter<'_> {
        ParsedCounter {
            parsed: &mut self.budget.parsed,
            max: lim(self.conf.limits.max_parsed_bytes),
            held: &mut self.held,
        }
    }

    /// Inserts `entry` as object `obj_nr`, charging [`ENTRY_BYTES`] to the ledger first when the
    /// key is new.
    ///
    /// # Errors
    /// [`Error::Alloc`]; the table is unchanged.
    pub(crate) fn insert_charged(&mut self, obj_nr: i64, entry: Entry) -> Result<(), Error> {
        if !self.table.contains_key(&obj_nr) {
            self.held.grow_by(ENTRY_BYTES)?;
        }
        self.table.insert(obj_nr, entry);
        Ok(())
    }

    /// Starts the working-copy reserve of a validate, optimize or write pass: charges
    /// [`WORKING_COPIES`] times the heaviest copy path of the loaded objects until
    /// [`XRefTable::end_reserve`]. A pass inside a pass keeps the outer reserve. Returns whether
    /// this call started it.
    ///
    /// # Errors
    /// [`Error::Alloc`].
    pub(crate) fn start_reserve(&mut self) -> Result<bool, Error> {
        if self.reserve.is_some() {
            return Ok(false);
        }
        let objects = self.table.values().filter_map(|e| match &e.object {
            Some(EntryObject::Obj(o)) => Some(path_copy_bytes(o).1),
            _ => None,
        });
        let root = self.root_dict.iter().map(|d| {
            let (c, w) = dict_path_copy_bytes(d);
            c.saturating_add(w)
        });
        let m = objects.chain(root).max().unwrap_or(0);
        let held = self.mem.charge(WORKING_COPIES.saturating_mul(m))?;
        self.reserve = Some((m, held));
        Ok(true)
    }

    /// Ends the reserve that [`XRefTable::start_reserve`] started (`started`).
    pub(crate) fn end_reserve(&mut self, started: bool) {
        if started {
            self.reserve = None;
        }
    }

    /// Raises the reserve of the pass in progress to cover `o`, an object loaded during it.
    ///
    /// # Errors
    /// [`Error::Alloc`]; the reserve is unchanged.
    pub(crate) fn grow_reserve(&mut self, o: &Object) -> Result<(), Error> {
        if let Some((m, held)) = &mut self.reserve {
            let w = path_copy_bytes(o).1;
            if w > *m {
                held.grow_by(WORKING_COPIES.saturating_mul(w - *m))?;
                *m = w;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Decode budget

    /// `DecodeStream`: decodes `sd` within the per-stream and cumulative decoded-byte limits.
    /// The cumulative budget charges Go's slice capacity of the decoded content, once.
    ///
    /// # Errors
    /// The errors of `StreamDict::decode_with_limit`, and [`LimitKind::DecodeBudget`] (which is
    /// also a decode-limit error) over the cumulative budget; the content is dropped then.
    pub fn decode_stream(&mut self, sd: &mut StreamDict, cancel: &AtomicBool) -> Result<(), Error> {
        Self::decode_budgeted(
            &self.conf.limits,
            &mut self.budget,
            &self.mem,
            sd,
            false,
            cancel,
        )
    }

    /// `DecodeObjectStream`: [`XRefTable::decode_stream`] for object streams. Without
    /// `max_total_decode_bytes` they keep their own allowance, apart from the one of streams.
    ///
    /// # Errors
    /// As [`XRefTable::decode_stream`].
    pub fn decode_object_stream(
        &mut self,
        sd: &mut StreamDict,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        Self::decode_budgeted(
            &self.conf.limits,
            &mut self.budget,
            &self.mem,
            sd,
            true,
            cancel,
        )
    }

    /// `DecodeImageStream`: [`XRefTable::decode_stream`] for images. Without
    /// `max_total_decode_bytes` images have no cumulative cap.
    ///
    /// # Errors
    /// As [`XRefTable::decode_stream`].
    pub fn decode_image_stream(
        &mut self,
        sd: &mut StreamDict,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if self.conf.limits.max_total_decode_bytes == 0 {
            return sd.decode_with_limit(lim(self.conf.limits.decode_limit()), &self.mem, cancel);
        }
        self.decode_stream(sd, cancel)
    }

    /// `decodeStream` on the parts of the table it uses, so that `sd` may live inside the table.
    pub(crate) fn decode_budgeted(
        limits: &Limits,
        budget: &mut DecodedBudget,
        mem: &Mem,
        sd: &mut StreamDict,
        obj_stm: bool,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let decode_limit = lim(limits.decode_limit());
        if sd.content.is_some() {
            return sd.decode_with_limit(decode_limit, mem, cancel);
        }

        let buffer_limit = lim(limits.total_decode_limit());
        let used = if obj_stm && limits.max_total_decode_bytes == 0 {
            &mut budget.obj_stm
        } else {
            &mut budget.used
        };
        let remaining = buffer_limit.saturating_sub(*used);
        let aggregate_limited = remaining < decode_limit;
        let mut limit = decode_limit;
        if aggregate_limited {
            // A limit of 0 means the default, so use one byte when the budget is exhausted.
            limit = remaining.max(1);
        }

        if let Err(e) = sd.decode_with_limit(limit, mem, cancel) {
            if aggregate_limited && e.is_decode_limit_exceeded() {
                return Err(decode_budget_error(buffer_limit, obj_stm));
            }
            return Err(e);
        }
        if decoded_content_aliases_raw(sd) {
            return Ok(());
        }
        if sd.content.as_ref().is_none_or(|c| c.go_cap() == 0) {
            return Ok(());
        }
        if obj_stm {
            compact_content(sd, mem)?;
        }

        let n = sd.content.as_ref().map_or(0, |c| lim(c.go_cap() as u64));
        if n > remaining {
            sd.content = None;
            return Err(decode_budget_error(buffer_limit, obj_stm));
        }
        *used = used.saturating_add(n);
        Ok(())
    }
}

/// `validateObjNrAndGenNr`.
fn validate_obj_nr_and_gen_nr(obj_nr: i64, gen_nr: i64) -> Result<(), Error> {
    if obj_nr < 0 {
        return Err(Error::Corrupt(format!(
            "invalid negative object number {obj_nr}"
        )));
    }
    if gen_nr < 0 {
        return Err(Error::Corrupt(format!(
            "invalid negative generation number {gen_nr} for obj#{obj_nr}"
        )));
    }
    Ok(())
}

/// `decodedContentAliasesRaw`: the content is the raw buffer itself.
fn decoded_content_aliases_raw(sd: &StreamDict) -> bool {
    match (&sd.content, &sd.raw) {
        (Some(c), Some(r)) => !c.is_empty() && !r.is_empty() && Bytes::ptr_eq(c, r),
        _ => false,
    }
}

/// `compactContent`: drops the spare capacity of a decoded object stream buffer.
fn compact_content(sd: &mut StreamDict, mem: &Mem) -> Result<(), Error> {
    let compact = match &sd.content {
        Some(c) if c.len() < c.go_cap() => Some(mem.bytes_from(c, c.len())?),
        _ => None,
    };
    if let Some(b) = compact {
        sd.content = Some(b);
    }
    Ok(())
}

/// `decodeBufferBudgetError`.
fn decode_budget_error(limit: i64, obj_stm: bool) -> Error {
    let what = if obj_stm { "object stream" } else { "stream" };
    Error::Limit(LimitKind::DecodeBudget { objstm: obj_stm }).context(format!(
        "{what} decoding exceeded {limit}-byte cumulative budget"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Sentinel;

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn r(n: i64) -> Ref {
        Ref {
            num: n,
            generation: 0,
        }
    }

    fn corrupt(msg: &str) -> Error {
        Error::Corrupt(msg.to_string())
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn free_entry(next: i64, generation: i64) -> Entry {
        Entry {
            free: true,
            offset: Some(next),
            generation: Some(generation),
            ..Entry::default()
        }
    }

    fn with_root(root: Option<Ref>, obj: Option<Object>, cached: Option<Dict>) -> XRefTable {
        let mut x = table();
        x.root = root;
        x.root_dict = cached;
        if let Some(o) = obj {
            x.table.insert(1, Entry::gen0(o));
        }
        x
    }

    // Port of TestCatalogContract.
    #[test]
    fn catalog_contract() {
        let cases: Vec<(&str, XRefTable, Option<&str>)> = vec![
            (
                "MissingRoot",
                with_root(None, None, None),
                Some("missing root dict"),
            ),
            (
                "UnresolvedRoot",
                with_root(Some(r(1)), None, None),
                Some("missing root dict"),
            ),
            (
                "NullObject",
                with_root(Some(r(1)), Some(Object::Null), None),
                Some("missing root dict"),
            ),
            (
                "WrongType",
                with_root(Some(r(1)), Some(Object::Integer(1)), None),
                Some("corrupt root dict"),
            ),
            (
                "Dictionary",
                with_root(Some(r(1)), Some(Object::Dict(Dict::default())), None),
                None,
            ),
            (
                "CachedDictionary",
                with_root(None, None, Some(Dict::default())),
                None,
            ),
        ];
        for (name, mut x, want) in cases {
            let got = x.catalog().map(|_| ());
            match want {
                Some(msg) => assert_eq!(got, Err(corrupt(msg)), "{name}"),
                None => assert_eq!(got, Ok(()), "{name}"),
            }
        }
    }

    // Port of TestParseRootVersion.
    #[test]
    fn parse_root_version_cases() {
        let ver = |s: &str| Object::Dict(dict(&[("Version", Object::name(s))]));
        let cases: Vec<(&str, XRefTable, Result<Option<&str>, &str>)> = vec![
            (
                "MissingRoot",
                with_root(None, None, None),
                Err("missing root dict"),
            ),
            (
                "UnresolvedRoot",
                with_root(Some(r(1)), None, None),
                Ok(None),
            ),
            (
                "NullRoot",
                with_root(Some(r(1)), Some(Object::Null), None),
                Ok(None),
            ),
            (
                "WrongRootType",
                with_root(Some(r(1)), Some(Object::Integer(1)), None),
                Err("corrupt root dict"),
            ),
            (
                "NoOverride",
                with_root(Some(r(1)), Some(Object::Dict(Dict::default())), None),
                Ok(None),
            ),
            (
                "Override",
                with_root(Some(r(1)), Some(ver("1.7")), None),
                Ok(Some("1.7")),
            ),
            (
                "CachedOverride",
                with_root(None, None, Some(dict(&[("Version", Object::name("1.6"))]))),
                Ok(Some("1.6")),
            ),
        ];
        for (name, mut x, want) in cases {
            let got = x.parse_root_version();
            match want {
                Err(msg) => assert_eq!(got, Err(corrupt(msg)), "{name}"),
                Ok(v) => assert_eq!(got, Ok(v.map(|s| s.as_bytes().to_vec())), "{name}"),
            }
        }
    }

    // Port of TestPageRootErrors (the Pages part).
    #[test]
    fn pages_root_errors() {
        let cases: Vec<(&str, Option<Dict>, &str)> = vec![
            ("MissingCatalog", None, "missing root dict"),
            ("MissingPages", Some(Dict::default()), "missing pages root"),
            (
                "NullPages",
                Some(dict(&[("Pages", Object::Null)])),
                "corrupt pages root",
            ),
            (
                "IntegerPages",
                Some(dict(&[("Pages", Object::Integer(1))])),
                "corrupt pages root",
            ),
            (
                "DirectPages",
                Some(dict(&[("Pages", Object::Dict(Dict::default()))])),
                "corrupt pages root",
            ),
            (
                "ArrayPages",
                Some(dict(&[("Pages", Object::Array(Vec::new()))])),
                "corrupt pages root",
            ),
        ];
        for (name, catalog, want) in cases {
            let mut x = with_root(None, None, catalog);
            assert_eq!(x.pages(), Err(corrupt(want)), "{name}");
        }
        let mut x = with_root(None, None, Some(dict(&[("Pages", Object::Ref(r(1)))])));
        assert_eq!(x.pages(), Ok(r(1)));
        assert_eq!(x.catalog_has_piece_info(), Ok(false));
    }

    #[test]
    fn catalog_mut_persists_through_the_table() {
        let mut x = with_root(Some(r(1)), Some(Object::Dict(Dict::default())), None);
        x.catalog_mut().unwrap().insert_name(b"Type", b"Catalog");
        assert_eq!(
            x.catalog().unwrap().name_entry(b"Type"),
            Some(b"Catalog".as_slice())
        );
        assert_eq!(x.catalog_has_piece_info(), Ok(false));
        x.catalog_mut().unwrap().insert_int(b"PieceInfo", 1);
        assert_eq!(x.catalog_has_piece_info(), Ok(true));
    }

    // Ports of TestEncryptDictRejectsMissingReferencedObject and ...WrongTypeObject.
    #[test]
    fn encrypt_dict_errors() {
        let mut x = table();
        assert!(
            x.encrypt_dict()
                .unwrap_err()
                .is(Sentinel::MissingEncryptDictObject)
        );
        x.encrypt = Some(r(7));
        assert!(
            x.encrypt_dict()
                .unwrap_err()
                .is(Sentinel::MissingEncryptDictObject)
        );
        x.table.insert(7, Entry::gen0(Object::Integer(1)));
        let err = x.encrypt_dict().unwrap_err();
        assert!(err.is(Sentinel::WrongTypeEncryptDictObject));
        assert!(err.to_string().contains("types.Integer"), "{err}");
        x.table
            .insert(7, Entry::gen0(Object::Dict(Dict::default())));
        assert!(x.encrypt_dict().unwrap().is_empty());
    }

    #[test]
    fn insert_new_and_object() {
        let mut x = table();
        assert!(matches!(
            x.insert_object(Object::Null),
            Err(Error::Panic(_))
        ));
        x.size = Some(3);
        assert_eq!(x.insert_object(Object::Integer(5)), Ok(3));
        assert_eq!(x.size, Some(4));
        let e = x.find(3).unwrap();
        assert_eq!((e.generation, e.ref_count), (Some(0), 1));
        assert_eq!(x.ind_ref_for_object(9, Object::Null), r(9));
        assert!(x.exists(9) && x.find_table_entry(9, 5).is_some());
        assert!(x.find_table_entry_for_ind_ref(r(10)).is_none());
        x.increment_ref_count(r(3));
        x.increment_ref_count(r(99));
        assert_eq!(x.find(3).unwrap().ref_count, 2);
    }

    #[test]
    fn insert_and_use_recycled_takes_the_first_free_object() {
        let mut x = table();
        x.size = Some(6);
        x.table.insert(
            0,
            Entry {
                offset: Some(5),
                ..Entry::free_head()
            },
        );
        x.table.insert(5, free_entry(0, 1));
        assert_eq!(x.ind_ref_for_new_object(Object::Integer(1)), Ok(r(5)));
        let e = x.find(5).unwrap();
        assert!(!e.free);
        assert_eq!((e.generation, e.ref_count), (Some(0), 1));
        assert_eq!(x.find(0).unwrap().offset, Some(0));
        assert_eq!(x.size, Some(6));
        // The free list is empty now: a new object.
        assert_eq!(x.ind_ref_for_new_object(Object::Integer(2)), Ok(r(6)));
        assert_eq!(x.size, Some(7));
        assert_eq!(x.find(6).unwrap().ref_count, 1);
    }

    #[test]
    fn insert_and_use_recycled_needs_the_head() {
        let mut x = table();
        x.size = Some(1);
        assert!(matches!(
            x.insert_and_use_recycled(Entry::default()),
            Err(Error::Panic(_))
        ));
        x.table.insert(
            0,
            Entry {
                offset: Some(4),
                ..Entry::free_head()
            },
        );
        assert_eq!(
            x.insert_and_use_recycled(Entry::default()),
            Err(corrupt("insertAndRecycle: no entry for obj #4"))
        );
        x.table.insert(2, Entry::gen0(Object::Null));
        assert!(x.free(2).is_err());
        assert!(matches!(x.free(3), Ok(None)));
    }

    fn free_list(x: &XRefTable) -> Vec<i64> {
        let mut out = Vec::new();
        let mut f = x.find(0).unwrap().offset.unwrap();
        while f != 0 {
            out.push(f);
            f = x.find(f).unwrap().offset.unwrap();
        }
        out
    }

    #[test]
    fn ensure_valid_free_list_creates_the_head() {
        let mut x = table();
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        let head = x.find(0).unwrap();
        assert!(head.free);
        assert_eq!(
            (head.offset, head.generation),
            (Some(0), Some(FREE_HEAD_GENERATION))
        );

        // A wrong generation and offset of the head are repaired.
        x.table.insert(
            0,
            Entry {
                free: true,
                offset: Some(7),
                generation: Some(1),
                ..Entry::default()
            },
        );
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        let head = x.find(0).unwrap();
        assert_eq!(
            (head.offset, head.generation),
            (Some(0), Some(FREE_HEAD_GENERATION))
        );
    }

    #[test]
    fn ensure_valid_free_list_keeps_a_valid_list() {
        let mut x = table();
        x.table.insert(
            0,
            Entry {
                offset: Some(2),
                ..Entry::free_head()
            },
        );
        x.table.insert(2, free_entry(3, 1));
        x.table.insert(3, free_entry(0, 1));
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        assert_eq!(free_list(&x), vec![2, 3]);
    }

    #[test]
    fn ensure_valid_free_list_links_dangling_objects() {
        let mut x = table();
        x.table.insert(0, Entry::free_head());
        x.table.insert(3, free_entry(0, 1));
        x.table.insert(5, free_entry(0, 1));
        // Forever deleted: points to object 0 and stays off the list.
        x.table.insert(4, free_entry(9, FREE_HEAD_GENERATION));
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        assert_eq!(free_list(&x), vec![5, 3]);
        assert_eq!(x.find(4).unwrap().offset, Some(0));
    }

    #[test]
    fn ensure_valid_free_list_cuts_at_a_broken_link() {
        let mut x = table();
        // The head points at an object that is not free; object 2 is free and unlisted.
        x.table.insert(
            0,
            Entry {
                offset: Some(9),
                ..Entry::free_head()
            },
        );
        x.table.insert(9, Entry::gen0(Object::Null));
        x.table.insert(2, free_entry(0, 1));
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        assert_eq!(free_list(&x), vec![2]);

        // An unlisted free object goes behind the head, in front of the listed ones.
        let mut x = table();
        x.table.insert(
            0,
            Entry {
                offset: Some(4),
                ..Entry::free_head()
            },
        );
        x.table.insert(4, free_entry(0, 1));
        x.table.insert(6, free_entry(0, 1));
        assert_eq!(x.ensure_valid_free_list(), Ok(()));
        assert_eq!(free_list(&x), vec![6, 4]);
    }

    #[test]
    fn validity_flags() {
        let mut x = table();
        x.table.insert(1, Entry::gen0(Object::Null));
        x.table.insert(2, free_entry(0, 1));
        assert_eq!(x.is_obj_valid(1, 0), Ok(false));
        assert_eq!(x.set_valid(r(1)), Ok(()));
        assert_eq!(x.is_valid(r(1)), Ok(true));
        assert_eq!(x.is_being_validated(r(1)), Ok(false));
        assert_eq!(x.set_being_validated(r(1)), Ok(()));
        assert_eq!(x.is_obj_being_validated(1, 0), Ok(true));
        assert_eq!(x.is_valid(r(1)), Ok(false));
        assert!(x.is_obj_valid(-1, 0).is_err());
        assert!(x.is_obj_valid(1, -1).is_err());
        assert!(x.is_obj_valid(7, 0).is_err());
        assert!(x.is_obj_valid(2, 0).is_err());
        assert!(x.set_valid(r(2)).is_err() && x.set_valid(r(7)).is_err());
        assert!(x.set_being_validated(r(2)).is_err());
        assert!(x.is_obj_being_validated(2, 0).is_err());
        assert!(x.is_obj_being_validated(7, 0).is_err());
    }

    #[test]
    fn dereference_stream_dict_marks_the_entry_valid() {
        let mut x = table();
        let mem = x.mem().clone();
        let content = mem.bytes_from(b"abc", 3).unwrap();
        let sd = StreamDict {
            content: Some(content),
            ..StreamDict::default()
        };
        x.table.insert(3, Entry::gen0(Object::Stream(Box::new(sd))));
        x.table.insert(4, Entry::gen0(Object::Integer(1)));
        x.table.insert(5, free_entry(0, 1));
        let o = Object::Ref(r(3));
        let (got, ev) = x.dereference_stream_dict(&o).unwrap();
        assert!(!ev && got.is_some());
        assert!(x.find(3).unwrap().valid);
        let (got, ev) = x.dereference_stream_dict(&o).unwrap();
        assert!(ev);
        // The clone shares the buffer: nothing is charged twice.
        assert_eq!(mem.used(), 3);
        drop(got);
        // Undefined, free and not loaded targets are null.
        for n in [9, 5] {
            assert!(
                x.dereference_stream_dict(&Object::Ref(r(n)))
                    .unwrap()
                    .0
                    .is_none()
            );
        }
        x.table.insert(
            6,
            Entry {
                generation: Some(0),
                ..Entry::default()
            },
        );
        assert!(
            x.dereference_stream_dict(&Object::Ref(r(6)))
                .unwrap()
                .0
                .is_none()
        );
        assert!(x.dereference_stream_dict(&Object::Ref(r(4))).is_err());
        assert_eq!(
            x.dereference_stream_dict(&Object::Integer(1)).err(),
            Some(Error::Corrupt(
                "dereferenceStreamDict: wrong type <1> types.Integer".to_string()
            ))
        );
        // A direct stream is cloned, and is not marked.
        let direct = Object::Stream(Box::default());
        assert!(!x.dereference_stream_dict(&direct).unwrap().1);
    }

    #[test]
    fn new_stream_dict_for_buf_uses_flate() {
        let x = table();
        let buf = x.mem().bytes_from(b"q Q", 3).unwrap();
        let sd = x.new_stream_dict_for_buf(buf);
        assert!(sd.has_sole_filter_named(b"FlateDecode"));
        assert_eq!(sd.name_entry(b"Filter"), Some(b"FlateDecode".as_slice()));
        assert_eq!(sd.content.as_deref(), Some(b"q Q".as_slice()));
        assert!(sd.raw.is_none());
    }

    // The limit charges.

    #[test]
    fn charge_object_stream_entries_limit() {
        let mut x = table();
        assert_eq!(x.charge_object_stream_entries(1 << 30), Ok(()));
        x.conf.limits.max_object_stream_total_count = 5;
        x.budget.entries = 0;
        assert_eq!(x.charge_object_stream_entries(3), Ok(()));
        let err = x.charge_object_stream_entries(3).unwrap_err();
        assert!(err.is_limit_exceeded() && !err.is_decode_limit_exceeded());
        assert_eq!(err.class(), "limit/objstm-entries");
        assert_eq!(x.budget().entries, 6);
    }

    #[test]
    fn charge_stream_bytes_limit() {
        let mut x = table();
        assert_eq!(x.charge_stream_bytes(1 << 40), Ok(()));
        assert_eq!(x.budget().streams, 0);
        x.conf.limits.max_stream_total_bytes = 10;
        assert_eq!(x.charge_stream_bytes(6), Ok(()));
        let err = x.charge_stream_bytes(5).unwrap_err();
        assert!(err.is_limit_exceeded());
        assert_eq!(err.class(), "limit/stream-total");
        assert_eq!(x.budget().streams, 6);
        assert_eq!(x.charge_stream_bytes(4), Ok(()));
        assert_eq!(x.budget().streams, 10);
    }

    #[test]
    fn parsed_charge_counts_against_max_parsed_bytes() {
        let mut x = table();
        // Off: nothing is counted, but the ledger is charged.
        assert_eq!(x.parsed_charge().charge_parsed(40), Ok(()));
        assert_eq!(x.budget().parsed, 0);
        assert_eq!(x.mem().used(), 40);
        x.conf.limits.max_parsed_bytes = 100;
        {
            let mut c = x.parsed_charge();
            assert_eq!(c.charge_parsed(60), Ok(()));
            let err = c.charge_parsed(41).unwrap_err();
            assert!(err.is_limit_exceeded());
            assert_eq!(err.class(), "limit/parsed");
        }
        assert_eq!(x.budget().parsed, 101);
    }

    #[test]
    fn dict_entries_charge_the_ledger_more_than_max_parsed_bytes() {
        let mut x = table();
        x.conf.limits.max_parsed_bytes = 1 << 20;
        let cancel = AtomicBool::new(false);
        {
            let mut c = x.parsed_charge();
            let mut cx = crate::parse::ParseCtx::with_charge(&cancel, &mut c);
            let mut line: &[u8] = b"<</A 1>>";
            crate::parse::parse_object(&mut cx, &mut line, 0, None).unwrap();
        }
        // Go's estimate (256 + 96 + 1) counts as Go does; the ledger also gets the rest of a
        // B-tree slot pair twice over, plus 16, for the entry.
        assert_eq!(x.budget().parsed, 353);
        let slot = (size_of::<Vec<u8>>() + size_of::<Object>()) as i64;
        assert_eq!(x.mem().used() as i64 - 353, 2 * slot + 16 - 96);
    }

    #[test]
    fn parsed_objects_and_entries_are_charged_to_max_alloc() {
        let mut conf = Config::default();
        conf.limits.max_alloc = 2 * ENTRY_BYTES + 63;
        let mut x = XRefTable::new(conf);
        assert_eq!(x.insert_charged(1, Entry::default()), Ok(()));
        // A key that exists is not charged again.
        assert_eq!(x.insert_charged(1, Entry::default()), Ok(()));
        assert_eq!(x.insert_charged(2, Entry::default()), Ok(()));
        assert_eq!(x.mem().used(), 2 * ENTRY_BYTES);
        assert!(matches!(
            x.insert_charged(3, Entry::default()),
            Err(Error::Alloc { .. })
        ));
        assert!(!x.table.contains_key(&3));
        // The parser's estimate goes to the ledger with max_parsed_bytes off.
        let cancel = AtomicBool::new(false);
        let mut c = x.parsed_charge();
        let mut cx = crate::parse::ParseCtx::with_charge(&cancel, &mut c);
        let mut line: &[u8] = b"[1 2 3]";
        let e = crate::parse::parse_object(&mut cx, &mut line, 0, None).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e}");
    }

    // The decode budget. These use ASCIIHex streams, whose decoded buffer has exactly the
    // length as its capacity (asciiHexDecode.go:97).

    fn hex_stream(mem: &Mem) -> StreamDict {
        StreamDict {
            raw: Some(mem.bytes_from(b"31323334353637383930>", 21).unwrap()),
            filter_pipeline: vec![PdfFilter {
                name: b"ASCIIHexDecode".to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        }
    }

    fn total_limit_table(total: u64) -> XRefTable {
        XRefTable::new(Config {
            limits: Limits {
                max_total_decode_bytes: total,
                ..Limits::default()
            },
            ..Config::default()
        })
    }

    // Port of TestDecodeStreamTotalDecodeBytesLimit.
    #[test]
    fn decode_stream_total_decode_bytes_limit() {
        let cancel = AtomicBool::new(false);
        let mut x = total_limit_table(15);
        let mem = x.mem().clone();
        let (mut a, mut b) = (hex_stream(&mem), hex_stream(&mem));
        assert_eq!(x.decode_stream(&mut a, &cancel), Ok(()));
        assert_eq!(a.content.as_deref(), Some(b"1234567890".as_slice()));
        assert_eq!(x.budget().used, 10);
        let err = x.decode_stream(&mut b, &cancel).unwrap_err();
        assert!(err.is_limit_exceeded() && err.is_decode_limit_exceeded());
        assert_eq!(err.class(), "limit/total-decode");
        assert!(err.to_string().contains("cumulative budget"), "{err}");
        // A failed decode keeps nothing, and the budget is unchanged.
        assert!(b.content.is_none());
        assert_eq!(x.budget().used, 10);
    }

    // Port of TestZeroTotalDecodeBytesKeepsSeparateLimits.
    #[test]
    fn zero_total_decode_bytes_keeps_separate_limits() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let mem = x.mem().clone();
        x.budget.used = lim(x.conf.limits.total_decode_limit());
        let err = x.decode_stream(&mut hex_stream(&mem), &cancel).unwrap_err();
        assert!(err.is_limit_exceeded());
        assert_eq!(
            x.decode_image_stream(&mut hex_stream(&mem), &cancel),
            Ok(())
        );
        assert_eq!(
            x.decode_object_stream(&mut hex_stream(&mem), &cancel),
            Ok(())
        );
        assert_eq!(x.budget().obj_stm, 10);
    }

    // Ports of ...CumulativeDecodeBudgetCountsCapacityOnce and ...CoversDeferredAliases.
    #[test]
    fn decode_budget_counts_capacity_once() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let mem = x.mem().clone();
        let mut sd = hex_stream(&mem);
        assert_eq!(x.decode_stream(&mut sd, &cancel), Ok(()));
        let want = lim(sd.content.as_ref().unwrap().go_cap() as u64);
        assert_eq!(x.budget().used, want);
        // A cached decode does not charge again.
        assert_eq!(x.decode_stream(&mut sd, &cancel), Ok(()));
        assert_eq!(x.budget().used, want);
        // Content that aliases the raw buffer is not charged.
        let mut raw = StreamDict {
            raw: Some(mem.bytes_from(b"q Q\n", 4).unwrap()),
            ..StreamDict::default()
        };
        assert_eq!(x.decode_stream(&mut raw, &cancel), Ok(()));
        assert_eq!(x.budget().used, want);
        assert!(Bytes::ptr_eq(
            raw.content.as_ref().unwrap(),
            raw.raw.as_ref().unwrap()
        ));
    }

    #[test]
    fn decode_budget_stops_at_the_remaining_budget() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let mem = x.mem().clone();
        x.budget.used = lim(x.conf.limits.total_decode_limit()) - 5;
        let mut sd = hex_stream(&mem);
        let err = x.decode_stream(&mut sd, &cancel).unwrap_err();
        assert_eq!(err.class(), "limit/total-decode");
        assert!(sd.content.is_none());
        // Exactly what is left is admitted, and charged.
        x.budget.used = lim(x.conf.limits.total_decode_limit()) - 10;
        let mut sd = hex_stream(&mem);
        assert_eq!(x.decode_stream(&mut sd, &cancel), Ok(()));
        assert_eq!(x.budget().used, lim(x.conf.limits.total_decode_limit()));
        // Nothing is left now.
        let err = x.decode_stream(&mut hex_stream(&mem), &cancel).unwrap_err();
        assert_eq!(err.class(), "limit/total-decode");
    }

    #[test]
    fn per_stream_limit_is_not_a_budget_error() {
        let cancel = AtomicBool::new(false);
        let mut x = XRefTable::new(Config {
            limits: Limits {
                max_decode_bytes: 9,
                ..Limits::default()
            },
            ..Config::default()
        });
        let mem = x.mem().clone();
        let err = x.decode_stream(&mut hex_stream(&mem), &cancel).unwrap_err();
        assert_eq!(err.root(), &Error::DecodeLimit);
        assert!(!err.is_limit_exceeded());
    }
}
