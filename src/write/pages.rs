//! Page tree writing (Go: pkg/pdfcpu/writePages.go; `writePages` of write.go:312).
//!
//! Differences from Go:
//!
//! * The selected-pages mode of `writeSelectedPage` (split, trim, remove pages) is not ported:
//!   every page is written, and `writePagesDictDepth` never skips a node, so it returns only the
//!   number of pages written.
//! * Go swaps `/Kids` and `/Count` of the table's dictionary for the new ones while it writes a
//!   node, and puts the old ones back. Here the node's dictionary is copied, without its `/Kids`
//!   array, and the copy gets the new entries.
//! * The kids of a node are read one at a time from the table, and the array of written kids is
//!   charged to the ledger, so that nothing uncharged scales with the number of kids.

use super::objects::{stop_object_stream, write_dict_object, write_entry};
use super::{WriteContext, check, nil};
use crate::error::Error;
use crate::ledger::Held;
use crate::model::{Context, PageTreeVisit};
use crate::object::{Dict, Object, Ref, dict_copy_bytes, entry_copy_bytes};
use std::sync::atomic::AtomicBool;

/// The entries of a page dictionary that `writePageDict` follows, in Go's order.
const PAGE_ENTRIES: &[&[u8]] = &[
    b"Contents",
    b"Resources",
    b"MediaBox",
    b"CropBox",
    b"BleedBox",
    b"TrimBox",
    b"ArtBox",
    b"BoxColorInfo",
    b"PieceInfo",
    b"LastModified",
    b"Rotate",
    b"Group",
    b"Annots",
    b"Thumb",
    b"B",
    b"Dur",
    b"Trans",
    b"AA",
    b"Metadata",
    b"StructParents",
    b"ID",
    b"PZ",
    b"SeparationInfo",
    b"Tabs",
    b"TemplateInstantiated",
    b"PresSteps",
    b"UserUnit",
    b"VP",
];

/// The entries of a page tree node that `writePageEntries` follows (the inheritable ones).
const NODE_ENTRIES: &[&[u8]] = &[b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// `writePageDict`: the page dictionary `page` of the object `ir`, and what its entries refer to.
fn write_page_dict(
    ctx: &mut Context,
    w: &mut WriteContext,
    ir: Ref,
    mut page: Dict,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    if w.has_write_offset(ir.num) {
        return Ok(());
    }

    write_dict_object(ctx, w, ir.num, ir.generation, &mut page, cancel)?;

    if page.indirect_ref_entry(b"Parent").is_none() {
        return Err(Error::Corrupt("missing parent".into()));
    }

    ctx.writing_pages = true;
    for name in PAGE_ENTRIES {
        check(cancel)?;
        write_entry(ctx, w, &page, name, cancel)?;
    }
    ctx.writing_pages = false;
    Ok(())
}

/// `pageNodeDict`: the reference and the `/Type` of the page tree node `o`. A null node is
/// `None`.
fn page_node(ctx: &mut Context, o: &Object) -> Result<Option<(Ref, Vec<u8>)>, Error> {
    let ir = match o {
        Object::Null => return Ok(None),
        // Dereference the next page node dict.
        Object::Ref(ir) => *ir,
        _ => return Err(Error::Corrupt("missing indirect reference".into())),
    };
    let type_value = match ctx.xref.dereference_dict_ref(ir) {
        Err(_) => return Err(Error::Corrupt("cannot dereference page node dict".into())),
        Ok(None) => return Err(Error::Corrupt("page node dict is null".into())),
        Ok(Some(d)) => d.find(b"Type").cloned(),
    };
    let (dict_type, _) = ctx
        .xref
        .name_from_value(b"Type", type_value.as_ref())
        .map_err(|e| e.context("page node dict Type"))?;
    match dict_type {
        Some(t) => Ok(Some((ir, t))),
        None => Err(Error::Corrupt("missing page node dict type".into())),
    }
}

/// The number of kids of the page tree node `parent`.
fn kids_len(ctx: &mut Context, parent: Ref) -> Result<usize, Error> {
    Ok(ctx
        .xref
        .dereference_dict_ref(parent)?
        .and_then(|d| d.array_entry(b"Kids"))
        .map_or(0, |a| a.len()))
}

/// Kid `index` of the page tree node `parent`; null if there is none.
fn kid_at(ctx: &mut Context, parent: Ref, index: usize) -> Result<Object, Error> {
    Ok(ctx
        .xref
        .dereference_dict_ref(parent)?
        .and_then(|d| d.array_entry(b"Kids"))
        .and_then(|a| a.get(index))
        .cloned()
        .unwrap_or(Object::Null))
}

/// `writeKids`: writes the kids of `parent`, and returns the kids that were written, the number
/// of pages under them and the charge of the array.
fn write_kids(
    ctx: &mut Context,
    w: &mut WriteContext,
    parent: Ref,
    depth: u64,
    visit: &mut PageTreeVisit,
    cancel: &AtomicBool,
) -> Result<(Vec<Object>, i64, Held), Error> {
    let mut held = ctx.xref.mem().charge(0)?;
    let mut kids: Vec<Object> = Vec::new();
    let mut count: i64 = 0;

    for index in 0..kids_len(ctx, parent)? {
        check(cancel)?;

        let o = kid_at(ctx, parent, index)?;
        let Some((ir, dict_type)) = page_node(ctx, &o)? else {
            continue;
        };

        match dict_type.as_slice() {
            // Recurse over the page tree.
            b"Pages" => {
                let written = write_pages_dict_depth(ctx, w, ir, depth + 1, visit, cancel)?;
                count = count.saturating_add(written);
            }
            b"Page" => {
                let mem = ctx.xref.mem().clone();
                let (page, _copy) = match ctx.xref.dereference_dict_ref(ir)? {
                    Some(p) => {
                        let copy = mem.charge(dict_copy_bytes(p))?;
                        (p.clone(), copy)
                    }
                    None => return Err(nil("nil page dict")),
                };
                write_page_dict(ctx, w, ir, page, cancel)?;
                count = count.saturating_add(1);
            }
            other => {
                return Err(Error::Corrupt(format!(
                    "unexpected dict type: {}",
                    String::from_utf8_lossy(other)
                )));
            }
        }

        held.grow_by(std::mem::size_of::<Object>() as u64)?;
        kids.try_reserve(1)?;
        kids.push(o);
    }

    Ok((kids, count, held))
}

/// `writePagesDictDepth`: the page tree node `ind_ref`, its kids and, then, itself with the
/// `/Kids` and `/Count` of what was written. Returns the number of pages written.
fn write_pages_dict_depth(
    ctx: &mut Context,
    w: &mut WriteContext,
    ind_ref: Ref,
    depth: u64,
    visit: &mut PageTreeVisit,
    cancel: &AtomicBool,
) -> Result<i64, Error> {
    check(cancel)?;
    ctx.xref.check_recursion_depth("page tree", depth)?;
    visit.enter(ind_ref.num)?;
    let written = write_pages_node(ctx, w, ind_ref, depth, visit, cancel);
    visit.leave(ind_ref.num);
    written
}

/// What `writePagesDictDepth` does between `Enter` and `Leave`.
fn write_pages_node(
    ctx: &mut Context,
    w: &mut WriteContext,
    ind_ref: Ref,
    depth: u64,
    visit: &mut PageTreeVisit,
    cancel: &AtomicBool,
) -> Result<i64, Error> {
    // The dictionary without its kids, which are read one by one, and the charge of the copy.
    let mem = ctx.xref.mem().clone();
    let (mut d, _copy) = match ctx.xref.dereference_dict_ref(ind_ref).map_err(|e| {
        e.context(format!(
            "writePagesDict: unable to dereference indirect object #{}",
            ind_ref.num
        ))
    })? {
        Some(node) => {
            let entries = || node.iter().filter(|(k, _)| k.as_slice() != b"Kids");
            let copy = mem.charge(entries().map(|(k, v)| entry_copy_bytes(k, v)).sum())?;
            let mut d = Dict::default();
            for (k, v) in entries() {
                d.0.insert(k.clone(), v.clone());
            }
            (d, copy)
        }
        // Go assigns to a nil map.
        None => return Err(nil("assignment to entry in nil map")),
    };

    // Iterate over the page tree.
    let (kids, count, _held) = write_kids(ctx, w, ind_ref, depth, visit, cancel)?;
    check(cancel)?;

    d.update(b"Kids", Object::Array(kids));
    d.update(b"Count", Object::Integer(count));

    write_dict_object(ctx, w, ind_ref.num, ind_ref.generation, &mut d, cancel)?;

    // TODO in Go: check the inheritance rules.
    for name in NODE_ENTRIES {
        check(cancel)?;
        write_entry(ctx, w, &d, name, cancel)?;
    }
    Ok(count)
}

/// `writePages`: the page tree of the catalog `root`, into object streams.
///
/// # Errors
/// [`Error::Corrupt`] if the catalog has no `/Pages` reference; the errors of the page tree walk
/// ([`crate::Sentinel::PageTreeCycle`], [`crate::Sentinel::PageTreeDuplicate`], the recursion
/// depth, a node that is not a dictionary, a page without a parent) and of the object writers.
pub(super) fn write_pages(
    ctx: &mut Context,
    w: &mut WriteContext,
    root: &Dict,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    // The page tree root (the top `Pages` dict) must be an indirect reference.
    let Some(ind_ref) = root.indirect_ref_entry(b"Pages") else {
        return Err(Error::Corrupt("missing indirect obj for pages dict".into()));
    };

    // Embed all page tree objects into object streams.
    w.write_to_object_stream = true;

    // Write the page tree.
    write_pages_dict_depth(ctx, w, ind_ref, 0, &mut PageTreeVisit::new(), cancel)?;

    stop_object_stream(ctx, w, cancel)
}
