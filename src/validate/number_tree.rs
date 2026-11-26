//! Number trees (Go: validate/numberTree.go).
//!
//! Relaxed validation only. What Go reports as a digested spec violation (keys out of order, a
//! leaf whose `/Limits` do not hold its keys, an odd `/Nums` array) has no effect and is not
//! collected. This file also holds `checkValidationTree` (numberTree.go:329), which the other
//! tree walks (outlines, form fields, structure tree, name trees) call.

use super::Validator;
use super::object::{
    OPTIONAL, REQUIRED, TreeVisit, validate_integer_array_entry, validate_integer_entry,
    validate_name_entry, validate_string_entry,
};
use super::struct_tree::validate_struct_tree_root_k_context;
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{EntryObject, Version, XRefTable};
use crate::object::{Dict, Object, Ref};

/// `checkValidationTree` (numberTree.go:329): the cancel check and the recursion limit for the
/// tree `name` at `depth`.
///
/// # Errors
/// [`Error::Cancelled`] and [`Sentinel::MaxRecursionDepth`].
pub fn check_validation_tree(v: &Validator<'_>, name: &str, depth: u64) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth(name, depth)
}

/// `model.WrapRecursionError` (recursion.go:129): adds `context` to `err` unless it is the
/// recursion limit, which is passed up unchanged.
pub(crate) fn wrap_recursion_error(context: String, err: Error) -> Error {
    if err.is(Sentinel::MaxRecursionDepth) {
        err
    } else {
        err.context(context)
    }
}

/// Writes `value` as the entry `key` of the dictionary the reference `o` points to, if `o` is a
/// reference. Go's validators change the stored map through the alias they were given; the
/// tree walks work on copies, so they write such a change back here. A dictionary that is not
/// reached through a reference (a direct one inside another object) cannot be found again, and
/// its change is not stored.
pub(crate) fn store_dict_entry(x: &mut XRefTable, o: Option<&Object>, key: &[u8], value: Object) {
    if let Some(Object::Ref(r)) = o
        && let Ok(Some(d)) = x.dereference_dict_mut(*r)
    {
        d.set(key, value);
    }
}

/// Replaces the object a reference points to by `o`, if it is a loaded plain object.
pub(crate) fn store_object(x: &mut XRefTable, r: Ref, o: Object) {
    if let Some(e) = x.entry_mut(r.num)
        && matches!(e.object, Some(EntryObject::Obj(_)))
    {
        e.object = Some(EntryObject::Obj(o));
    }
}

/// `validatePageLabelDict` (12.4.2): the value of a page label number tree.
fn validate_page_label_dict(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    let x = &mut *v.x;
    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context("PageLabel number tree value: dereference dict"))?
        .cloned();
    // Relaxed: a missing dictionary is skipped.
    let Some(d) = d else {
        return Ok(());
    };

    let dict_name = "pageLabelDict";

    validate_name_entry(
        x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"PageLabel"),
    )?;
    // The numbering style of the numeric portion of each page label.
    validate_name_entry(
        x,
        &d,
        dict_name,
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"D" | b"R" | b"r" | b"A" | b"a")),
    )?;
    validate_string_entry(x, &d, dict_name, "P", OPTIONAL, Version::V10, None)?;
    validate_integer_entry(
        x,
        &d,
        dict_name,
        "St",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i >= 1),
    )?;
    Ok(())
}

/// `validateNumberTreeKey` (7.9.7): the key `o` and whether it is valid. An invalid key of the
/// structure tree is skipped (relaxed); the ascending order is not checked (a digested spec
/// violation).
fn validate_number_tree_key(
    v: &mut Validator<'_>,
    o: &Object,
    name: &str,
) -> Result<(i64, bool), Error> {
    let o =
        v.x.dereference(o)
            .map_err(|e| e.context(format!("number tree {name} key: dereference")))?;
    if let Object::Integer(i) = o {
        return Ok((*i, true));
    }
    if name != "StructTree" {
        return Err(Error::Validation(format!(
            "number tree {name} key: expected integer, got {}",
            type_name(o)
        )));
    }
    Ok((0, false))
}

/// `numberTreeKeyRange`.
fn number_tree_key_range(
    name: &str,
    first: i64,
    last: i64,
    next_first: i64,
    next_last: i64,
    have_first: bool,
) -> (i64, i64) {
    if !have_first {
        return (next_first, next_last);
    }
    if name == "PageLabel" {
        // Include every index, even in an unsorted tree accepted in relaxed mode.
        return (first.min(next_first), last.max(next_last));
    }
    (first, next_last)
}

/// The initial key range of a tree: for page labels the page count and -1, so that the
/// minimum and maximum of the keys replace them.
fn initial_range(v: &Validator<'_>, name: &str) -> (i64, i64) {
    if name == "PageLabel" {
        (v.x.page_count, -1)
    } else {
        (0, 0)
    }
}

/// `validateNumberTreeDictNumsEntry` (7.9.7): the keys and values of a leaf.
fn validate_number_tree_dict_nums_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    name: &str,
    use_ids: bool,
) -> Result<(i64, i64), Error> {
    let (mut first_key, mut last_key) = initial_range(v, name);

    // Nums: array of the form [key1 value1 key2 value2 ... key n value n]
    let Some(o) = d.find(b"Nums") else {
        return Err(Error::Validation(format!(
            "number tree {name}: missing Kids or Nums"
        )));
    };
    let a =
        v.x.dereference_array(o)
            .map_err(|e| e.context(format!("number tree {name} Nums: dereference array")))?
            .cloned();
    let Some(a) = a else {
        return Err(Error::Validation(format!(
            "number tree {name}: missing Nums array"
        )));
    };

    // Relaxed: an odd array is a digested spec violation, and the tree is skipped.
    if !a.len().is_multiple_of(2) {
        return Ok((0, 0));
    }

    let mut have_first_key = false;

    for (i, o) in a.iter().enumerate() {
        v.check()?;
        if i.is_multiple_of(2) {
            let (key, valid) = validate_number_tree_key(v, o, name)
                .map_err(|e| e.context(format!("number tree {name} Nums[{i}]")))?;
            if !valid {
                continue;
            }
            (first_key, last_key) =
                number_tree_key_range(name, first_key, last_key, key, key, have_first_key);
            have_first_key = true;
            continue;
        }

        // The value is an indirect reference to a struct element dictionary or to an array of
        // them (structure tree), or a page label dictionary.
        let r = match name {
            "PageLabel" => validate_page_label_dict(v, o),
            "StructTree" => validate_struct_tree_root_k_context(v, o, use_ids),
            _ => Ok(()),
        };
        r.map_err(|e| e.context(format!("number tree {name} key {last_key}")))?;
    }

    Ok((first_key, last_key))
}

/// `validateNumberTreeDictLimitsEntry`: the `/Limits` of a node are required and have to be two
/// integers. Relaxed validation digests a range that does not hold the keys.
fn validate_number_tree_dict_limits_entry(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let a = validate_integer_array_entry(
        v.x,
        d,
        "numberTreeDict",
        "Limits",
        REQUIRED,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?
    .unwrap_or_default();

    for (i, o) in a.iter().enumerate() {
        v.x.dereference_integer(o)
            .map_err(|e| e.context(format!("number tree Limits[{i}]")))?;
    }
    Ok(())
}

/// `validateNumberTree` (numberTree.go:248): validates the number tree `name` with the root node
/// `d` (`root_object` is the object it came from) and returns its first and last key. `use_ids`
/// is for the parent tree of the structure tree.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_number_tree(
    v: &mut Validator<'_>,
    name: &str,
    d: &Dict,
    root: bool,
    use_ids: bool,
    root_object: Option<&Object>,
) -> Result<(i64, i64), Error> {
    let mut visit = TreeVisit::new(false, root_object);
    validate_number_tree_depth(v, name, d, root, use_ids, 0, &mut visit)
}

fn number_tree_kid_context(name: &str, o: &Object, i: usize) -> String {
    match o {
        Object::Ref(r) => format!("number tree {name} Kids[{i}] obj#{}", r.num),
        _ => format!("number tree {name} Kids[{i}]"),
    }
}

/// `validateNumberTreeDepth`.
fn validate_number_tree_depth(
    v: &mut Validator<'_>,
    name: &str,
    d: &Dict,
    root: bool,
    use_ids: bool,
    depth: u64,
    visit: &mut TreeVisit,
) -> Result<(i64, i64), Error> {
    check_validation_tree(v, &format!("number tree {name}"), depth)?;

    let (mut first_key, mut last_key) = initial_range(v, name);

    // A node has a "Kids" or a "Nums" entry.
    if let Some(o) = d.find(b"Kids") {
        let a =
            v.x.dereference_array(o)
                .map_err(|e| e.context(format!("number tree {name} Kids: dereference array")))?
                .cloned();
        let Some(a) = a else {
            return Err(Error::Validation(format!(
                "number tree {name}: missing Kids array"
            )));
        };

        let mut have_first_key = false;

        for (i, o) in a.iter().enumerate() {
            v.check()?;

            let d1 =
                v.x.dereference_dict(o)
                    .map_err(|e| {
                        e.context(format!(
                            "{}: dereference dict",
                            number_tree_kid_context(name, o, i)
                        ))
                    })?
                    .cloned();
            let Some(d1) = d1 else {
                return Err(Error::Validation(format!(
                    "{}: missing dict",
                    number_tree_kid_context(name, o, i)
                )));
            };

            let (fk, lk) = validate_number_tree_child(
                v,
                name,
                &d1,
                o,
                use_ids,
                depth.saturating_add(1),
                visit,
            )
            .map_err(|e| wrap_recursion_error(number_tree_kid_context(name, o, i), e))?;
            (first_key, last_key) =
                number_tree_key_range(name, first_key, last_key, fk, lk, have_first_key);
            have_first_key = true;
        }
    } else {
        // Leaf node
        (first_key, last_key) = validate_number_tree_dict_nums_entry(v, d, name, use_ids)?;
    }

    if !root {
        validate_number_tree_dict_limits_entry(v, d)
            .map_err(|e| e.context(format!("number tree {name} Limits")))?;
    }

    Ok((first_key, last_key))
}

/// `validateNumberTreeChild`.
fn validate_number_tree_child(
    v: &mut Validator<'_>,
    name: &str,
    d: &Dict,
    o: &Object,
    use_ids: bool,
    depth: u64,
    visit: &mut TreeVisit,
) -> Result<(i64, i64), Error> {
    let n = visit.enter(o)?;
    let r = validate_number_tree_depth(v, name, d, false, use_ids, depth, visit);
    visit.leave(n);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::XRefTable;
    use std::sync::atomic::AtomicBool;

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn ints(v: &[i64]) -> Object {
        Object::Array(v.iter().map(|i| Object::Integer(*i)).collect())
    }

    fn page_label(style: &str) -> Object {
        Object::Dict(dict(&[("S", Object::Name(style.as_bytes().to_vec()))]))
    }

    #[test]
    fn a_page_label_tree_gives_the_range_of_its_keys() {
        let mut x = table();
        x.page_count = 5;
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let nums = Object::Array(vec![
            Object::Integer(0),
            page_label("D"),
            Object::Integer(3),
            page_label("r"),
        ]);
        let d = dict(&[("Nums", nums)]);
        let r = validate_number_tree(&mut v, "PageLabel", &d, true, false, None);
        assert_eq!(r, Ok((0, 3)));
    }

    #[test]
    fn a_page_label_style_is_checked() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let nums = Object::Array(vec![Object::Integer(0), page_label("X")]);
        let d = dict(&[("Nums", nums)]);
        let r = validate_number_tree(&mut v, "PageLabel", &d, true, false, None);
        assert!(matches!(r, Err(Error::Context(..) | Error::Validation(_))));
    }

    #[test]
    fn an_odd_nums_array_is_skipped_and_an_integer_key_is_required() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let odd = dict(&[("Nums", ints(&[1, 2, 3]))]);
        assert_eq!(
            validate_number_tree(&mut v, "PageLabel", &odd, true, false, None),
            Ok((0, 0))
        );
        let bad_key = dict(&[(
            "Nums",
            Object::Array(vec![Object::Name(b"x".to_vec()), page_label("D")]),
        )]);
        assert!(validate_number_tree(&mut v, "PageLabel", &bad_key, true, false, None).is_err());
        // The structure tree skips a key that is no integer.
        let skipped = dict(&[(
            "Nums",
            Object::Array(vec![Object::Name(b"x".to_vec()), Object::Null]),
        )]);
        assert_eq!(
            validate_number_tree(&mut v, "StructTree", &skipped, true, false, None),
            Ok((0, 0))
        );
    }

    #[test]
    fn kids_need_limits_and_a_missing_kids_or_nums_is_an_error() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let leaf = dict(&[(
            "Nums",
            Object::Array(vec![Object::Integer(2), page_label("a")]),
        )]);
        let with_limits = {
            let mut l = leaf.clone();
            l.set(b"Limits", ints(&[2, 2]));
            l
        };
        let root = dict(&[("Kids", Object::Array(vec![Object::Dict(with_limits)]))]);
        assert_eq!(
            validate_number_tree(&mut v, "PageLabel", &root, true, false, None),
            Ok((2, 2))
        );
        let no_limits = dict(&[("Kids", Object::Array(vec![Object::Dict(leaf)]))]);
        assert!(validate_number_tree(&mut v, "PageLabel", &no_limits, true, false, None).is_err());
        let empty = Dict::default();
        assert!(validate_number_tree(&mut v, "PageLabel", &empty, true, false, None).is_err());
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let mut x = table();
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        let d = dict(&[("Nums", ints(&[]))]);
        assert_eq!(
            validate_number_tree(&mut v, "PageLabel", &d, true, false, None),
            Err(Error::Cancelled)
        );
    }
}
