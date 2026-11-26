//! Page resources and object deletion (Go: pkg/pdfcpu/model/resource.go `PageResourceNames`;
//! xreftable.go :593-835 `FreeObject`, `DeleteObject`; :1062 `DeleteDictEntry`; :2049-2200
//! `consolidateResources`; :2372-2470 `PageContent` and the consolidation of the resource
//! dictionaries; :2858ff `ConsolidatePageResources`).
//!
//! The configuration is the fixed one of docs/scope.md section 4.1: relaxed validation. A
//! missing required resource, and a missing resource dictionary, are skipped without an error,
//! and an inline image whose `EI` has no whitespace in front of it is accepted.
//!
//! Differences from Go:
//!
//! * [`XRefTable::consolidate_page_resources`] takes the `cancel` flag Go has no context for.
//! * Go's `FontObject`, `ImageObject` and `DuplicateImageObject` (the rest of `resource.go`)
//!   belong to the optimize package and are not here.
//! * Go marks every dictionary and array that `DeleteObject` walks with a key or a slice
//!   identity, so that maps that the object graph shares are walked once. Dictionaries and
//!   arrays are owned here, so nothing is shared and nothing is marked. A reference that was
//!   freed is still remembered, and a direct object is walked as often as it is met.
//! * `DeleteObject` walks a copy of what it deletes, because it changes the table while it
//!   walks. The walk order is Go's with the sorted loop of the determinism patch: the entries
//!   of a dictionary last key first, the elements of an array first to last.
//! * `DeleteDictEntry` takes a dictionary that is not in the table. For a dictionary that is,
//!   use [`XRefTable::delete_dict_entry_at`].
//! * `contentParseError` (a message for a corrupt hex literal) is not ported; the error is the
//!   parser's own, with the context of the content stream.
//! * Go's `ShowSkipped` and `ShowRepaired` messages are not ported.

use super::deref::type_name;
use super::pages::{InheritedPageAttrs, PageTreeVisit};
use super::xref::XRefTable;
use crate::error::{Error, Sentinel};
use crate::ledger::{Bytes, Held};
use crate::object::{Dict, Object, Ref, StreamDict, dict_copy_bytes};
use crate::parse::content::parse_content;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// `resourceTypes`: the resource dictionaries of a page, in the order they are consolidated.
pub const RESOURCE_TYPES: [&str; 7] = [
    "ColorSpace",
    "ExtGState",
    "Font",
    "Pattern",
    "Properties",
    "Shading",
    "XObject",
];

/// `PageResourceNames`: the resource names that a page needs, as found in its content streams,
/// per resource dictionary. The names are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResourceNames(pub BTreeMap<&'static str, BTreeSet<Vec<u8>>>);

impl Default for PageResourceNames {
    fn default() -> PageResourceNames {
        PageResourceNames(
            RESOURCE_TYPES
                .iter()
                .map(|&k| (k, BTreeSet::new()))
                .collect(),
        )
    }
}

impl PageResourceNames {
    /// `NewPageResourceNames`: all seven sets, empty.
    pub fn new() -> PageResourceNames {
        PageResourceNames::default()
    }

    /// `Resources`: the set of the required names for the resource dictionary `s`.
    pub fn resources(&self, s: &str) -> Option<&BTreeSet<Vec<u8>>> {
        self.0.get(s)
    }

    /// `HasResources`: whether any name is required from the resource dictionary `s`.
    pub fn has_resources(&self, s: &str) -> bool {
        self.resources(s).is_some_and(|r| !r.is_empty())
    }

    /// `HasContent`: whether any name is required at all.
    pub fn has_content(&self) -> bool {
        RESOURCE_TYPES.iter().any(|k| self.has_resources(k))
    }

    /// Adds `name` to the set for the resource dictionary `kind`.
    pub fn insert(&mut self, kind: &'static str, name: Vec<u8>) {
        self.0.entry(kind).or_default().insert(name);
    }
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// The error of `consolidateResources` for an inherited entry that is no dictionary.
fn expected_dict_error(o: &Object) -> Error {
    Error::Corrupt(format!(
        "consolidateResources: expected Dict d2: {}",
        type_name(o)
    ))
}

/// The combined content of the content streams of a page, and its charge.
#[derive(Default)]
struct Combined {
    bytes: Vec<u8>,
    held: Option<Held>,
}

/// What a content entry of a page dictionary is, for messages.
fn content_kind(o: &Object) -> &'static str {
    match o {
        Object::Stream(_) => "content stream",
        Object::Array(_) => "content stream array",
        _ => "page content",
    }
}

/// The state of one `ConsolidatePageResources` walk.
struct ConsolidateWalk<'a> {
    cancel: &'a AtomicBool,
    /// Pages consolidated so far.
    page_nr: i64,
    visit: PageTreeVisit,
    /// Go's `pageResourceContextError`: the error came from a page, which says which one.
    page_error: bool,
}

impl XRefTable {
    // -----------------------------------------------------------------------------------------
    // Free and delete

    /// `locateObjForIndRef`: the object that `ir` refers to if this is the only reference to it,
    /// so that its graph can be deleted. `None` is Go's nil: another reference is left (and the
    /// count of references goes down by one), or the target is null.
    fn locate_obj_for_ind_ref(
        &mut self,
        ir: Ref,
        cancel: &AtomicBool,
    ) -> Result<Option<Object>, Error> {
        let Some(entry) = self.table.get_mut(&ir.num) else {
            return Err(Error::Corrupt(format!(
                "locateObjForIndRef: no xref entry found for obj #{}",
                ir.num
            )));
        };

        // Check for multiple indRefs.
        if entry.ref_count > 1 {
            entry.ref_count -= 1;
            // By returning nil we signal this object is still in use and can't be deleted.
            return Ok(None);
        }

        // Since this is the only indRef we can move on and delete the entire object graph.
        let r = Object::Ref(ir);
        Ok(match self.deref_c(&r, cancel)? {
            Object::Null => None,
            o => Some(o.clone()),
        })
    }

    /// `FreeObject`: marks the entry of `obj_nr` as free and inserts it into the free list right
    /// after the head (7.5.4).
    ///
    /// # Errors
    /// [`Error::Corrupt`] if there is no free list head or no entry for `obj_nr`, and the
    /// errors of [`XRefTable::free`]. [`Error::Panic`] where Go increments a nil generation.
    pub fn free_object(&mut self, obj_nr: i64) -> Result<(), Error> {
        let head_offset = match self.free(0)? {
            Some(head) => head.offset,
            None => {
                return Err(Error::Corrupt(
                    "freeObject: missing free list head".to_string(),
                ));
            }
        };

        let Some(entry) = self.table.get_mut(&obj_nr) else {
            return Err(Error::Corrupt(format!(
                "freeObject: no entry for obj #{obj_nr}"
            )));
        };

        if entry.free {
            return Ok(());
        }

        let Some(generation) = entry.generation.as_mut() else {
            return Err(Error::Panic("freeObject: nil generation"));
        };
        *generation = generation.saturating_add(1);
        entry.free = true;
        entry.compressed = false;
        entry.offset = head_offset;
        entry.object = None;
        entry.ref_count = 0;

        // The head is a different entry from `entry` here: `free(0)` found it free, and
        // `entry` was not.
        if let Some(head) = self.table.get_mut(&0) {
            head.offset = Some(obj_nr);
        }
        Ok(())
    }

    /// The step of `objectDeletion.delete`: frees the object that `o` refers to, if `o` is a
    /// reference, and pushes what the object holds on the stack of the walk.
    fn delete_step(
        &mut self,
        o: Object,
        processed: &mut BTreeSet<i64>,
        stack: &mut Vec<Object>,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let o = match o {
            Object::Ref(ir) => {
                if processed.contains(&ir.num) {
                    return Ok(());
                }
                let Some(target) = self.locate_obj_for_ind_ref(ir, cancel)? else {
                    return Ok(());
                };
                self.free_object(ir.num)?;
                processed.insert(ir.num);
                target
            }
            o => o,
        };

        match o {
            Object::Dict(d) => push_dict(d, stack),
            Object::Stream(sd) => {
                let StreamDict { dict, .. } = *sd;
                push_dict(dict, stack);
            }
            Object::Array(a) if !a.is_empty() => stack.extend(a.into_iter().rev()),
            _ => {}
        }
        Ok(())
    }

    /// `DeleteObject`: frees `o` and the objects that are reachable only through it, stopping
    /// when `cancel` is set. A cancellation can leave the graph partially deleted. A target
    /// that other references still point to is freed when the last of them is deleted.
    ///
    /// # Errors
    /// [`Error::Cancelled`]; [`Error::Corrupt`] for a reference to an object that has no
    /// entry; the errors of [`XRefTable::free_object`] and of the dereferencing.
    pub fn delete_object(&mut self, o: &Object, cancel: &AtomicBool) -> Result<(), Error> {
        let mut processed = BTreeSet::new();
        let mut stack = vec![o.clone()];
        while let Some(o) = stack.pop() {
            check_cancel(cancel)?;
            self.delete_step(o, &mut processed, &mut stack, cancel)?;
        }
        Ok(())
    }

    /// `DeleteDictEntry`: removes `key` from `d` and deletes the object graph it refers to. `d`
    /// must not be an object of the table; see [`XRefTable::delete_dict_entry_at`].
    ///
    /// # Errors
    /// As [`XRefTable::delete_object`].
    pub fn delete_dict_entry(
        &mut self,
        d: &mut Dict,
        key: &[u8],
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        check_cancel(cancel)?;
        let Some(o) = d.find(key).cloned() else {
            return Ok(());
        };
        self.delete_object(&o, cancel)?;
        d.delete(key);
        Ok(())
    }

    /// `DeleteDictEntry` for the dictionary of the indirect object `owner`.
    ///
    /// # Errors
    /// As [`XRefTable::delete_object`], and the errors of [`XRefTable::dereference_dict_mut`].
    pub fn delete_dict_entry_at(
        &mut self,
        owner: Ref,
        key: &[u8],
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        check_cancel(cancel)?;
        let o = self
            .dereference_dict_mut(owner)?
            .and_then(|d| d.find(key).cloned());
        let Some(o) = o else {
            return Ok(());
        };
        self.delete_object(&o, cancel)?;
        if let Some(d) = self.dereference_dict_mut(owner)? {
            d.delete(key);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Page content

    /// `decodeContentStream`: decodes `sd`. A corrupt Flate stream, and one with a bad checksum,
    /// are skipped: relaxed validation takes what was decoded.
    fn decode_content_stream(
        &mut self,
        sd: &mut StreamDict,
        page_nr: i64,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let Err(e) = self.decode_stream(sd, cancel) else {
            return Ok(());
        };
        let skip = matches!(
            e.root(),
            Error::Flate(deflate::Error::Corrupt(_) | deflate::Error::Checksum)
        );
        if skip {
            // Go: ShowSkipped("page %d: corrupt content stream (flate)").
            return Ok(());
        }
        Err(e.context(format!("page {page_nr} content decode")))
    }

    /// `appendPageContent`: decodes `sd` and appends its content to `acc`. The combined content
    /// is bound by the decode limit and charged to the ledger.
    fn append_page_content(
        &mut self,
        acc: &mut Combined,
        sd: &mut StreamDict,
        page_nr: i64,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        self.decode_content_stream(sd, page_nr, cancel)?;
        let content: &[u8] = sd.content.as_deref().unwrap_or(&[]);

        let limit = self.conf.limits.decode_limit();
        let have = acc.bytes.len() as u64;
        let more = content.len() as u64;
        if more > limit.saturating_sub(have) {
            return Err(Error::DecodeLimit.context(format!("page {page_nr} combined content")));
        }

        match acc.held.as_mut() {
            Some(h) => h.grow_by(more)?,
            None => acc.held = Some(self.mem().charge(more)?),
        }
        acc.bytes.try_reserve(content.len())?;
        acc.bytes.extend_from_slice(content);
        Ok(())
    }

    /// `PageContent`: the content, in PDF syntax, of the page dictionary `d`. The combined
    /// content of an array of streams is bound by the decode limit. A `/Contents` that does not
    /// resolve gives empty content.
    ///
    /// # Errors
    /// [`Error::NoContent`] if there is no `/Contents`, or the streams have no content;
    /// [`Error::DecodeLimit`] over the decode limit; the decode errors, with the context
    /// `page N content decode`; [`Error::Corrupt`] if `/Contents` is neither a stream nor an
    /// array.
    pub fn page_content(
        &mut self,
        d: &Dict,
        page_nr: i64,
        cancel: &AtomicBool,
    ) -> Result<Bytes, Error> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let Some(o) = d.find(b"Contents").filter(|o| !o.is_null()) else {
            return Err(Error::NoContent);
        };

        let o = self.deref_c(o, cancel)?.clone();
        if o.is_null() {
            // Go returns nil bytes and no error for a `/Contents` that does not resolve.
            return self.mem().bytes_from(&[], 0);
        }
        let mut acc = Combined::default();

        match o {
            // No further processing.
            Object::Stream(mut sd) => {
                self.append_page_content(&mut acc, &mut sd, page_nr, cancel)?;
            }

            // Process an array of content stream dicts.
            Object::Array(a) => {
                for o in a {
                    if o.is_null() {
                        continue;
                    }
                    let (sd, _) = self
                        .dereference_stream_dict(&o)
                        .map_err(|e| e.context(format!("page {page_nr} content decode")))?;
                    let Some(mut sd) = sd else {
                        continue;
                    };
                    self.append_page_content(&mut acc, &mut sd, page_nr, cancel)?;
                }
            }

            _ => {
                return Err(Error::Corrupt(
                    "page content must be stream dict or array".to_string(),
                ));
            }
        }

        if acc.bytes.is_empty() {
            return Err(Error::NoContent);
        }
        Ok(Bytes::new(acc.bytes, 0, acc.held))
    }

    /// `pageContentContext`: what the `/Contents` of the page dictionary `d` is, for messages.
    fn page_content_context(&mut self, d: &Dict) -> String {
        let (obj_nr, kind) = match d.0.get(b"Contents".as_slice()) {
            Some(Object::Ref(ir)) => {
                let r = Object::Ref(*ir);
                match self.dereference(&r) {
                    Ok(o) => (ir.num, content_kind(o)),
                    Err(_) => return "page content".to_string(),
                }
            }
            Some(o) => (0, content_kind(o)),
            None => (0, "page content"),
        };
        if obj_nr > 0 {
            format!("{kind} obj#{obj_nr}")
        } else {
            kind.to_string()
        }
    }

    // -----------------------------------------------------------------------------------------
    // Consolidating resources

    /// `consolidateResources`: takes over the resource dictionary `obj` of a page tree node
    /// into the inherited resources. The first one becomes the inherited resources, with every
    /// entry resolved; the ones below add their sub dictionaries, and their names override
    /// the names of the nodes above.
    ///
    /// # Errors
    /// The errors of the dereferencing, with the context `resource <key>`; [`Error::Corrupt`]
    /// for an inherited entry that is no dictionary.
    pub(super) fn consolidate_resources(
        &mut self,
        obj: &Object,
        attrs: &mut InheritedPageAttrs,
    ) -> Result<(), Error> {
        let Some(d) = self.dereference_dict(obj)?.cloned() else {
            return Ok(());
        };
        if d.is_empty() {
            return Ok(());
        }

        let Some(res) = attrs.resources.as_mut() else {
            // Create a resource dict that eventually will contain any inherited resources
            // walking down from page root to leaf node representing the page in question.
            let mut res = d;
            let keys: Vec<Vec<u8>> = res.0.keys().cloned().collect();
            for k in keys {
                let v = res.0.get(&k).cloned().unwrap_or(Object::Null);
                let o = self
                    .dereference(&v)
                    .map_err(|e| e.context(format!("resource {}", String::from_utf8_lossy(&k))))?
                    .clone();
                if o.is_null() {
                    // A missing reference is null, equivalent to an omitted entry (PDF 2.0 7.3.9).
                    res.0.remove(&k);
                    continue;
                }
                res.0.insert(k, o);
            }
            attrs.resources = Some(res);
            return Ok(());
        };

        // Accumulate any resources defined in this page node into the inherited resources.
        for (k, v) in d.iter() {
            if k.as_slice() == b"ProcSet" || v.is_null() {
                continue;
            }
            let Some(d1) = self
                .dereference_dict(v)
                .map_err(|e| e.context(format!("resource {}", String::from_utf8_lossy(k))))?
                .cloned()
            else {
                continue;
            };
            // We have identified a subdict that needs to go into the inherited res dict.
            match res.0.get_mut(k) {
                None | Some(Object::Null) => {
                    res.0.insert(k.clone(), Object::Dict(d1));
                }
                // Weave sub dict d1 into inherited sub dict d2. Any existing resource names
                // will be overridden.
                Some(Object::Dict(d2)) => d2.0.extend(d1.0),
                Some(other) => return Err(expected_dict_error(other)),
            }
        }

        Ok(())
    }

    /// `consolidateResourceSubDict`: the resource dictionary `key` of `d` keeps the entries that
    /// the page needs. Without needs it is removed. A missing required dictionary is skipped.
    ///
    /// # Errors
    /// The errors of the dereferencing, with the context `page N: resource <key>`.
    fn consolidate_resource_sub_dict(
        &mut self,
        d: &mut Dict,
        key: &'static str,
        prn: &PageResourceNames,
        page_nr: i64,
    ) -> Result<(), Error> {
        let k = key.as_bytes();
        let Some(res) = prn.resources(key).filter(|r| !r.is_empty()) else {
            d.delete(k);
            return Ok(());
        };
        let o = d.0.get(k).cloned().unwrap_or(Object::Null);
        let d1 = self
            .dereference_dict(&o)
            .map_err(|e| e.context(format!("page {page_nr}: resource {key}")))?
            .cloned();
        let Some(mut d1) = d1 else {
            // A missing required resource dictionary: Go shows a skipped message.
            d.delete(k);
            return Ok(());
        };
        // Iterate over inherited resource sub dict and remove any entries not required. A
        // required entry that is missing: Go shows a skipped message.
        d1.0.retain(|name, _| res.contains(name));
        d.set(k, Object::Dict(d1));
        Ok(())
    }

    /// `consolidateResourceDict`: [`XRefTable::consolidate_resource_sub_dict`] for every kind.
    fn consolidate_resource_dict(
        &mut self,
        d: &mut Dict,
        prn: &PageResourceNames,
        page_nr: i64,
    ) -> Result<(), Error> {
        for k in RESOURCE_TYPES {
            self.consolidate_resource_sub_dict(d, k, prn, page_nr)?;
        }
        Ok(())
    }

    /// `consolidateResourcesWithContent` with `consolidateRes=true`: parses the content of the
    /// page `page_dict` and cuts `res_dict` down to what the content uses.
    ///
    /// # Errors
    /// The errors of [`XRefTable::page_content`] and [`parse_content`] with the context of the
    /// content (`content stream obj#N`), and of the consolidation (`resource dict`). A page
    /// without content is not an error.
    fn consolidate_resources_with_content(
        &mut self,
        page_dict: &Dict,
        res_dict: &mut Dict,
        page_nr: i64,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let context = self.page_content_context(page_dict);
        let bb = match self.page_content(page_dict, page_nr, cancel) {
            Ok(bb) => bb,
            Err(Error::NoContent) => return Ok(()),
            Err(e) => return Err(e.context(context)),
        };

        // Calculate resources required by the content stream of this page.
        // The names are charged while the page is consolidated.
        let pdf20 = self.pdf20()?;
        let (prn, _names) =
            parse_content(&bb, true, pdf20, self.mem(), cancel).map_err(|e| e.context(context))?;

        // Compare required resources (prn) with available resources (pAttrs.resources). Remove
        // any resource that's not required.
        self.consolidate_resource_dict(res_dict, &prn, page_nr)
            .map_err(|e| e.context("resource dict"))
    }

    /// `consolidatePageResourcesForNode`.
    fn consolidate_node(
        &mut self,
        w: &mut ConsolidateWalk<'_>,
        root: Ref,
        attrs: &InheritedPageAttrs,
        depth: u64,
    ) -> Result<(), Error> {
        check_cancel(w.cancel)?;
        self.check_recursion_depth("page tree", depth)?;

        let node = self
            .page_node(root, w.cancel)?
            .ok_or(Error::Sentinel(Sentinel::PageNotFound))?;
        // This node's copy of the inherited resources is charged before it is made; what the
        // node's own resources add to it, right after the merge.
        let res_bytes = |a: &InheritedPageAttrs| a.resources.as_ref().map_or(0, dict_copy_bytes);
        let mut copy = self.mem().charge(res_bytes(attrs))?;
        let mut attrs = attrs.clone();
        self.check_inherited_page_attrs(&node.dict, &mut attrs, true)?;
        copy.grow_by(res_bytes(&attrs).saturating_sub(copy.bytes()))?;

        let Some(n) = node.kids else {
            let current = w.page_nr.saturating_add(1);
            let mut res = attrs.resources.take().unwrap_or_default();
            if let Err(e) =
                self.consolidate_resources_with_content(&node.dict, &mut res, current, w.cancel)
            {
                w.page_error = true;
                return Err(e.context(format!("page {current} obj#{}", root.num)));
            }
            if !res.is_empty() {
                // The copy moves into the page, so its charge moves to the table's.
                let n = copy.bytes();
                drop(copy);
                self.held.grow_by(n)?;
                if let Some(d) = self.dereference_dict_mut(root)? {
                    d.set(b"Resources", Object::Dict(res));
                }
            }
            w.page_nr = current;
            return Ok(());
        };

        w.visit.enter(root.num)?;
        let r = self.consolidate_kids(w, root, n, &attrs, depth);
        w.visit.leave(root.num);
        r
    }

    /// The loop over the `n` kids of `root` in `consolidatePageResourcesForNode`.
    fn consolidate_kids(
        &mut self,
        w: &mut ConsolidateWalk<'_>,
        root: Ref,
        n: usize,
        attrs: &InheritedPageAttrs,
        depth: u64,
    ) -> Result<(), Error> {
        for i in 0..n {
            let kid = match self.page_node_kid(root, i, w.cancel)? {
                None | Some(Object::Null) => continue,
                Some(o) => o,
            };
            self.consolidate_kid(w, &kid, attrs, depth)?;
        }
        Ok(())
    }

    /// `consolidatePageResourcesForKid`: a node of any other type than `Pages` and `Page` is
    /// skipped in relaxed mode.
    fn consolidate_kid(
        &mut self,
        w: &mut ConsolidateWalk<'_>,
        kid: &Object,
        attrs: &InheritedPageAttrs,
        depth: u64,
    ) -> Result<(), Error> {
        let Object::Ref(r) = kid else {
            return Err(Error::Corrupt("corrupt page node dict".to_string()));
        };

        let ty = self.page_obj_type(*r, w.cancel)?;
        if ty != b"Pages" && ty != b"Page" {
            return Ok(());
        }
        self.consolidate_node(w, *r, attrs, depth + 1)
    }

    /// `ConsolidatePageResources`: cuts the resource dictionary of every page down to what its
    /// content uses, in one pass over the page tree. The inherited resources of the page tree
    /// nodes move into the dictionary of the page.
    ///
    /// # Errors
    /// [`Error::Cancelled`]; the errors of the page tree walk, with the context `page N:
    /// resource dict`; the errors of [`XRefTable::consolidate_resources_with_content`] with the
    /// context `page N obj#M`.
    pub fn consolidate_page_resources(&mut self, cancel: &AtomicBool) -> Result<(), Error> {
        let root = self
            .pages()
            .map_err(|e| e.context("page 1: resource dict"))?;

        let mut w = ConsolidateWalk {
            cancel,
            page_nr: 0,
            visit: PageTreeVisit::new(),
            page_error: false,
        };
        match self.consolidate_node(&mut w, root, &InheritedPageAttrs::default(), 0) {
            Ok(()) => Ok(()),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(e) if w.page_error => Err(e),
            Err(e) => Err(e.context(format!(
                "page {}: resource dict",
                w.page_nr.saturating_add(1)
            ))),
        }
    }
}

/// `pushDict`: the entries of `d` go on the stack, so that the last key is walked first.
fn push_dict(d: Dict, stack: &mut Vec<Object>) {
    stack.extend(d.0.into_values());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::model::context::Version;
    use crate::model::xref::{Entry, EntryObject};
    use crate::object::PdfFilter;

    fn r(n: i64) -> Ref {
        Ref {
            num: n,
            generation: 0,
        }
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn obj(entries: &[(&str, Object)]) -> Object {
        Object::Dict(dict(entries))
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn refs(nums: &[i64]) -> Object {
        Object::Array(nums.iter().map(|&n| Object::Ref(r(n))).collect())
    }

    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// A table with a free list head and a PDF 1.7 header.
    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x.table.insert(0, Entry::free_head());
        x
    }

    fn with_limits(limits: Limits) -> XRefTable {
        let mut x = XRefTable::new(Config {
            limits,
            ..Config::default()
        });
        x.header_version = Some(Version::V17);
        x.table.insert(0, Entry::free_head());
        x
    }

    fn stream_dict(x: &XRefTable, raw: &[u8]) -> StreamDict {
        StreamDict {
            raw: Some(x.mem().bytes_from(raw, raw.len()).unwrap()),
            ..StreamDict::default()
        }
    }

    fn stream(x: &XRefTable, raw: &[u8]) -> Object {
        Object::Stream(Box::new(stream_dict(x, raw)))
    }

    fn filtered(x: &XRefTable, raw: &[u8], filters: &[&str]) -> Object {
        let mut sd = stream_dict(x, raw);
        sd.filter_pipeline = filters
            .iter()
            .map(|f| PdfFilter {
                name: f.as_bytes().to_vec(),
                decode_parms: None,
            })
            .collect();
        Object::Stream(Box::new(sd))
    }

    /// An entry that is not a plain object: dereferencing it is an error.
    fn bad_entry() -> Entry {
        Entry {
            generation: Some(0),
            object: Some(EntryObject::ObjStm(Box::default())),
            ..Entry::default()
        }
    }

    /// The zlib stream of `content` as one stored block, with its Adler-32 checksum.
    const ZLIB_CONTENT: [u8; 18] = [
        0x78, 0x01, 0x01, 0x07, 0x00, 0xf8, 0xff, b'c', b'o', b'n', b't', b'e', b'n', b't', 0x0b,
        0xcb, 0x02, 0xfc,
    ];

    fn zlib_content(corrupt_checksum: bool) -> Vec<u8> {
        let mut z = ZLIB_CONTENT.to_vec();
        if corrupt_checksum {
            z[17] ^= 0xff;
        }
        z
    }

    fn entry_of(x: &XRefTable, n: i64) -> &Entry {
        x.find(n).unwrap()
    }

    // ---------------------------------------------------------------------------------------
    // PageResourceNames

    #[test]
    fn page_resource_names() {
        let mut prn = PageResourceNames::new();
        assert_eq!(prn.0.len(), 7);
        assert!(prn.0.keys().copied().eq(RESOURCE_TYPES));
        assert!(!prn.has_content());
        assert!(!prn.has_resources("Font"));
        assert!(!prn.has_resources("Unknown"));
        assert!(prn.resources("Unknown").is_none());
        prn.insert("Font", b"F1".to_vec());
        prn.insert("Font", b"F1".to_vec());
        assert!(prn.has_content() && prn.has_resources("Font"));
        assert!(!prn.has_resources("XObject"));
        assert_eq!(prn.resources("Font").map(BTreeSet::len), Some(1));
    }

    // ---------------------------------------------------------------------------------------
    // FreeObject and DeleteObject

    #[test]
    fn free_object_inserts_after_the_head() {
        let mut x = table();
        x.table.insert(5, Entry::gen0(Object::Integer(1)));
        x.table.insert(6, Entry::gen0(Object::Integer(2)));
        x.free_object(5).unwrap();
        let e = entry_of(&x, 5);
        assert!(e.free && !e.compressed && e.object.is_none());
        assert_eq!((e.generation, e.offset, e.ref_count), (Some(1), Some(0), 0));
        assert_eq!(entry_of(&x, 0).offset, Some(5));

        // Freeing a free object changes nothing.
        x.free_object(5).unwrap();
        assert_eq!(entry_of(&x, 5).generation, Some(1));
        assert_eq!(entry_of(&x, 0).offset, Some(5));
        x.free_object(0).unwrap();
        assert_eq!(entry_of(&x, 0).offset, Some(5));

        x.free_object(6).unwrap();
        assert_eq!(entry_of(&x, 6).offset, Some(5));
        assert_eq!(entry_of(&x, 0).offset, Some(6));
    }

    #[test]
    fn free_object_errors() {
        let mut x = table();
        assert_eq!(
            x.free_object(99),
            Err(Error::Corrupt(
                "freeObject: no entry for obj #99".to_string()
            ))
        );
        x.table.insert(
            4,
            Entry {
                object: Some(EntryObject::Obj(Object::Null)),
                ..Entry::default()
            },
        );
        assert!(matches!(x.free_object(4), Err(Error::Panic(_))));
        assert!(!entry_of(&x, 4).free);

        let mut x = XRefTable::new(Config::default());
        x.table.insert(4, Entry::gen0(Object::Null));
        assert_eq!(
            x.free_object(4),
            Err(Error::Corrupt(
                "freeObject: missing free list head".to_string()
            ))
        );
        // The head is not free.
        x.table.insert(0, Entry::gen0(Object::Null));
        assert!(x.free_object(4).is_err());
    }

    #[test]
    fn delete_object_frees_the_graph_and_counts_references() {
        let mut x = table();
        x.table.insert(
            1,
            Entry::gen0(obj(&[("A", Object::Ref(r(2))), ("B", refs(&[3]))])),
        );
        // Two references: from object 1 and from object 3.
        x.table.insert(
            2,
            Entry {
                ref_count: 2,
                ..Entry::gen0(Object::Integer(7))
            },
        );
        x.table
            .insert(3, Entry::gen0(obj(&[("C", Object::Ref(r(2)))])));
        x.delete_object(&Object::Ref(r(1)), &cancel()).unwrap();

        // The order is 1, then B (3), then C (2, which still has a reference), then A (2).
        for n in [1, 2, 3] {
            let e = entry_of(&x, n);
            assert!(e.free && e.object.is_none() && e.ref_count == 0, "{n}");
        }
        assert_eq!(entry_of(&x, 0).offset, Some(2));
        assert_eq!(entry_of(&x, 2).offset, Some(3));
        assert_eq!(entry_of(&x, 3).offset, Some(1));
        assert_eq!(entry_of(&x, 1).offset, Some(0));
    }

    #[test]
    fn delete_object_keeps_an_object_that_is_referenced_elsewhere() {
        let mut x = table();
        x.table.insert(
            3,
            Entry {
                ref_count: 2,
                ..Entry::gen0(Object::Integer(7))
            },
        );
        x.delete_object(&Object::Ref(r(3)), &cancel()).unwrap();
        let e = entry_of(&x, 3);
        assert!(!e.free && e.ref_count == 1 && e.object.is_some());
        assert_eq!(entry_of(&x, 0).offset, Some(0));
        // The second delete frees it.
        x.delete_object(&Object::Ref(r(3)), &cancel()).unwrap();
        assert!(entry_of(&x, 3).free);
    }

    #[test]
    fn delete_object_walks_direct_objects_and_streams() {
        let mut x = table();
        let sd = StreamDict {
            dict: dict(&[("Parent", Object::Ref(r(7)))]),
            ..StreamDict::default()
        };
        x.table.insert(7, Entry::gen0(Object::Integer(1)));
        x.table.insert(8, Entry::gen0(Object::Integer(2)));
        x.table.insert(9, Entry::gen0(Object::Integer(3)));
        let direct = obj(&[
            ("S", Object::Stream(Box::new(sd))),
            ("L", Object::Array(vec![Object::Null, Object::Ref(r(8))])),
            ("N", Object::Array(Vec::new())),
            ("D", obj(&[])),
            ("Z", Object::Ref(r(9))),
        ]);
        x.delete_object(&direct, &cancel()).unwrap();
        for n in [7, 8, 9] {
            assert!(entry_of(&x, n).free, "{n}");
        }
        // The last key is walked first: Z, then S (Parent 7), then L (8).
        assert_eq!(entry_of(&x, 9).offset, Some(0));
        assert_eq!(entry_of(&x, 7).offset, Some(9));
        assert_eq!(entry_of(&x, 8).offset, Some(7));
    }

    #[test]
    fn delete_object_stops_at_cycles() {
        let mut x = table();
        x.table.insert(1, Entry::gen0(refs(&[2])));
        x.table.insert(2, Entry::gen0(refs(&[1])));
        x.delete_object(&Object::Ref(r(1)), &cancel()).unwrap();
        assert!(entry_of(&x, 1).free && entry_of(&x, 2).free);
        assert_eq!(entry_of(&x, 0).offset, Some(2));
    }

    #[test]
    fn delete_object_errors() {
        let mut x = table();
        assert_eq!(
            x.delete_object(&Object::Ref(r(99)), &cancel()),
            Err(Error::Corrupt(
                "locateObjForIndRef: no xref entry found for obj #99".to_string()
            ))
        );
        // A null target is not freed.
        x.table.insert(4, Entry::gen0(Object::Null));
        x.delete_object(&Object::Ref(r(4)), &cancel()).unwrap();
        assert!(!entry_of(&x, 4).free);
        // An entry that is no plain object is an error.
        x.table.insert(5, bad_entry());
        assert!(x.delete_object(&Object::Ref(r(5)), &cancel()).is_err());
        // Scalars and null are nothing to delete.
        x.delete_object(&Object::Integer(1), &cancel()).unwrap();
        x.delete_object(&Object::Null, &cancel()).unwrap();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            x.delete_object(&Object::Null, &cancelled),
            Err(Error::Cancelled)
        );
    }

    #[test]
    fn delete_dict_entry_removes_the_key_and_the_graph() {
        let mut x = table();
        x.table.insert(4, Entry::gen0(obj(&[])));
        x.table
            .insert(5, Entry::gen0(obj(&[("PieceInfo", Object::Ref(r(4)))])));
        let mut d = dict(&[
            ("PieceInfo", Object::Ref(r(4))),
            ("Keep", Object::Integer(1)),
        ]);
        x.delete_dict_entry(&mut d, b"Missing", &cancel()).unwrap();
        assert_eq!(d.len(), 2);
        x.delete_dict_entry(&mut d, b"PieceInfo", &cancel())
            .unwrap();
        assert!(!d.has_entry(b"PieceInfo") && d.has_entry(b"Keep"));
        assert!(entry_of(&x, 4).free);

        // The dictionary of an object of the table.
        x.table.insert(6, Entry::gen0(obj(&[])));
        x.table.insert(
            7,
            Entry::gen0(obj(&[
                ("PieceInfo", Object::Ref(r(6))),
                ("Keep", Object::Integer(1)),
            ])),
        );
        x.delete_dict_entry_at(r(7), b"PieceInfo", &cancel())
            .unwrap();
        assert!(entry_of(&x, 6).free);
        let d = x.dereference_dict_ref(r(7)).unwrap().unwrap();
        assert!(!d.has_entry(b"PieceInfo") && d.has_entry(b"Keep"));
        x.delete_dict_entry_at(r(7), b"PieceInfo", &cancel())
            .unwrap();
        x.delete_dict_entry_at(r(99), b"PieceInfo", &cancel())
            .unwrap();
        assert!(
            x.delete_dict_entry_at(r(5), b"PieceInfo", &cancel())
                .is_ok()
        );
    }

    // ---------------------------------------------------------------------------------------
    // PageContent

    // Port of TestPageContentPreservesUnsupportedFilter.
    #[test]
    fn page_content_preserves_an_unsupported_filter() {
        let mut x = table();
        let sd = filtered(&x, b"content", &["JBIG2Decode", "FlateDecode"]);
        let d = dict(&[("Contents", sd)]);
        let err = x.page_content(&d, 3, &cancel()).unwrap_err();
        assert!(matches!(err.root(), Error::UnsupportedFilter(_)), "{err}");
        assert!(err.to_string().contains("page 3 content decode"), "{err}");
    }

    // Port of TestPageContentArrayPreservesUnsupportedFilter.
    #[test]
    fn page_content_array_preserves_an_unsupported_filter() {
        let mut x = table();
        let sd = filtered(&x, b"content", &["JBIG2Decode", "FlateDecode"]);
        x.table.insert(7, Entry::gen0(sd));
        let d = dict(&[("Contents", refs(&[7]))]);
        let err = x.page_content(&d, 4, &cancel()).unwrap_err();
        assert!(matches!(err.root(), Error::UnsupportedFilter(_)), "{err}");
        assert!(err.to_string().contains("page 4 content decode"), "{err}");
    }

    #[test]
    fn page_content_concatenates_the_streams_of_an_array() {
        let mut x = table();
        let (a, b) = (stream(&x, b"q "), stream(&x, b"Q"));
        x.table.insert(7, Entry::gen0(a));
        x.table.insert(8, Entry::gen0(b));
        // An entry that is not loaded is null.
        x.table.insert(
            9,
            Entry {
                generation: Some(0),
                ..Entry::default()
            },
        );
        let contents = Object::Array(vec![
            Object::Ref(r(7)),
            Object::Null,
            Object::Ref(r(9)),
            Object::Ref(r(99)),
            Object::Ref(r(8)),
        ]);
        let d = dict(&[("Contents", contents)]);
        let bb = x.page_content(&d, 1, &cancel()).unwrap();
        assert_eq!(&*bb, b"q Q");
        // The combined content is charged until it drops.
        assert!(x.mem().used() >= 3);
        let before = x.mem().used();
        drop(bb);
        assert_eq!(x.mem().used(), before - 3);
    }

    #[test]
    fn page_content_without_content() {
        let mut x = table();
        assert_eq!(
            x.page_content(&Dict::default(), 1, &cancel()).unwrap_err(),
            Error::NoContent
        );
        let d = dict(&[("Contents", Object::Null)]);
        assert_eq!(
            x.page_content(&d, 1, &cancel()).unwrap_err(),
            Error::NoContent
        );
        // Streams without bytes.
        let d = dict(&[("Contents", stream(&x, b""))]);
        assert_eq!(
            x.page_content(&d, 1, &cancel()).unwrap_err(),
            Error::NoContent
        );
        let d = dict(&[("Contents", Object::Array(vec![Object::Null]))]);
        assert_eq!(
            x.page_content(&d, 1, &cancel()).unwrap_err(),
            Error::NoContent
        );
        // A reference that does not resolve is empty content, not an error.
        let d = dict(&[("Contents", Object::Ref(r(99)))]);
        assert!(x.page_content(&d, 1, &cancel()).unwrap().is_empty());
        let d = dict(&[("Contents", Object::Integer(1))]);
        assert_eq!(
            x.page_content(&d, 1, &cancel()).unwrap_err(),
            Error::Corrupt("page content must be stream dict or array".to_string())
        );
        let d = dict(&[("Contents", Object::Array(vec![Object::Integer(1)]))]);
        let err = x.page_content(&d, 2, &cancel()).unwrap_err();
        assert!(err.to_string().contains("page 2 content decode"), "{err}");
    }

    // Port of TestPageContentCombinedDecodeLimit (streams that are not filtered).
    #[test]
    fn page_content_combined_decode_limit() {
        let content = b"q Q\n".repeat(8);
        for limit in [63, 64, 65, 0] {
            let mut x = with_limits(Limits {
                max_decode_bytes: limit,
                ..Limits::default()
            });
            let sd = stream(&x, &content);
            let page = dict(&[(
                "Contents",
                Object::Array(vec![sd.clone(), Object::Null, sd]),
            )]);
            let got = x.page_content(&page, 1, &cancel());
            if limit == 63 {
                let err = got.unwrap_err();
                assert!(err.is_decode_limit_exceeded(), "{err}");
                assert!(err.to_string().contains("page 1 combined content"), "{err}");
            } else {
                assert_eq!(&*got.unwrap(), b"q Q\n".repeat(16).as_slice(), "{limit}");
            }
        }
    }

    #[test]
    fn page_content_decodes_flate_and_relaxes_a_bad_checksum() {
        let mut x = table();
        let good = filtered(&x, &zlib_content(false), &["FlateDecode"]);
        let d = dict(&[("Contents", good)]);
        assert_eq!(&*x.page_content(&d, 1, &cancel()).unwrap(), b"content");

        // Relaxed validation skips a stream that does not decode: no content.
        let bad = filtered(&x, &zlib_content(true), &["FlateDecode"]);
        let d = dict(&[("Contents", bad)]);
        assert_eq!(
            x.page_content(&d, 5, &cancel()).unwrap_err(),
            Error::NoContent
        );
        // Port of TestConsolidateResourcesRelaxesFlateChecksumError.
        let mut res = Dict::default();
        x.consolidate_resources_with_content(&d, &mut res, 5, &cancel())
            .unwrap();

        // Another error is not skipped.
        let bad = filtered(&x, b"not zlib", &["FlateDecode"]);
        let d = dict(&[("Contents", bad)]);
        let err = x.page_content(&d, 6, &cancel()).unwrap_err();
        assert!(err.to_string().contains("page 6 content decode"), "{err}");
    }

    #[test]
    fn page_content_honours_cancellation() {
        let mut x = table();
        let d = dict(&[("Contents", stream(&x, b"q Q"))]);
        let cancelled = AtomicBool::new(true);
        let err = x.page_content(&d, 1, &cancelled).unwrap_err();
        assert!(matches!(err.root(), Error::Cancelled), "{err}");
    }

    #[test]
    fn page_content_context_names_the_content() {
        let mut x = table();
        let s = stream(&x, b"q");
        x.table.insert(3, Entry::gen0(s));
        x.table.insert(4, Entry::gen0(refs(&[3])));
        x.table.insert(5, bad_entry());
        let ctx = |x: &mut XRefTable, o: Option<Object>| {
            let d = match o {
                Some(o) => dict(&[("Contents", o)]),
                None => Dict::default(),
            };
            x.page_content_context(&d)
        };
        assert_eq!(ctx(&mut x, None), "page content");
        assert_eq!(ctx(&mut x, Some(Object::Integer(1))), "page content");
        assert_eq!(ctx(&mut x, Some(Object::Ref(r(3)))), "content stream obj#3");
        assert_eq!(
            ctx(&mut x, Some(Object::Ref(r(4)))),
            "content stream array obj#4"
        );
        assert_eq!(ctx(&mut x, Some(Object::Ref(r(5)))), "page content");
        assert_eq!(ctx(&mut x, Some(Object::Ref(r(99)))), "page content obj#99");
        assert_eq!(ctx(&mut x, Some(refs(&[3]))), "content stream array");
    }

    // ---------------------------------------------------------------------------------------
    // Consolidating the resources

    // Port of TestConsolidateResourcesOmitsMissingCategoryReference.
    #[test]
    fn consolidate_resources_omits_a_missing_category_reference() {
        let mut x = table();
        let resources = obj(&[("Font", obj(&[])), ("Properties", Object::Ref(r(9)))]);
        let mut attrs = InheritedPageAttrs::default();
        x.consolidate_resources(&resources, &mut attrs).unwrap();
        let res = attrs.resources.unwrap();
        assert!(res.find(b"Properties").is_none());
        assert!(res.find(b"Font").is_some());
    }

    #[test]
    fn consolidate_resources_first_resolves_then_weaves() {
        let mut x = table();
        x.table
            .insert(8, Entry::gen0(obj(&[("F1", Object::Ref(r(20)))])));
        let mut attrs = InheritedPageAttrs::default();
        // An empty dictionary and null change nothing.
        x.consolidate_resources(&obj(&[]), &mut attrs).unwrap();
        x.consolidate_resources(&Object::Null, &mut attrs).unwrap();
        assert!(attrs.resources.is_none());

        let first = obj(&[
            ("Font", Object::Ref(r(8))),
            ("ProcSet", Object::Array(vec![name("PDF")])),
        ]);
        x.consolidate_resources(&first, &mut attrs).unwrap();
        let res = attrs.resources.clone().unwrap();
        assert!(res.dict_entry(b"Font").is_some_and(|f| f.has_entry(b"F1")));
        assert!(res.find(b"ProcSet").is_some());

        // Below: a reference, a null entry, ProcSet, a new kind, and a name that is overridden.
        x.table.insert(
            9,
            Entry::gen0(obj(&[
                ("F1", Object::Integer(1)),
                ("F2", Object::Integer(2)),
            ])),
        );
        let second = obj(&[
            ("Font", Object::Ref(r(9))),
            ("XObject", obj(&[("Im", Object::Integer(3))])),
            ("Shading", Object::Null),
            ("ProcSet", Object::Array(Vec::new())),
            ("Pattern", Object::Ref(r(99))),
        ]);
        x.consolidate_resources(&second, &mut attrs).unwrap();
        let res = attrs.resources.unwrap();
        let font = res.dict_entry(b"Font").unwrap();
        assert!(matches!(font.find(b"F1"), Some(Object::Integer(1))));
        assert!(font.has_entry(b"F2"));
        assert!(res.dict_entry(b"XObject").is_some());
        assert!(res.find(b"Shading").is_none() && res.find(b"Pattern").is_none());
        // ProcSet is not woven.
        assert!(matches!(res.find(b"ProcSet"), Some(Object::Array(a)) if a.len() == 1));
    }

    #[test]
    fn consolidate_resources_rejects_an_inherited_entry_that_is_no_dictionary() {
        let mut x = table();
        let mut attrs = InheritedPageAttrs::default();
        x.consolidate_resources(&obj(&[("Font", Object::Integer(1))]), &mut attrs)
            .unwrap();
        let err = x
            .consolidate_resources(&obj(&[("Font", obj(&[("F", Object::Null)]))]), &mut attrs)
            .unwrap_err();
        assert_eq!(
            err,
            Error::Corrupt("consolidateResources: expected Dict d2: types.Integer".to_string())
        );
        let err = x
            .consolidate_resources(&obj(&[("Font", Object::Integer(2))]), &mut attrs)
            .unwrap_err();
        assert!(err.to_string().contains("resource Font"), "{err}");
    }

    // Port of TestConsolidateResourcesSelectsLowestDictionaryKey.
    #[test]
    fn consolidate_resources_reports_the_lowest_dictionary_key() {
        for _ in 0..25 {
            let mut x = table();
            x.table.insert(3, bad_entry());
            x.table.insert(9, bad_entry());
            let d = obj(&[("Zulu", Object::Ref(r(9))), ("Alpha", Object::Ref(r(3)))]);
            let err = x
                .consolidate_resources(&d, &mut InheritedPageAttrs::default())
                .unwrap_err();
            assert!(err.to_string().contains("resource Alpha"), "{err}");
        }
    }

    // Port of TestConsolidateResourceSubDictDereferencesCategory (relaxed validation: a missing
    // required dictionary is skipped).
    #[test]
    fn consolidate_resource_sub_dict_dereferences_the_category() {
        struct Case {
            name: &'static str,
            object: Option<Object>,
            required: bool,
            want_type_error: bool,
            want_retained: bool,
        }
        let cases = [
            Case {
                name: "missing required",
                object: None,
                required: true,
                want_type_error: false,
                want_retained: false,
            },
            Case {
                name: "missing unused",
                object: None,
                required: false,
                want_type_error: false,
                want_retained: false,
            },
            Case {
                name: "wrong type required",
                object: Some(Object::Integer(1)),
                required: true,
                want_type_error: true,
                want_retained: true,
            },
            Case {
                name: "wrong type unused",
                object: Some(Object::Integer(1)),
                required: false,
                want_type_error: false,
                want_retained: false,
            },
            Case {
                name: "valid dictionary",
                object: Some(obj(&[
                    ("P1", Object::Integer(1)),
                    ("P2", Object::Integer(2)),
                ])),
                required: true,
                want_type_error: false,
                want_retained: true,
            },
        ];
        for c in cases {
            let mut x = table();
            if let Some(o) = c.object {
                x.table.insert(9, Entry::gen0(o));
            }
            let mut d = dict(&[("Properties", Object::Ref(r(9)))]);
            let mut prn = PageResourceNames::new();
            if c.required {
                prn.insert("Properties", b"P1".to_vec());
            }
            let err = x
                .consolidate_resource_sub_dict(&mut d, "Properties", &prn, 1)
                .err();
            if c.want_type_error {
                assert!(
                    err.as_ref().is_some_and(|e| e.is(Sentinel::ExpectedDict)),
                    "{}: {err:?}",
                    c.name
                );
            } else {
                assert_eq!(err, None, "{}", c.name);
            }
            assert_eq!(
                d.find(b"Properties").is_some(),
                c.want_retained,
                "{}",
                c.name
            );
        }

        // Only the required names stay.
        let mut x = table();
        let mut d = dict(&[(
            "Properties",
            obj(&[("P1", Object::Integer(1)), ("P2", Object::Integer(2))]),
        )]);
        let mut prn = PageResourceNames::new();
        prn.insert("Properties", b"P1".to_vec());
        prn.insert("Properties", b"P3".to_vec());
        x.consolidate_resource_sub_dict(&mut d, "Properties", &prn, 1)
            .unwrap();
        let kept = d.dict_entry(b"Properties").unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept.has_entry(b"P1"));
    }

    #[test]
    fn consolidate_resource_dict_goes_over_every_kind() {
        let mut x = table();
        let mut d = dict(&[
            (
                "Font",
                obj(&[("F1", Object::Integer(1)), ("F2", Object::Integer(2))]),
            ),
            ("XObject", obj(&[("Im", Object::Integer(3))])),
            ("ProcSet", Object::Array(Vec::new())),
        ]);
        let mut prn = PageResourceNames::new();
        prn.insert("Font", b"F1".to_vec());
        // A required kind that the dictionary does not have is skipped.
        prn.insert("Shading", b"S".to_vec());
        x.consolidate_resource_dict(&mut d, &prn, 1).unwrap();
        assert!(d.dict_entry(b"Font").is_some_and(|f| f.len() == 1));
        assert!(d.find(b"XObject").is_none() && d.find(b"Shading").is_none());
        assert!(d.find(b"ProcSet").is_some());
    }

    #[test]
    fn consolidate_resources_with_content_errors_name_the_content() {
        let mut x = table();
        let s = stream(&x, b"(a(b)");
        x.table.insert(3, Entry::gen0(s));
        let page = dict(&[("Contents", Object::Ref(r(3)))]);
        let err = x
            .consolidate_resources_with_content(&page, &mut Dict::default(), 1, &cancel())
            .unwrap_err();
        assert!(
            err.to_string().starts_with("content stream obj#3: "),
            "{err}"
        );
        assert!(matches!(err.root(), Error::Corrupt(_)), "{err}");

        let page = dict(&[("Contents", Object::Integer(1))]);
        let err = x
            .consolidate_resources_with_content(&page, &mut Dict::default(), 1, &cancel())
            .unwrap_err();
        assert!(err.to_string().starts_with("page content: "), "{err}");

        // No content, and no `/Contents`, are fine.
        let mut res = dict(&[("Font", obj(&[]))]);
        x.consolidate_resources_with_content(&Dict::default(), &mut res, 1, &cancel())
            .unwrap();
        assert!(res.find(b"Font").is_some());
    }

    /// Page tree: 1 (Pages, Font F1 and F9) with the pages 2 and 3.
    fn page_tree() -> XRefTable {
        let mut x = table();
        x.root_dict = Some(dict(&[("Pages", Object::Ref(r(1)))]));
        x.page_count = 2;
        let font = |names: &[(&str, i64)]| {
            obj(&names
                .iter()
                .map(|&(n, o)| (n, Object::Ref(r(o))))
                .collect::<Vec<_>>())
        };
        x.table.insert(
            1,
            Entry::gen0(obj(&[
                ("Type", name("Pages")),
                ("Count", Object::Integer(2)),
                ("Kids", refs(&[2, 3])),
                (
                    "Resources",
                    obj(&[("Font", font(&[("F1", 10), ("F9", 11)]))]),
                ),
            ])),
        );
        x.table.insert(
            2,
            Entry::gen0(obj(&[
                ("Type", name("Page")),
                ("Contents", Object::Ref(r(4))),
                (
                    "Resources",
                    obj(&[
                        ("Font", font(&[("F2", 14)])),
                        ("XObject", font(&[("Im1", 12), ("Im2", 13)])),
                    ]),
                ),
            ])),
        );
        x.table.insert(
            3,
            Entry::gen0(obj(&[
                ("Type", name("Page")),
                ("Contents", Object::Ref(r(5))),
            ])),
        );
        let s4 = stream(&x, b"/F1 12 Tf /F2 12 Tf /Im1 Do");
        let s5 = stream(&x, b"/F1 12 Tf");
        x.table.insert(4, Entry::gen0(s4));
        x.table.insert(5, Entry::gen0(s5));
        x
    }

    fn resources_of(x: &mut XRefTable, n: i64) -> Option<Dict> {
        x.dereference_dict_ref(r(n))
            .unwrap()
            .unwrap()
            .dict_entry(b"Resources")
            .cloned()
    }

    fn names_of(d: &Dict, kind: &str) -> Vec<String> {
        d.dict_entry(kind.as_bytes())
            .map(|f| {
                f.iter()
                    .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn consolidate_page_resources_cuts_the_resources_down_to_the_content() {
        let mut x = page_tree();
        x.consolidate_page_resources(&cancel()).unwrap();

        // Page 1 (object 2): the inherited F1, its own F2 and Im1.
        let res = resources_of(&mut x, 2).unwrap();
        assert_eq!(names_of(&res, "Font"), ["F1", "F2"]);
        assert_eq!(names_of(&res, "XObject"), ["Im1"]);
        assert_eq!(res.len(), 2);
        assert!(matches!(
            res.dict_entry(b"Font").and_then(|f| f.find(b"F2")),
            Some(Object::Ref(ir)) if ir.num == 14
        ));

        // Page 2 (object 3) had no resources of its own: it gets the inherited F1.
        let res = resources_of(&mut x, 3).unwrap();
        assert_eq!(names_of(&res, "Font"), ["F1"]);
        assert_eq!(res.len(), 1);

        // The page tree node keeps its resources.
        let root = x.dereference_dict_ref(r(1)).unwrap().unwrap();
        let font = root.dict_entry(b"Resources").unwrap();
        assert_eq!(names_of(font, "Font"), ["F1", "F9"]);
        assert!(root.array_entry(b"Kids").is_some_and(|k| k.len() == 2));
    }

    #[test]
    fn consolidate_page_resources_skips_other_nodes_and_keeps_inherited_resources_without_content()
    {
        let mut x = page_tree();
        // Object 2 is a template: skipped. Object 6 is a page with no content.
        x.table.insert(
            2,
            Entry::gen0(obj(&[
                ("Type", name("Template")),
                ("Contents", Object::Ref(r(4))),
            ])),
        );
        x.table.insert(
            6,
            Entry::gen0(obj(&[("Type", name("Page")), ("Resources", obj(&[]))])),
        );
        x.table.insert(
            1,
            Entry::gen0(obj(&[
                ("Type", name("Pages")),
                ("Count", Object::Integer(2)),
                (
                    "Kids",
                    Object::Array(vec![
                        Object::Null,
                        Object::Ref(r(2)),
                        Object::Ref(r(6)),
                        Object::Ref(r(3)),
                    ]),
                ),
                (
                    "Resources",
                    obj(&[("Font", obj(&[("F1", Object::Integer(1))]))]),
                ),
            ])),
        );
        x.consolidate_page_resources(&cancel()).unwrap();
        assert!(resources_of(&mut x, 2).is_none());
        // Without content nothing is cut: the page gets the inherited resources as they are.
        let res = resources_of(&mut x, 6).unwrap();
        assert_eq!(names_of(&res, "Font"), ["F1"]);
        let res = resources_of(&mut x, 3).unwrap();
        assert_eq!(names_of(&res, "Font"), ["F1"]);
    }

    #[test]
    fn consolidate_page_resources_errors() {
        // The error of a page names the page and the object.
        let mut x = page_tree();
        x.table.insert(
            2,
            Entry::gen0(obj(&[
                ("Type", name("Page")),
                ("Contents", Object::Integer(1)),
            ])),
        );
        let err = x.consolidate_page_resources(&cancel()).unwrap_err();
        assert!(err.to_string().starts_with("page 1 obj#2: "), "{err}");
        assert!(!err.to_string().contains("resource dict:"), "{err}");

        // The error of a node is told apart from it.
        let mut x = page_tree();
        x.table.insert(
            1,
            Entry::gen0(obj(&[
                ("Type", name("Pages")),
                ("Kids", Object::Array(vec![Object::Integer(3)])),
            ])),
        );
        assert_eq!(
            x.consolidate_page_resources(&cancel())
                .unwrap_err()
                .to_string(),
            "page 1: resource dict: corrupt page node dict"
        );

        // No catalog.
        let mut x = table();
        let err = x.consolidate_page_resources(&cancel()).unwrap_err();
        assert!(
            err.to_string().starts_with("page 1: resource dict: "),
            "{err}"
        );

        // A page tree that does not resolve.
        let mut x = page_tree();
        x.table.remove(&1);
        assert_eq!(
            x.consolidate_page_resources(&cancel()).unwrap_err(),
            Error::Sentinel(Sentinel::PageNotFound).context("page 1: resource dict")
        );

        // A cycle.
        let mut x = page_tree();
        x.table.insert(
            2,
            Entry::gen0(obj(&[("Type", name("Pages")), ("Kids", refs(&[1]))])),
        );
        let err = x.consolidate_page_resources(&cancel()).unwrap_err();
        assert!(err.is(Sentinel::PageTreeCycle), "{err}");

        // The recursion depth.
        let mut x = with_limits(Limits {
            max_recursion_depth: 1,
            ..Limits::default()
        });
        x.root_dict = Some(dict(&[("Pages", Object::Ref(r(1)))]));
        for n in 1..=3 {
            x.table.insert(
                n,
                Entry::gen0(obj(&[("Type", name("Pages")), ("Kids", refs(&[n + 1]))])),
            );
        }
        let err = x.consolidate_page_resources(&cancel()).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");

        // Cancellation is not wrapped.
        let mut x = page_tree();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            x.consolidate_page_resources(&cancelled),
            Err(Error::Cancelled)
        );
    }

    #[test]
    fn consolidate_page_resources_parses_content_in_pdf20_mode() {
        let mut x = page_tree();
        x.header_version = Some(Version::V20);
        // An inline image without a length is accepted in relaxed mode.
        let s = stream(&x, b"/F1 12 Tf BI /CS /G ID x EI /Im1 Do");
        x.table.insert(5, Entry::gen0(s));
        x.consolidate_page_resources(&cancel()).unwrap();
        let res = resources_of(&mut x, 3).unwrap();
        assert_eq!(names_of(&res, "Font"), ["F1"]);
    }
}
