//! The page tree (Go: pkg/pdfcpu/model/xreftable.go:2009-2200 `InheritedPageAttrs`,
//! `checkInheritedPageAttrs`; :2555-2754 `pageObjType`, `processPageTreeForPageDict`, `PageDict`;
//! :3002-3026 `EnsurePageCount`; recursion.go:137-170 `PageTreeVisit`).
//!
//! The configuration is the fixed one of docs/scope.md section 4.1: relaxed validation, so a
//! page tree node without `/Type` that has `/Count` and `/Kids` is a `Pages` node, and a node
//! of any other type is skipped without an error. `PageDict` is always called with
//! `consolidateRes=false`, so resource consolidation and content parsing are not ported here;
//! the optimize package adds them with `model/resources.rs`.
//!
//! Differences from Go:
//!
//! * [`XRefTable::page_dict`] returns a copy of the page dictionary and of the resources, where
//!   Go returns the maps stored in the table. Code that changes a page dictionary does it
//!   through [`XRefTable::dereference_dict_mut`] with the returned reference; inherited
//!   resources are changed through [`InheritedPageAttrs::resources_owner`].
//! * A page tree node is copied without its `/Kids` array, and the kids are read one by one
//!   from the table, so the walk allocates nothing that scales with the number of kids.
//! * `WithValidationErrorObject` is dropped (see `model/deref.rs`): the error keeps its cause
//!   and gets a context message naming the node.
//! * `/MediaBox` or `/CropBox` that is null or has fewer than four elements is an
//!   [`Error::Panic`]: Go indexes the array and panics. The elements are dereferenced in order
//!   first, so an element that is not a number fails with `wrong type` before that.
//! * `PageNumber` (`processPageTreeForPageNumber`) is not ported; byblos does not reach it.

use super::deref::{NEVER, go_v, type_name};
use super::xref::XRefTable;
use crate::error::{Error, Sentinel};
use crate::object::{Dict, Object, Rect, Ref};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// `InheritedPageAttrs`: the page attributes that a page inherits from its ancestors.
#[derive(Debug, Clone, Default)]
pub struct InheritedPageAttrs {
    /// A copy of the `/Resources` dictionary in effect; `None` is Go's nil.
    pub resources: Option<Dict>,
    pub media_box: Option<Rect>,
    pub crop_box: Option<Rect>,
    pub rotate: i64,
    /// [`XRefTable::page_dict`] only: the page tree node (an ancestor of the page) whose
    /// `/Resources` entry `resources` comes from. `None` when the page has its own `/Resources`
    /// or no resources are inherited. Go's `PageDict` returns that node's stored map
    /// (xreftable.go:2164-2170), so a change to it is shared with the sibling pages; here the
    /// change goes through [`XRefTable::dereference_dict_mut`] on this reference (or on the
    /// reference its `/Resources` entry holds).
    pub resources_owner: Option<Ref>,
}

/// `PageTreeVisit`: the state of one page tree traversal. It rejects cycles and page tree nodes
/// that are reachable from more than one parent.
#[derive(Debug, Default)]
pub struct PageTreeVisit {
    ancestors: BTreeSet<i64>,
    seen: BTreeSet<i64>,
}

impl PageTreeVisit {
    /// `NewPageTreeVisit`.
    pub fn new() -> PageTreeVisit {
        PageTreeVisit::default()
    }

    /// `Enter`: rejects page tree cycles and duplicate page tree nodes. Object number 0 (a
    /// direct object) is not tracked.
    ///
    /// # Errors
    /// [`Sentinel::PageTreeCycle`], [`Sentinel::PageTreeDuplicate`].
    pub fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if self.ancestors.contains(&obj_nr) {
            return Err(Error::Sentinel(Sentinel::PageTreeCycle).context(format!("obj#{obj_nr}")));
        }
        if self.seen.contains(&obj_nr) {
            return Err(
                Error::Sentinel(Sentinel::PageTreeDuplicate).context(format!("obj#{obj_nr}"))
            );
        }
        self.ancestors.insert(obj_nr);
        self.seen.insert(obj_nr);
        Ok(())
    }

    /// `Leave`: leaves the current page tree node.
    pub fn leave(&mut self, obj_nr: i64) {
        if obj_nr != 0 {
            self.ancestors.remove(&obj_nr);
        }
    }
}

/// The state that one `PageDict` walk shares (Go's `c`, `pAttrs`, `p`, `page` and `visit`).
struct Walk<'a> {
    cancel: &'a AtomicBool,
    attrs: InheritedPageAttrs,
    /// Pages passed so far.
    p: i64,
    page: i64,
    visit: PageTreeVisit,
}

/// A page tree node: its dictionary, without the `/Kids` array if it has one, and the length
/// of that array.
pub(super) struct PageNode {
    pub(super) dict: Dict,
    pub(super) kids: Option<usize>,
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

impl XRefTable {
    /// `DereferenceDict(ref)` for a page tree node, as a [`PageNode`]. A null node is `None`
    /// (Go's nil dictionary, which behaves as an empty one).
    pub(super) fn page_node(
        &mut self,
        r: Ref,
        cancel: &AtomicBool,
    ) -> Result<Option<PageNode>, Error> {
        let (o, _) = self.ind_ref_to_object(r, true, cancel)?;
        let node = match o {
            Object::Null => return Ok(None),
            Object::Dict(d) => d,
            other => {
                return Err(Error::Sentinel(Sentinel::ExpectedDict).context(format!(
                    "obj#{} gen#{}: expected types.Dict, got {}",
                    r.num,
                    r.generation,
                    type_name(other)
                )));
            }
        };
        let kids = match node.find(b"Kids") {
            Some(Object::Array(a)) => Some(a),
            _ => None,
        };
        let dict = Dict(
            node.iter()
                .filter(|(_, v)| {
                    !kids.is_some_and(|a| matches!(v, Object::Array(b) if std::ptr::eq(a, b)))
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        Ok(Some(PageNode {
            dict,
            kids: kids.map(Vec::len),
        }))
    }

    /// `rect`: the rectangle of the first four numbers of `a`, each of which may be a reference.
    /// As Go (xreftable.go:2016), the elements are dereferenced in order, so an element that is
    /// not a number fails before a missing one; Go panics only on the first index past the end.
    fn rect(&mut self, a: &[Object]) -> Result<Rect, Error> {
        let mut v = [0.0; 4];
        for (i, n) in v.iter_mut().enumerate() {
            let Some(o) = a.get(i) else {
                return Err(Error::Panic(
                    "page boundary array has fewer than four elements",
                ));
            };
            *n = self.dereference_number(o)?;
        }
        Ok(Rect::new(v[0], v[1], v[2], v[3]))
    }

    /// `checkInheritedPageAttrs`: takes over the boundaries, the rotation and the resources that
    /// `d` defines. With `consolidate` the resources go through
    /// [`XRefTable::consolidate_resources`] (`consolidateRes=true`), else they are taken as they
    /// are.
    pub(super) fn check_inherited_page_attrs(
        &mut self,
        d: &Dict,
        attrs: &mut InheritedPageAttrs,
        consolidate: bool,
    ) -> Result<(), Error> {
        if let Some(obj) = d.find(b"MediaBox") {
            let a = self.dereference_array(obj)?.cloned().unwrap_or_default();
            attrs.media_box = Some(self.rect(&a)?);
        }

        if let Some(obj) = d.find(b"CropBox") {
            let a = self.dereference_array(obj)?.cloned().unwrap_or_default();
            attrs.crop_box = Some(self.rect(&a)?);
        }

        if let Some(obj) = d.find(b"Rotate") {
            match self.dereference(obj)? {
                Object::Integer(i) => attrs.rotate = *i,
                // Relaxed mode accepts a float.
                Object::Float(f) => attrs.rotate = f.round() as i64,
                other => {
                    return Err(Error::Corrupt(format!(
                        "dereferenceNumber: wrong type <{}>",
                        go_v(other)
                    )));
                }
            }
        }

        let Some(obj) = d.find(b"Resources") else {
            return Ok(());
        };
        if consolidate {
            // Accumulate inherited resources.
            return self.consolidate_resources(obj, attrs);
        }
        // Return the resource dict as is.
        attrs.resources = self.dereference_dict(obj)?.cloned();
        Ok(())
    }

    /// `pageObjType` in relaxed mode: the `/Type` of the node, `Pages` for a node without one
    /// that has `/Count` and `/Kids`, otherwise empty.
    pub(super) fn page_obj_type(&mut self, r: Ref, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
        let Some(node) = self.page_node(r, cancel)? else {
            return Ok(Vec::new());
        };
        let ty = node.dict.find(b"Type").cloned();
        if let (Some(t), _) = self.name_from_value(b"Type", ty.as_ref())? {
            return Ok(t);
        }
        let has_kids = node.kids.is_some() || node.dict.find(b"Kids").is_some();
        if node.dict.find(b"Count").is_some() && has_kids {
            return Ok(b"Pages".to_vec());
        }
        Ok(Vec::new())
    }

    /// `processPageTreeKidForPageDict`.
    fn page_tree_kid(
        &mut self,
        w: &mut Walk<'_>,
        r: Ref,
        depth: u64,
    ) -> Result<Option<(Dict, Ref)>, Error> {
        check_cancel(w.cancel)?;
        let ty = self.page_obj_type(r, w.cancel)?;
        match ty.as_slice() {
            b"Pages" => self.page_tree_node(w, r, depth + 1),
            b"Page" => {
                w.p = w.p.saturating_add(1);
                if w.p == w.page {
                    self.page_tree_node(w, r, depth + 1)
                } else {
                    Ok(None)
                }
            }
            // Relaxed mode skips any other node.
            _ => Ok(None),
        }
    }

    /// The loop over the kids of `processPageTreeForPageDictDepth`; `n` is the length of the
    /// `/Kids` array of `root`.
    fn page_tree_kids(
        &mut self,
        w: &mut Walk<'_>,
        root: Ref,
        n: usize,
        depth: u64,
    ) -> Result<Option<(Dict, Ref)>, Error> {
        for i in 0..n {
            check_cancel(w.cancel)?;

            let kid = match self.page_node_kid(root, i, w.cancel)? {
                None | Some(Object::Null) => continue,
                Some(o) => o,
            };

            // Process next page node dict.
            let Object::Ref(r) = kid else {
                return Err(Error::Corrupt(
                    "processPageTreeForPageDictDepth: corrupt page node dict".to_string(),
                ));
            };

            if let Some(found) = self.page_tree_kid(w, r, depth)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The `i`th element of the `/Kids` array of the node `root`.
    pub(super) fn page_node_kid(
        &mut self,
        root: Ref,
        i: usize,
        cancel: &AtomicBool,
    ) -> Result<Option<Object>, Error> {
        let (o, _) = self.ind_ref_to_object(root, true, cancel)?;
        Ok(match o {
            Object::Dict(d) => d.array_entry(b"Kids").and_then(|a| a.get(i)).cloned(),
            _ => None,
        })
    }

    /// `processPageTreeForPageDictDepth`: walks the page tree below `root` down to the leaf
    /// node that represents the page `w.page`. `Ok(None)` is Go's nil page dictionary.
    fn page_tree_node(
        &mut self,
        w: &mut Walk<'_>,
        root: Ref,
        depth: u64,
    ) -> Result<Option<(Dict, Ref)>, Error> {
        check_cancel(w.cancel)?;
        self.check_recursion_depth("page tree", depth)?;

        let Some(node) = self.page_node(root, w.cancel)? else {
            return Ok(None);
        };

        let (count, _) = self
            .dereference_integer_entry(&node.dict, b"Count")
            .map_err(|e| e.context(format!("page tree obj#{} Count", root.num)))?;
        if let Some(count) = count {
            let end = w.p.saturating_add(count);
            if end < w.page {
                // Skip sub pagetree.
                w.p = end;
                return Ok(None);
            }
        }

        // Return the current state of all page attributes that may be inherited.
        self.check_inherited_page_attrs(&node.dict, &mut w.attrs, false)?;
        if node.dict.find(b"Resources").is_some() {
            w.attrs.resources_owner = w.attrs.resources.is_some().then_some(root);
        }

        let Some(n) = node.kids else {
            return Ok(Some((node.dict, root)));
        };
        w.visit.enter(root.num)?;
        let found = self.page_tree_kids(w, root, n, depth);
        w.visit.leave(root.num);
        found
    }

    /// `PageDict`, with `consolidateRes=false`: the dictionary of page `page_nr` (counted from
    /// 1), the reference to it, and the attributes in effect, with
    /// [`InheritedPageAttrs::resources_owner`] naming the ancestor that holds inherited
    /// `/Resources`. The page count has to be known
    /// ([`XRefTable::ensure_page_count`]).
    ///
    /// # Errors
    /// [`Sentinel::PageNotFound`], [`Sentinel::PageTreeCycle`], [`Sentinel::PageTreeDuplicate`],
    /// [`Sentinel::MaxRecursionDepth`], [`Error::Cancelled`], the errors of
    /// [`XRefTable::pages`] and of the dereferencing of the nodes and their attributes.
    pub fn page_dict(
        &mut self,
        page_nr: i64,
        cancel: &AtomicBool,
    ) -> Result<(Dict, Ref, InheritedPageAttrs), Error> {
        check_cancel(cancel)?;
        if page_nr <= 0 || page_nr > self.page_count {
            return Err(Error::Sentinel(Sentinel::PageNotFound));
        }

        // Get an indirect reference to the page tree root dict.
        let root = self.pages()?;

        let mut w = Walk {
            cancel,
            attrs: InheritedPageAttrs::default(),
            p: 0,
            page: page_nr,
            visit: PageTreeVisit::new(),
        };
        let found = self.page_tree_node(&mut w, root, 0)?;
        let (d, r) = found.ok_or(Error::Sentinel(Sentinel::PageNotFound))?;
        if w.attrs.resources_owner == Some(r) {
            w.attrs.resources_owner = None;
        }
        Ok((d, r, w.attrs))
    }

    /// `EnsurePageCount`: sets `page_count` from the `/Count` of the page tree root if it is not
    /// known yet.
    ///
    /// # Errors
    /// The errors of [`XRefTable::pages`], a root that is no dictionary, and `pageDict: missing
    /// "Count"`.
    pub fn ensure_page_count(&mut self) -> Result<(), Error> {
        if self.page_count > 0 {
            return Ok(());
        }

        let root = self.pages()?;
        let missing = || Error::Corrupt("pageDict: missing \"Count\"".to_string());
        let node = self.page_node(root, &NEVER)?.ok_or_else(missing)?;
        let (count, _) = self
            .dereference_integer_entry(&node.dict, b"Count")
            .map_err(|e| e.context("pageDict Count"))?;
        self.page_count = count.ok_or_else(missing)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::xref::Entry;

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

    fn kids(nums: &[i64]) -> Object {
        Object::Array(nums.iter().map(|&n| Object::Ref(r(n))).collect())
    }

    fn numbers(v: &[i64]) -> Object {
        Object::Array(v.iter().map(|&n| Object::Integer(n)).collect())
    }

    fn pages_node(count: i64, k: &[i64]) -> Object {
        obj(&[
            ("Type", name("Pages")),
            ("Count", Object::Integer(count)),
            ("Kids", kids(k)),
        ])
    }

    fn page_node(extra: &[(&str, Object)]) -> Object {
        let mut d = dict(&[("Type", name("Page"))]);
        for (k, v) in extra {
            d.set(k.as_bytes(), v.clone());
        }
        Object::Dict(d)
    }

    /// A table whose catalog points to object 1, with the given objects and page count.
    fn table(objs: Vec<(i64, Object)>, page_count: i64) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.root_dict = Some(dict(&[("Pages", Object::Ref(r(1)))]));
        x.page_count = page_count;
        for (n, o) in objs {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn page_ref(x: &mut XRefTable, n: i64) -> Result<Ref, Error> {
        let cancel = AtomicBool::new(false);
        x.page_dict(n, &cancel).map(|(_, r, _)| r)
    }

    // Port of TestPageRootErrors (the EnsurePageCount part).
    #[test]
    fn ensure_page_count_rejects_malformed_roots() {
        let cases = [
            ("MissingCatalog", None, None, "missing root dict"),
            ("UnresolvedCatalog", Some(r(1)), None, "missing root dict"),
            (
                "MissingPages",
                None,
                Some(Dict::default()),
                "missing pages root",
            ),
            (
                "NullPages",
                None,
                Some(dict(&[("Pages", Object::Null)])),
                "corrupt pages root",
            ),
            (
                "IntegerPages",
                None,
                Some(dict(&[("Pages", Object::Integer(1))])),
                "corrupt pages root",
            ),
            (
                "DirectPages",
                None,
                Some(dict(&[("Pages", obj(&[]))])),
                "corrupt pages root",
            ),
            (
                "ArrayPages",
                None,
                Some(dict(&[("Pages", Object::Array(Vec::new()))])),
                "corrupt pages root",
            ),
        ];
        for (name, root, catalog, want) in cases {
            let mut x = XRefTable::new(Config::default());
            x.root = root;
            x.root_dict = catalog;
            assert_eq!(
                x.ensure_page_count(),
                Err(Error::Corrupt(want.to_string())),
                "{name}"
            );
        }
    }

    // Port of TestPageRootValidReference (without PageNumber).
    #[test]
    fn ensure_page_count_reads_the_root_count() {
        let mut x = table(
            vec![
                (1, pages_node(1, &[2])),
                (2, page_node(&[("Parent", Object::Ref(r(1)))])),
            ],
            0,
        );
        assert_eq!(x.pages(), Ok(r(1)));
        assert_eq!(x.ensure_page_count(), Ok(()));
        assert_eq!(x.page_count, 1);
        // Known already: the root is not looked at again.
        x.table.remove(&1);
        assert_eq!(x.ensure_page_count(), Ok(()));
    }

    #[test]
    fn ensure_page_count_needs_a_count() {
        let missing = Err(Error::Corrupt("pageDict: missing \"Count\"".to_string()));
        let mut x = table(vec![(1, obj(&[("Type", name("Pages"))]))], 0);
        assert_eq!(x.ensure_page_count(), missing);
        let mut x = table(vec![], 0);
        assert_eq!(x.ensure_page_count(), missing);
        let mut x = table(vec![(1, obj(&[("Count", name("x"))]))], 0);
        let err = x.ensure_page_count().unwrap_err();
        assert!(err.to_string().starts_with("pageDict Count: "), "{err}");
        let mut x = table(vec![(1, Object::Integer(7))], 0);
        assert!(
            x.ensure_page_count()
                .unwrap_err()
                .is(Sentinel::ExpectedDict)
        );
    }

    // Port of TestPageDictRejectsUnresolvedPage.
    #[test]
    fn page_dict_rejects_an_unresolved_page() {
        let mut x = table(vec![(1, pages_node(1, &[]))], 1);
        let cancel = AtomicBool::new(false);
        let err = x.page_dict(1, &cancel).unwrap_err();
        assert!(err.is(Sentinel::PageNotFound), "{err}");
    }

    #[test]
    fn page_dict_rejects_numbers_out_of_range() {
        let mut x = table(vec![(1, pages_node(1, &[2])), (2, page_node(&[]))], 1);
        assert_eq!(page_ref(&mut x, 1), Ok(r(2)));
        for n in [i64::MIN, -1, 0, 2, i64::MAX] {
            assert_eq!(
                page_ref(&mut x, n),
                Err(Error::Sentinel(Sentinel::PageNotFound)),
                "{n}"
            );
        }
    }

    // Port of TestPageTreeLookupRejectsRecursionDepth (the page dict part).
    #[test]
    fn page_tree_walk_rejects_recursion_depth() {
        let mut x = table(vec![], 1);
        let cancel = AtomicBool::new(false);
        let mut w = Walk {
            cancel: &cancel,
            attrs: InheritedPageAttrs::default(),
            p: 0,
            page: 1,
            visit: PageTreeVisit::new(),
        };
        let max = x.max_recursion_depth();
        let err = x.page_tree_node(&mut w, r(1), max + 1).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");
        // At the limit the walk goes on: the node is unresolved, so there is no page.
        assert!(matches!(x.page_tree_node(&mut w, r(1), max), Ok(None)));
    }

    /// A huge `max_recursion_depth` does not lift the hard ceiling: a deep /Pages chain fails
    /// with the recursion-depth error instead of overflowing the native stack.
    #[test]
    fn page_tree_walk_is_capped_by_the_ceiling() {
        let n = 100_000;
        let mut objs: Vec<(i64, Object)> = (1..n).map(|i| (i, pages_node(1, &[i + 1]))).collect();
        objs.push((n, page_node(&[])));
        let mut x = table(objs, 1);
        x.conf.limits.max_recursion_depth = u64::MAX;
        let err = page_ref(&mut x, 1).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");
    }

    #[test]
    fn page_dict_walks_to_the_page_and_collects_inherited_attributes() {
        let mut x = table(
            vec![
                (1, pages_node(3, &[2, 3])),
                (
                    2,
                    obj(&[
                        ("Type", name("Pages")),
                        ("Count", Object::Integer(2)),
                        ("Kids", kids(&[4, 5])),
                        ("MediaBox", numbers(&[0, 0, 100, 200])),
                        ("Rotate", Object::Integer(90)),
                    ]),
                ),
                (
                    3,
                    page_node(&[
                        ("MediaBox", numbers(&[0, 0, 10, 10])),
                        ("Resources", obj(&[("Font", obj(&[]))])),
                    ]),
                ),
                (4, page_node(&[("Marker", Object::Integer(4))])),
                (5, page_node(&[("Marker", Object::Integer(5))])),
            ],
            3,
        );
        let cancel = AtomicBool::new(false);

        let (d, ir, attrs) = x.page_dict(2, &cancel).unwrap();
        assert_eq!(ir, r(5));
        assert_eq!(d.int_entry(b"Marker"), Some(5));
        assert_eq!(attrs.media_box, Some(Rect::new(0.0, 0.0, 100.0, 200.0)));
        assert_eq!(attrs.crop_box, None);
        assert_eq!(attrs.rotate, 90);
        assert!(attrs.resources.is_none());

        // The subtree of object 2 is skipped by its count, so its attributes are not taken.
        let (d, ir, attrs) = x.page_dict(3, &cancel).unwrap();
        assert_eq!(ir, r(3));
        assert_eq!(d.name_entry(b"Type"), Some(b"Page".as_slice()));
        assert_eq!(attrs.media_box, Some(Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert_eq!(attrs.rotate, 0);
        assert!(attrs.resources.is_some_and(|d| d.has_entry(b"Font")));

        // The returned dictionary is a copy, and the table keeps the kids.
        assert!(matches!(
            x.table.get(&1).and_then(|e| e.object.as_ref()),
            Some(crate::model::xref::EntryObject::Obj(Object::Dict(d))) if d.array_entry(b"Kids").is_some_and(|k| k.len() == 2)
        ));
    }

    #[test]
    fn page_dict_follows_references_in_attributes() {
        let mut x = table(
            vec![
                (1, pages_node(1, &[2])),
                (
                    2,
                    page_node(&[
                        (
                            "MediaBox",
                            Object::Array(vec![
                                Object::Integer(0),
                                Object::Ref(r(3)),
                                Object::Float(5.5),
                                Object::Integer(7),
                            ]),
                        ),
                        ("CropBox", Object::Ref(r(4))),
                        ("Rotate", Object::Ref(r(5))),
                        ("Resources", Object::Ref(r(6))),
                    ]),
                ),
                (3, Object::Integer(2)),
                (4, numbers(&[1, 2, 3, 4])),
                (5, Object::Float(89.5)),
                (6, obj(&[("XObject", obj(&[]))])),
            ],
            1,
        );
        let cancel = AtomicBool::new(false);
        let (_, _, attrs) = x.page_dict(1, &cancel).unwrap();
        assert_eq!(attrs.media_box, Some(Rect::new(0.0, 2.0, 5.5, 7.0)));
        assert_eq!(attrs.crop_box, Some(Rect::new(1.0, 2.0, 3.0, 4.0)));
        // Relaxed mode rounds a float rotation, half away from zero.
        assert_eq!(attrs.rotate, 90);
        assert!(attrs.resources.is_some_and(|d| d.has_entry(b"XObject")));
    }

    #[test]
    fn page_dict_rejects_malformed_attributes() {
        let cancel = AtomicBool::new(false);
        for (key, value) in [
            ("MediaBox", numbers(&[0, 0, 1])),
            ("MediaBox", Object::Null),
            ("CropBox", Object::Array(Vec::new())),
        ] {
            let mut x = table(
                vec![(1, pages_node(1, &[2])), (2, page_node(&[(key, value)]))],
                1,
            );
            let err = x.page_dict(1, &cancel).unwrap_err();
            assert!(matches!(err.root(), Error::Panic(_)), "{key}: {err}");
        }
        let mut x = table(
            vec![
                (1, pages_node(1, &[2])),
                (2, page_node(&[("Rotate", name("x"))])),
            ],
            1,
        );
        let err = x.page_dict(1, &cancel).unwrap_err();
        assert!(
            err.to_string().contains("dereferenceNumber: wrong type"),
            "{err}"
        );
        let mut x = table(
            vec![
                (1, pages_node(1, &[2])),
                (2, page_node(&[("MediaBox", numbers(&[0, 0, 1, 1]))])),
            ],
            1,
        );
        assert_eq!(page_ref(&mut x, 1), Ok(r(2)));
    }

    #[test]
    fn page_dict_repairs_an_untyped_pages_node_and_skips_unknown_types() {
        let mut x = table(
            vec![
                (1, pages_node(2, &[2, 3, 5, 6])),
                // No /Type, but /Count and /Kids: a Pages node in relaxed mode.
                (
                    2,
                    obj(&[("Count", Object::Integer(1)), ("Kids", kids(&[4]))]),
                ),
                // No /Type and no /Kids: skipped.
                (3, obj(&[("Count", Object::Integer(1))])),
                (4, page_node(&[])),
                // Another type: skipped.
                (5, obj(&[("Type", name("Template"))])),
                // A null node: skipped.
                (6, Object::Null),
            ],
            2,
        );
        assert_eq!(page_ref(&mut x, 1), Ok(r(4)));
        assert_eq!(
            page_ref(&mut x, 2),
            Err(Error::Sentinel(Sentinel::PageNotFound))
        );
    }

    #[test]
    fn page_dict_rejects_a_kid_that_is_no_reference() {
        let mut x = table(
            vec![(
                1,
                obj(&[
                    ("Type", name("Pages")),
                    ("Count", Object::Integer(1)),
                    (
                        "Kids",
                        Object::Array(vec![Object::Null, Object::Integer(3)]),
                    ),
                ]),
            )],
            1,
        );
        let err = page_ref(&mut x, 1).unwrap_err();
        assert_eq!(
            err,
            Error::Corrupt("processPageTreeForPageDictDepth: corrupt page node dict".to_string())
        );
    }

    #[test]
    fn page_dict_rejects_cycles_and_duplicates() {
        // 1 -> 2 -> 1.
        let mut x = table(
            vec![
                (1, pages_node(5, &[2])),
                (2, obj(&[("Type", name("Pages")), ("Kids", kids(&[1]))])),
            ],
            5,
        );
        let err = page_ref(&mut x, 5).unwrap_err();
        assert!(err.is(Sentinel::PageTreeCycle), "{err}");
        assert_eq!(err.to_string(), "obj#1: circular page tree");

        // 1 -> 2 and 1 -> 2.
        let mut x = table(
            vec![
                (1, pages_node(5, &[2, 2])),
                (2, obj(&[("Type", name("Pages")), ("Kids", kids(&[]))])),
            ],
            5,
        );
        let err = page_ref(&mut x, 5).unwrap_err();
        assert!(err.is(Sentinel::PageTreeDuplicate), "{err}");
    }

    #[test]
    fn page_dict_honours_cancellation() {
        let mut x = table(vec![(1, pages_node(1, &[2])), (2, page_node(&[]))], 1);
        let cancelled = AtomicBool::new(true);
        assert_eq!(x.page_dict(1, &cancelled).unwrap_err(), Error::Cancelled);
    }

    #[test]
    fn page_dict_keeps_attributes_of_nodes_walked_before_the_page() {
        // Go shares one attribute record over the whole walk: what a node that was walked into
        // sets stays in effect for the nodes after it.
        let mut x = table(
            vec![
                (1, pages_node(2, &[2, 3])),
                (
                    2,
                    obj(&[
                        ("Type", name("Pages")),
                        ("Kids", kids(&[4])),
                        ("Rotate", Object::Integer(270)),
                    ]),
                ),
                (3, page_node(&[])),
                (4, obj(&[("Type", name("Other"))])),
            ],
            2,
        );
        let cancel = AtomicBool::new(false);
        let (_, ir, attrs) = x.page_dict(1, &cancel).unwrap();
        assert_eq!(ir, r(3));
        assert_eq!(attrs.rotate, 270);
    }

    #[test]
    fn page_dict_names_the_ancestor_that_owns_inherited_resources() {
        let mut x = table(
            vec![
                (1, pages_node(4, &[2, 3])),
                (
                    2,
                    obj(&[
                        ("Type", name("Pages")),
                        ("Count", Object::Integer(2)),
                        ("Kids", kids(&[4, 5])),
                        ("Resources", Object::Ref(r(6))),
                    ]),
                ),
                (
                    3,
                    obj(&[
                        ("Type", name("Pages")),
                        ("Count", Object::Integer(2)),
                        ("Kids", kids(&[7, 8])),
                    ]),
                ),
                (4, page_node(&[])),
                (
                    5,
                    page_node(&[("Resources", obj(&[("XObject", obj(&[]))]))]),
                ),
                (6, obj(&[("Font", obj(&[]))])),
                (7, page_node(&[])),
                (8, page_node(&[("Resources", Object::Null)])),
            ],
            4,
        );
        let cancel = AtomicBool::new(false);

        // Inherited from object 2 through a reference.
        let (_, ir, attrs) = x.page_dict(1, &cancel).unwrap();
        assert_eq!(ir, r(4));
        assert_eq!(attrs.resources_owner, Some(r(2)));
        assert!(attrs.resources.is_some_and(|d| d.has_entry(b"Font")));

        // The page's own resources.
        let (_, _, attrs) = x.page_dict(2, &cancel).unwrap();
        assert_eq!(attrs.resources_owner, None);
        assert!(attrs.resources.is_some_and(|d| d.has_entry(b"XObject")));

        // Nothing inherited, and a null /Resources is Go's nil.
        for n in [3, 4] {
            let (_, _, attrs) = x.page_dict(n, &cancel).unwrap();
            assert_eq!(attrs.resources_owner, None);
            assert!(attrs.resources.is_none());
        }
    }

    #[test]
    fn page_tree_visit_tracks_ancestors_and_seen() {
        let mut v = PageTreeVisit::new();
        assert_eq!(v.enter(0), Ok(()));
        assert_eq!(v.enter(0), Ok(()));
        assert_eq!(v.enter(1), Ok(()));
        assert!(v.enter(1).unwrap_err().is(Sentinel::PageTreeCycle));
        assert_eq!(v.enter(2), Ok(()));
        v.leave(2);
        assert!(v.enter(2).unwrap_err().is(Sentinel::PageTreeDuplicate));
        v.leave(1);
        v.leave(0);
        assert!(v.enter(1).unwrap_err().is(Sentinel::PageTreeDuplicate));
    }

    #[test]
    fn rect_dereferences_in_order_before_the_missing_index() {
        let mut x = table(vec![], 0);
        // `[0 0!612 792]` parses as `[0 null 792]`: Go fails on the null before a[3].
        let a = [Object::Integer(0), Object::Null, Object::Integer(792)];
        let err = x.rect(&a).unwrap_err();
        assert!(!matches!(err, Error::Panic(_)), "{err:?}");
        assert!(err.to_string().contains("wrong type <<nil>>"), "{err}");
        let a = [Object::Integer(0), Object::Integer(0), Object::Integer(612)];
        assert!(matches!(x.rect(&a), Err(Error::Panic(_))));
        let a = [0, 0, 612, 792].map(Object::Integer);
        assert!(x.rect(&a).is_ok());
    }
}
