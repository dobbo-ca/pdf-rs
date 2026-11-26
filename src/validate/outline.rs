//! The document outline (Go: validate/outlineTree.go).
//!
//! The outline walk repairs the bookmark tree in place: Go changes the stored dictionaries
//! through the maps it dereferenced. Here the walk reads copies and writes each change to the
//! stored dictionary (`stored_dict_mut`), and it reads the stored dictionary again where Go
//! reads the changed map (the `Next` of an item).
//!
//! Differences from Go:
//!
//! * Validation is relaxed, always. The `fixed` flag, which only feeds the "repaired" notice, is
//!   not kept, and `ErrBookmarksRepair` ([`Sentinel::BookmarksRepair`]) removes the outline.
//! * `handleDuplicate` compares `ir == last` as pointers, which is never true for the pointers
//!   the walk passes, so that branch is not ported. `ir == first` is the first item of the list.
//! * `validateActionOrDestination` (annotation.go:268) is `validate_action_or_destination`
//!   here, without the merge branch (`IsMerging`): WP 16 owns the annotation copy.
//! * `DereferenceDestArray` resolves a name through the `Dests` name tree by a search of the
//!   internalised tree for the key (Go's `Node.Value` tries the keys as PDF bytes first). Its
//!   error is ignored in relaxed mode, so only the dereferences it makes are kept.
//! * `xRefTable.Outlines` holds a copy of the outline dictionary.

use super::Validator;
use super::action::validate_action_dict_object;
use super::destination::validate_destination;
use super::media::wrap_recursion_error;
use super::object::{
    OPTIONAL, REQUIRED, validate_dict_entry, validate_entry, validate_ind_ref_entry,
    validate_integer_entry, validate_name_entry, validate_number_array_entry,
    validate_string_entry,
};
use crate::error::{Error, Sentinel};
use crate::model::{Entry, EntryObject, Version, XRefTable};
use crate::object::{Dict, Object, Ref};
use std::collections::BTreeSet;

/// The stored dictionary of the object `num`, if it is a loaded dictionary.
fn stored_dict(x: &XRefTable, num: i64) -> Option<&Dict> {
    match x.table.get(&num) {
        Some(Entry {
            object: Some(EntryObject::Obj(Object::Dict(d))),
            ..
        }) => Some(d),
        _ => None,
    }
}

/// [`stored_dict`] to change the dictionary.
fn stored_dict_mut(x: &mut XRefTable, num: i64) -> Option<&mut Dict> {
    match x.table.get_mut(&num) {
        Some(Entry {
            object: Some(EntryObject::Obj(Object::Dict(d))),
            ..
        }) => Some(d),
        _ => None,
    }
}

/// `checkValidationTree` (numberTree.go:337): the cancel check and the recursion limit.
fn check_validation_tree(v: &Validator<'_>, name: &str, depth: u64) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth(name, depth)
}

/// `validateOutlineItemDictTitle`: relaxed validation also takes a name.
fn validate_outline_item_dict_title(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if validate_string_entry(x, d, dict_name, "Title", REQUIRED, Version::V10, None).is_err() {
        validate_name_entry(x, d, dict_name, "Title", REQUIRED, Version::V10, None)?;
    }
    Ok(())
}

/// `validateOutlineItemDictParent`: `Parent` is optional in relaxed validation.
fn validate_outline_item_dict_parent(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if let Some(ir) = validate_ind_ref_entry(x, d, dict_name, "Parent", OPTIONAL, Version::V10)? {
        x.dereference_dict_ref(ir)?;
    }
    Ok(())
}

/// `validateActionOrDestination` (annotation.go:268): the action `A` is validated, else the
/// destination `Dest`; the result is the name of a named destination (empty for an action, an
/// array or none).
fn validate_action_or_destination(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<String, Error> {
    // The action that shall be performed when this item is activated.
    if let Some(d1) = validate_dict_entry(v.x, d, dict_name, "A", OPTIONAL, since, None)? {
        validate_action_dict_object(v, &d1, d.0.get(b"A".as_slice()), &format!("{dict_name}.A"))?;
        return Ok(String::new());
    }

    // A destination that shall be displayed when this item is activated. Relaxed validation
    // takes it since 1.0.
    let raw = d.0.get(b"Dest".as_slice()).cloned();
    let obj = validate_entry(v.x, d, dict_name, "Dest", OPTIONAL, Version::V10)?;
    let (Some(raw), Some(_)) = (raw, obj) else {
        return Ok(String::new());
    };
    validate_destination(v.x, &raw, false)
}

/// `dereferenceDestArray`: the destination array `o` is, refers to, or has as `D`.
fn dereference_dest_array(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    match x.dereference(o)?.clone() {
        Object::Null | Object::Array(_) => Ok(()),
        Object::Dict(d) => match x.dereference_dict_entry(&d, b"D")? {
            Object::Array(_) => Ok(()),
            _ => Err(Error::Validation("invalid dest array".to_string())),
        },
        _ => Err(Error::Validation("invalid dest array".to_string())),
    }
}

/// `DereferenceDestArray`: the destination array of the named destination `key`.
fn dereference_named_dest_array(v: &mut Validator<'_>, key: &str) -> Result<(), Error> {
    v.check()?;
    let from_tree = v.x.names.get("Dests").and_then(|root| {
        // The first entry with the key, depth first.
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if let Some(e) = n.names.iter().find(|e| e.k == key) {
                return Some(e.v.clone());
            }
            stack.extend(n.kids.iter().rev());
        }
        None
    });
    if let Some(o) = from_tree {
        return dereference_dest_array(v.x, &o);
    }
    let from_dict =
        v.x.vstate
            .dests
            .as_ref()
            .and_then(|d| d.0.get(key.as_bytes()))
            .cloned();
    if let Some(o) = from_dict {
        return dereference_dest_array(v.x, &o);
    }
    Err(Error::Validation(format!(
        "invalid named destination for: {key}"
    )))
}

/// `validateOutlineItemDict`.
fn validate_outline_item_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "outlineItemDict";

    // Title, required, text string
    validate_outline_item_dict_title(v.x, d, dict_name)?;

    // Parent, required, dict indRef
    validate_outline_item_dict_parent(v.x, d, dict_name)?;

    // SE, optional, dict indRef, since V1.3
    if let Some(ir) = validate_ind_ref_entry(v.x, d, dict_name, "SE", OPTIONAL, Version::V13)? {
        v.x.dereference_dict_ref(ir)?;
    }

    // C, optional, array of 3 numbers, since V1.4 (relaxed: 1.3)
    validate_number_array_entry(
        v.x,
        d,
        dict_name,
        "C",
        OPTIONAL,
        Version::V13,
        Some(&|a: &[Object]| a.len() == 3),
    )?;

    // F, optional integer, since V1.4 (relaxed: 1.3)
    validate_integer_entry(v.x, d, dict_name, "F", OPTIONAL, Version::V13, None)?;

    // Optional A or Dest, since V1.1
    let dest_name = validate_action_or_destination(v, d, dict_name, Version::V11)?;

    // An unresolvable destination is a spec violation in relaxed validation.
    if !dest_name.is_empty() {
        let _ = dereference_named_dest_array(v, &dest_name);
    }

    Ok(())
}

/// `handleOutlineItemDict`: validates the outline item `ir` and returns a copy of its dictionary.
fn handle_outline_item_dict(v: &mut Validator<'_>, ir: Ref) -> Result<Dict, Error> {
    let obj_nr = ir.num;
    let d =
        v.x.dereference_dict_ref(ir)
            .map_err(|e| e.context(format!("outline item obj#{obj_nr}: dereference")))?
            .cloned();
    let Some(d) = d else {
        return Err(Error::Validation(format!(
            "outline item obj#{obj_nr}: missing dict"
        )));
    };

    validate_outline_item_dict(v, &d)
        .map_err(|e| e.context(format!("outline item obj#{obj_nr}")))?;

    Ok(d)
}

/// `leaf` (relaxed): no children, or a node that points to itself as its only child.
fn leaf(first_child: Option<Ref>, last_child: Option<Ref>, obj_nr: i64) -> bool {
    match (first_child, last_child) {
        (None, None) => true,
        (Some(f), Some(l)) => f.num == obj_nr && l.num == obj_nr,
        _ => false,
    }
}

/// `evalOutlineCount` (relaxed): repairs `Count` of the item `num` to the number of its
/// descendants, and counts them if the item is open.
fn eval_outline_count(
    x: &mut XRefTable,
    num: i64,
    c: i64,
    visc: i64,
    count: i64,
    total: &mut i64,
    visible: &mut i64,
) {
    let expected = c + visc;
    let mut count = count;
    if count == 0 {
        count = expected;
        set_count(x, num, count);
    }

    if count != expected && count != -expected {
        count = expected;
        set_count(x, num, count);
    }

    if count > 0 {
        *total += c;
        *visible += visc;
    }
}

/// `d["Count"] = count` for the stored dictionary of the object `num`.
fn set_count(x: &mut XRefTable, num: i64, count: i64) {
    if let Some(d) = stored_dict_mut(x, num) {
        d.set(b"Count", Object::Integer(count));
    }
}

/// `dereferenceOutlineCount`: `Count`, `None` if there is none.
fn dereference_outline_count(x: &mut XRefTable, d: &Dict) -> Result<Option<i64>, Error> {
    Ok(x.dereference_integer_entry(d, b"Count")?.0)
}

/// `validateOutlineLeafCount` (relaxed): a leaf has no `Count`.
fn validate_outline_leaf_count(x: &mut XRefTable, num: i64, count: i64) {
    if count == 0 {
        return;
    }
    if let Some(d) = stored_dict_mut(x, num) {
        d.0.remove(b"Count".as_slice());
    }
}

/// `validateOutlineTree`/`validateOutlineTreeDepth`: validates the list of outline items that
/// starts with `first` and returns the number of items and the number of visible descendants.
/// The last item of the list is not checked in relaxed validation. `seen` holds the items that
/// were scanned.
fn validate_outline_tree_depth(
    v: &mut Validator<'_>,
    first: Option<Ref>,
    seen: &mut BTreeSet<i64>,
    depth: u64,
) -> Result<(i64, i64), Error> {
    check_validation_tree(v, "outline tree", depth)?;

    let mut total = 0;
    let mut visible = 0;

    // Process linked list of outline items.
    let mut ir = first;
    while let Some(cur) = ir {
        v.check()?;
        let obj_nr = cur.num;
        total += 1;

        let d = handle_outline_item_dict(v, cur)?;

        let count = dereference_outline_count(v.x, &d)
            .map_err(|e| e.context(format!("outline item obj#{obj_nr}")))?
            .unwrap_or(0);

        let mut first_child = d.indirect_ref_entry(b"First");
        let last_child = d.indirect_ref_entry(b"Last");

        if leaf(first_child, last_child, obj_nr) {
            validate_outline_leaf_count(v.x, obj_nr, count);
        } else {
            scan_and_fix_outline_items(v, &mut first_child, last_child, seen)
                .map_err(|e| e.context(format!("outline item obj#{obj_nr}: scan children")))?;

            let (child_count, visc) = validate_outline_tree_depth(v, first_child, seen, depth + 1)
                .map_err(|e| {
                    wrap_recursion_error(format!("outline item obj#{obj_nr}: validate children"), e)
                })?;

            eval_outline_count(
                v.x,
                obj_nr,
                child_count,
                visc,
                count,
                &mut total,
                &mut visible,
            );
        }

        // Go reads the Next entry of the (possibly changed) stored dictionary.
        ir = stored_dict(v.x, obj_nr).and_then(|d| d.indirect_ref_entry(b"Next"));
    }

    Ok((total, visible))
}

/// `validateOutlineCount` (relaxed): the `Count` of the outline root.
fn validate_outline_count(total: i64, visible: i64, count: Option<i64>) -> Result<(), Error> {
    if visible <= 0 {
        return Ok(());
    }
    let Some(count) = count else {
        return Err(Error::Validation(format!(
            "missing Count, expected {}",
            total + visible
        )));
    };
    if count != total + visible && count != -total - visible {
        return Err(Error::Validation(format!(
            "Count={count}, expected {}",
            total + visible
        )));
    }
    Ok(())
}

/// `firstOfRemainder`: starting with the last item, goes back along `Prev` until it hits the
/// item that precedes `dupl_obj_nr` or `one_before_dupl_obj`. The item it finds is returned; if
/// the duplicate is its `Prev`, that is repaired to `one_before_dupl_obj`. `None` is Go's 0.
fn first_of_remainder(
    v: &mut Validator<'_>,
    last: Option<Ref>,
    dupl_obj_nr: i64,
    one_before_dupl_obj: i64,
) -> Result<Option<Ref>, Error> {
    let mut visited = BTreeSet::new();
    let mut ir = last;
    while let Some(cur) = ir {
        v.check()?;
        let obj_nr = cur.num;
        if !visited.insert(obj_nr) {
            return Err(Error::Validation(format!(
                "outline item obj#{obj_nr}: outline item previous chain: cycle detected"
            )));
        }
        let d =
            v.x.dereference_dict_ref(cur)
                .map_err(|e| {
                    e.context(format!(
                        "outline item obj#{obj_nr}: dereference previous chain"
                    ))
                })?
                .cloned();
        let Some(prev) = d.and_then(|d| d.indirect_ref_entry(b"Prev")) else {
            break;
        };
        if prev.num == dupl_obj_nr {
            if let Some(d) = stored_dict_mut(v.x, obj_nr) {
                d.set(
                    b"Prev",
                    Object::Ref(Ref {
                        num: one_before_dupl_obj,
                        generation: 0,
                    }),
                );
            }
            return Ok(Some(cur));
        }
        if prev.num == one_before_dupl_obj {
            return Ok(Some(cur));
        }
        ir = Some(prev);
    }

    Ok(None)
}

/// `removeDuplFirst`: the first item is a duplicate; the list starts at the item before it.
fn remove_dupl_first(
    v: &mut Validator<'_>,
    first: &mut Option<Ref>,
    last: Option<Ref>,
    dupl_obj_nr: i64,
    one_before_dupl_obj: i64,
) -> Result<(), Error> {
    let next = first_of_remainder(v, last, dupl_obj_nr, one_before_dupl_obj)?;
    let Some(next) = next.filter(|r| r.num != 0) else {
        return Err(Error::Sentinel(Sentinel::BookmarksRepair));
    };
    if let Some(d) = stored_dict_mut(v.x, next.num) {
        d.0.remove(b"Prev".as_slice());
    }
    if let Some(f) = first.as_mut() {
        f.num = one_before_dupl_obj;
    }
    Ok(())
}

/// `handleDuplicate` (relaxed): the item `obj_nr` was seen before. `prev` is the item before it
/// in the list, `None` for the first item.
fn handle_duplicate(
    v: &mut Validator<'_>,
    is_first: bool,
    first: &mut Option<Ref>,
    last: Option<Ref>,
    prev: Option<Ref>,
    obj_nr: i64,
) -> Result<(), Error> {
    let prev_obj_nr = prev.map_or(0, |p| p.num);
    if is_first {
        return remove_dupl_first(v, first, last, obj_nr, prev_obj_nr);
    }

    let next = first_of_remainder(v, last, obj_nr, prev_obj_nr)?;
    let Some(next) = next.filter(|r| r.num != 0) else {
        return Err(Error::Sentinel(Sentinel::BookmarksRepair));
    };

    let next_ref = prev
        .and_then(|p| stored_dict(v.x, p.num))
        .and_then(|d| d.indirect_ref_entry(b"Next"));
    if next_ref.is_none() {
        return Err(Error::Sentinel(Sentinel::BookmarksRepair));
    }

    if let Some(d) = prev.and_then(|p| stored_dict_mut(v.x, p.num)) {
        d.set(
            b"Next",
            Object::Ref(Ref {
                num: next.num,
                generation: 0,
            }),
        );
    }

    Ok(())
}

/// `scanAndFixOutlineItems`: walks the list that starts with `first`, repairs cycles, a `Prev`
/// of the first item and duplicates (items that `seen` already holds), and marks the items seen.
fn scan_and_fix_outline_items(
    v: &mut Validator<'_>,
    first: &mut Option<Ref>,
    last: Option<Ref>,
    seen: &mut BTreeSet<i64>,
) -> Result<(), Error> {
    let mut visited = BTreeSet::new();
    let mut prev: Option<Ref> = None;
    let mut is_first = true;

    let mut ir = *first;
    while let Some(cur) = ir {
        v.check()?;
        let obj_nr = cur.num;

        if !visited.insert(obj_nr) {
            // handleCircular (relaxed): the previous item is the last of the list.
            if let (Some(p), Some(f)) = (prev, *first)
                && let Some(d) = stored_dict_mut(v.x, p.num)
            {
                d.set(b"Prev", Object::Ref(f));
                d.0.remove(b"Next".as_slice());
            }
            return Ok(());
        }

        let dict = v
            .x
            .dereference_dict_ref(cur)
            .map_err(|e| e.context(format!("outline item obj#{obj_nr}: dereference item list")))?
            .cloned();
        let Some(dict) = dict.filter(|d| !d.is_empty()) else {
            // handleCorruptDict (relaxed)
            return Err(Error::Sentinel(Sentinel::BookmarksRepair));
        };

        if is_first
            && dict.0.get(b"Prev".as_slice()).is_some_and(|o| !o.is_null())
            && let Some(d) = stored_dict_mut(v.x, obj_nr)
        {
            d.0.remove(b"Prev".as_slice());
        }

        if seen.contains(&obj_nr) {
            return handle_duplicate(v, is_first, first, last, prev, obj_nr);
        }

        seen.insert(obj_nr);
        prev = Some(cur);
        is_first = false;
        ir = dict.indirect_ref_entry(b"Next");
    }

    Ok(())
}

/// `removeOutlines`: the document has no outline.
fn remove_outlines(x: &mut XRefTable, root: &mut Dict) {
    x.outlines = None;
    root.0.remove(b"Outlines".as_slice());
}

/// `validateOutlinesGeneral`: the outline root's `Type`, `First`, `Last` and `Count`. A root
/// without items removes the outline (`None`, `None`, `None`).
fn validate_outlines_general(
    x: &mut XRefTable,
    root: &mut Dict,
    d: &Dict,
) -> Result<(Option<Ref>, Option<Ref>, Option<i64>), Error> {
    // Type, optional, name
    validate_name_entry(
        x,
        d,
        "outlineDict",
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Outlines" || s == b"Outline" || s == b"BMoutlines"),
    )?;

    let first = d.indirect_ref_entry(b"First");
    let last = d.indirect_ref_entry(b"Last");

    if first.is_none() {
        if last.is_some() {
            return Err(Error::Validation("missing First".to_string()));
        }
        remove_outlines(x, root);
        return Ok((None, None, None));
    }

    let count = dereference_outline_count(x, d)?;

    Ok((first, last, count))
}

/// `scanAndFixOutlines`: scans, validates and counts the outline items. Corrupt items remove the
/// outline.
fn scan_and_fix_outlines(
    v: &mut Validator<'_>,
    root: &mut Dict,
    first: Ref,
    last: Option<Ref>,
    count: Option<i64>,
) -> Result<(), Error> {
    let mut seen = BTreeSet::new();
    let mut first = Some(first);

    if let Err(e) = scan_and_fix_outline_items(v, &mut first, last, &mut seen) {
        if e.is(Sentinel::BookmarksRepair) {
            remove_outlines(v.x, root);
            return Ok(());
        }
        return Err(e.context("outline item list: scan"));
    }

    let (total, visible) = match validate_outline_tree_depth(v, first, &mut seen, 0) {
        Ok(tv) => tv,
        Err(e) if e.is(Sentinel::BookmarksRepair) => {
            remove_outlines(v.x, root);
            return Ok(());
        }
        Err(e) => return Err(e.context("outline item tree")),
    };

    validate_outline_count(total, visible, count).map_err(|e| e.context("outline root"))
}

/// `validateOutlines` (12.3.3 Document Outline): the `Outlines` entry of the catalog.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_outlines(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(ir) = validate_ind_ref_entry(v.x, root, "rootDict", "Outlines", required, since)?
    else {
        return Ok(());
    };

    let d =
        v.x.dereference_dict_ref(ir)
            .map_err(|e| e.context("outline root: dereference"))?
            .cloned();
    let Some(d) = d else {
        remove_outlines(v.x, root);
        return Ok(());
    };

    v.x.outlines = Some(d.clone());

    let (first, last, count) =
        validate_outlines_general(v.x, root, &d).map_err(|e| e.context("outline root"))?;
    let Some(first) = first else {
        return Ok(());
    };

    scan_and_fix_outlines(v, root, first, last, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;

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

    fn table(objects: Vec<(i64, Dict)>) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        for (n, d) in objects {
            x.table.insert(n, Entry::gen0(Object::Dict(d)));
        }
        x
    }

    fn prev_of(x: &XRefTable, n: i64) -> Option<i64> {
        stored_dict(x, n)
            .and_then(|d| d.indirect_ref_entry(b"Prev"))
            .map(|r| r.num)
    }

    /// `TestOutlinePreviousChain`: backward cycles and terminating repair paths.
    #[test]
    fn previous_chain() {
        for scenario in [
            "self",
            "pair",
            "duplicate",
            "predecessor",
            "end",
            "canceled",
        ] {
            let last = r(7);
            let mut d = dict(&[
                ("Title", Object::string("item")),
                ("Prev", Object::Ref(r(8))),
            ]);
            match scenario {
                "self" => d.set(b"Prev", Object::Ref(last)),
                "duplicate" => d.set(b"Prev", Object::Ref(r(6))),
                "predecessor" => d.set(b"Prev", Object::Ref(r(5))),
                "end" => {
                    d.0.remove(b"Prev".as_slice());
                }
                _ => {}
            }
            let mut x = table(vec![(7, d), (8, dict(&[("Prev", Object::Ref(r(7)))]))]);
            let cancel = AtomicBool::new(scenario == "canceled");
            let mut v = Validator::new(&mut x, &cancel);
            let res = first_of_remainder(&mut v, Some(last), 6, 5);
            match scenario {
                "self" | "pair" => {
                    let e = res.unwrap_err();
                    assert!(
                        e.to_string().contains("previous chain: cycle detected"),
                        "{scenario}: {e}"
                    );
                }
                "canceled" => {
                    assert!(matches!(res.unwrap_err().root(), Error::Cancelled));
                }
                "end" => assert_eq!(res.unwrap(), None, "{scenario}"),
                _ => {
                    assert_eq!(res.unwrap(), Some(last), "{scenario}");
                    assert_eq!(prev_of(&x, 7), Some(5), "{scenario}");
                }
            }
        }
    }

    fn item(parent: i64, prev: Option<i64>, next: Option<i64>) -> Dict {
        let mut d = dict(&[
            ("Title", Object::string("item")),
            ("Parent", Object::Ref(r(parent))),
        ]);
        if let Some(p) = prev {
            d.set(b"Prev", Object::Ref(r(p)));
        }
        if let Some(n) = next {
            d.set(b"Next", Object::Ref(r(n)));
        }
        d
    }

    fn run(x: &mut XRefTable, root: &mut Dict) -> Result<(), Error> {
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(x, &cancel);
        validate_outlines(&mut v, root, OPTIONAL, Version::V10)
    }

    /// A two-item outline validates, a leaf's `Count` is dropped.
    #[test]
    fn a_flat_outline_validates() {
        let mut one = item(1, None, Some(3));
        one.set(b"Count", Object::Integer(4));
        let mut x = table(vec![
            (
                1,
                dict(&[
                    ("Type", Object::name("Outlines")),
                    ("First", Object::Ref(r(2))),
                    ("Last", Object::Ref(r(3))),
                    ("Count", Object::Integer(2)),
                ]),
            ),
            (2, one),
            (3, item(1, Some(2), None)),
        ]);
        let mut root = dict(&[("Outlines", Object::Ref(r(1)))]);
        run(&mut x, &mut root).unwrap();
        assert!(root.has_entry(b"Outlines"));
        assert!(x.outlines.is_some());
        assert!(!stored_dict(&x, 2).unwrap().has_entry(b"Count"));
    }

    /// A parent with children gets its `Count` repaired.
    #[test]
    fn a_count_is_repaired() {
        let mut parent = item(1, None, None);
        parent.set(b"First", Object::Ref(r(3)));
        parent.set(b"Last", Object::Ref(r(3)));
        parent.set(b"Count", Object::Integer(9));
        let mut x = table(vec![
            (
                1,
                dict(&[
                    ("First", Object::Ref(r(2))),
                    ("Last", Object::Ref(r(2))),
                    ("Count", Object::Integer(2)),
                ]),
            ),
            (2, parent),
            (3, item(2, None, None)),
        ]);
        let mut root = dict(&[("Outlines", Object::Ref(r(1)))]);
        run(&mut x, &mut root).unwrap();
        let count = stored_dict(&x, 2).and_then(|d| d.int_entry(b"Count"));
        assert_eq!(count, Some(1));
    }

    /// A circular list is cut, a corrupt item removes the outline.
    #[test]
    fn a_cycle_is_cut_and_a_corrupt_item_removes_the_outline() {
        let mut x = table(vec![
            (
                1,
                dict(&[
                    ("First", Object::Ref(r(2))),
                    ("Last", Object::Ref(r(3))),
                    ("Count", Object::Integer(2)),
                ]),
            ),
            (2, item(1, None, Some(3))),
            (3, item(1, Some(2), Some(2))),
        ]);
        let mut root = dict(&[("Outlines", Object::Ref(r(1)))]);
        run(&mut x, &mut root).unwrap();
        assert!(root.has_entry(b"Outlines"));
        let last = stored_dict(&x, 3).unwrap();
        assert!(last.indirect_ref_entry(b"Next").is_none());

        let mut x = table(vec![
            (
                1,
                dict(&[
                    ("First", Object::Ref(r(2))),
                    ("Last", Object::Ref(r(2))),
                    ("Count", Object::Integer(1)),
                ]),
            ),
            (2, Dict::default()),
        ]);
        let mut root = dict(&[("Outlines", Object::Ref(r(1)))]);
        run(&mut x, &mut root).unwrap();
        assert!(!root.has_entry(b"Outlines"));
        assert!(x.outlines.is_none());
    }
}
