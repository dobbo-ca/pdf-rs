//! The structure tree (Go: validate/structTree.go, structure_hierarchy.go).
//!
//! Relaxed validation only. What Go digests as a spec violation (an unexpected `Type` or object
//! in a `K` entry, a duplicate node of the hierarchy, a missing `S` or `P`) has no effect and is
//! not collected, so those branches return `Ok`. The structure tree's own errors (a cycle, an
//! unsupported object) are [`Error::Validation`] with Go's message.
//!
//! One change reaches the table: a structure element whose `S` is an integer gets the name of
//! that integer (structTree.go:518). Go changes the stored map; here the element is a copy, so
//! the change is written back when the element was reached through a reference. A direct
//! element inside a `K` array or dictionary is not stored back (it has no reference to find it
//! by).

use super::Validator;
use super::name_tree::validate_name_tree;
use super::number_tree::{
    check_validation_tree, store_dict_entry, validate_number_tree, wrap_recursion_error,
};
use super::object::{
    OPTIONAL, object_context, validate_dict_entry, validate_integer_entry, validate_name_entry,
    validate_string_entry,
};
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, Ref};
use std::collections::{BTreeMap, BTreeSet};

/// `model.StructureTreeVisit` (recursion.go:217): the ancestors of the node being validated,
/// and the depth at which each completed subtree was validated.
#[derive(Debug, Default)]
struct StructureTraversal {
    ancestors: BTreeSet<i64>,
    validated: BTreeMap<i64, u64>,
}

impl StructureTraversal {
    /// `Enter`: rejects a cycle. An object number of 0 (a direct object) is not tracked.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::StructureTreeCycle));
        }
        Ok(())
    }

    /// `Leave`.
    fn leave(&mut self, obj_nr: i64) {
        self.ancestors.remove(&obj_nr);
    }

    /// `AlreadyValidated`: whether a completed subtree covers the starting depth.
    fn already_validated(&self, obj_nr: i64, depth: u64) -> bool {
        obj_nr != 0 && self.validated.get(&obj_nr).is_some_and(|d| depth <= *d)
    }

    /// `MarkValidated`.
    fn mark_validated(&mut self, obj_nr: i64, depth: u64) {
        if obj_nr == 0 {
            return;
        }
        let d = self.validated.entry(obj_nr).or_insert(depth);
        if depth > *d {
            *d = depth;
        }
    }
}

/// The object number of `o` if it is a reference, else 0 (`validationObjectNumber(0, o)`).
fn ref_num(o: Option<&Object>) -> i64 {
    match o {
        Some(Object::Ref(r)) => r.num,
        _ => 0,
    }
}

fn unsupported(o: &Object) -> Error {
    Error::Validation(format!("unsupported PDF object: {}", type_name(o)))
}

/// `validateMarkedContentReferenceDict`.
fn validate_marked_content_reference_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    // Pg: optional, the page the marked-content sequence is rendered on.
    if let Some(ir) = d.indirect_ref_entry(b"Pg") {
        process_struct_element_dict_pg_entry(x, ir)
            .map_err(|e| e.context("marked content reference Pg"))?;
    }

    // Stm: optional, the content stream containing the marked-content sequence.
    if let Some(ir) = d.indirect_ref_entry(b"Stm") {
        x.dereference(&Object::Ref(ir))
            .map_err(|e| e.context("marked content reference Stm: dereference"))?;
    }

    // StmOwn: optional, the object owning the stream.
    if let Some(ir) = d.indirect_ref_entry(b"StmOwn") {
        x.dereference(&Object::Ref(ir))
            .map_err(|e| e.context("marked content reference StmOwn: dereference"))?;
    }

    // MCID: required, integer
    let Some(obj) = d.find(b"MCID") else {
        return Err(Error::Validation(
            "marked content reference: missing MCID".to_string(),
        ));
    };
    x.dereference_integer(obj)
        .map_err(|e| e.context("marked content reference MCID: dereference"))?;
    Ok(())
}

/// `validateObjectReferenceDict`.
fn validate_object_reference_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    if let Some(ir) = d.indirect_ref_entry(b"Pg") {
        process_struct_element_dict_pg_entry(x, ir)
            .map_err(|e| e.context("object reference Pg"))?;
    }

    // Obj: required, indirect reference; relaxed skips a missing or null one.
    let Some(ir) = d.indirect_ref_entry(b"Obj") else {
        return Ok(());
    };
    x.dereference(&Object::Ref(ir))
        .map_err(|e| e.context("object reference Obj: dereference"))?;
    Ok(())
}

/// `enterStructureTreeObject`: the cancel check, and the object number of `o` after entering it
/// (0 for a direct object).
fn enter_structure_tree_object(
    v: &Validator<'_>,
    visit: &mut StructureTraversal,
    o: &Object,
) -> Result<i64, Error> {
    v.check()?;
    let Object::Ref(r) = o else {
        return Ok(0);
    };
    visit.enter(r.num)?;
    Ok(r.num)
}

/// `validateStructElementKArrayElement`: an integer, a structure element, a marked-content
/// reference or an object reference. `raw` is the object `o` was found as. Relaxed validation
/// digests a dictionary of another `Type`.
fn validate_struct_element_k_array_element(
    v: &mut Validator<'_>,
    o: &Object,
    raw: &Object,
    use_ids: bool,
    depth: u64,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    match o {
        Object::Integer(_) => Ok(()),
        Object::Dict(d) => {
            let (t, _) =
                v.x.dereference_name_entry(d, b"Type")
                    .map_err(|e| e.context("structure element K Type"))?;
            match t.as_deref() {
                None | Some(b"StructElem") => validate_struct_element_dict_depth(
                    v,
                    d,
                    Some(raw),
                    use_ids,
                    depth.saturating_add(1),
                    visit,
                ),
                Some(b"MCR") => validate_marked_content_reference_dict(v.x, d),
                Some(b"OBJR") => validate_object_reference_dict(v.x, d),
                Some(_) => Ok(()),
            }
        }
        other => Err(unsupported(other)),
    }
}

/// `validateStructElementDictEntryKArrayElement`: the element `index` of a `K` array.
fn validate_struct_element_dict_entry_k_array_element(
    v: &mut Validator<'_>,
    raw: &Object,
    index: usize,
    use_ids: bool,
    depth: u64,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    let context = object_context(&format!("structure element K[{index}]"), Some(raw));
    let n = enter_structure_tree_object(v, visit, raw).map_err(|e| e.context(context.clone()))?;
    let r = (|| -> Result<(), Error> {
        let o =
            v.x.dereference(raw)
                .map_err(|e| e.context(format!("{context}: dereference")))?
                .clone();
        if o.is_null() {
            return Ok(());
        }
        validate_struct_element_k_array_element(v, &o, raw, use_ids, depth, visit)
            .map_err(|e| wrap_recursion_error(context.clone(), e))
    })();
    visit.leave(n);
    r
}

/// `validateStructElementDictEntryKDepth`: the `K` entry. It is a structure element, a
/// marked-content reference, an object reference, a marked-content identifier, or an array of
/// these.
fn validate_struct_element_dict_entry_k_depth(
    v: &mut Validator<'_>,
    raw: &Object,
    use_ids: bool,
    depth: u64,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    let context = object_context("structure element K", Some(raw));
    let n = enter_structure_tree_object(v, visit, raw).map_err(|e| e.context(context.clone()))?;
    let r = (|| -> Result<(), Error> {
        let o =
            v.x.dereference(raw)
                .map_err(|e| e.context(format!("{context}: dereference")))?
                .clone();
        match &o {
            Object::Null | Object::Integer(_) => Ok(()),
            Object::Dict(_) => {
                validate_struct_element_k_array_element(v, &o, raw, use_ids, depth, visit)
                    .map_err(|e| wrap_recursion_error(context.clone(), e))
            }
            Object::Array(a) => {
                for (i, e) in a.iter().enumerate() {
                    validate_struct_element_dict_entry_k_array_element(
                        v, e, i, use_ids, depth, visit,
                    )?;
                }
                Ok(())
            }
            other => Err(unsupported(other).context(context.clone())),
        }
    })();
    visit.leave(n);
    r
}

/// `processStructElementDictPgEntry`: relaxed validation accepts a `Pg` that is not a page, so
/// only the lookups happen (they can load objects).
fn process_struct_element_dict_pg_entry(x: &mut XRefTable, ir: Ref) -> Result<(), Error> {
    let o = x
        .dereference(&Object::Ref(ir))
        .map_err(|e| e.context("page: dereference"))?
        .clone();
    if let Object::Dict(page) = &o {
        // The result does not matter: a missing or wrong `Type` is skipped.
        let _ = x.dereference_name_entry(page, b"Type");
    }
    Ok(())
}

/// `validateStructElementDictEntryA`: attribute objects, a dictionary or stream or an array of
/// these, each element possibly followed by a revision number.
fn validate_struct_element_dict_entry_a(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    match x
        .dereference(o)
        .map_err(|e| e.context("structure element A: dereference"))?
    {
        Object::Null | Object::Dict(_) | Object::Stream(_) => Ok(()),
        Object::Array(a) => {
            let a = a.clone();
            for (i, e) in a.iter().enumerate() {
                match x
                    .dereference(e)
                    .map_err(|e| e.context(format!("structure element A[{i}]: dereference")))?
                {
                    Object::Null | Object::Integer(_) | Object::Dict(_) | Object::Stream(_) => {}
                    other => {
                        return Err(unsupported(other).context(format!("structure element A[{i}]")));
                    }
                }
            }
            Ok(())
        }
        other => Err(unsupported(other).context("structure element A")),
    }
}

/// `validateStructElementDictEntryC`: a class name or an array of names, each possibly followed
/// by a revision number.
fn validate_struct_element_dict_entry_c(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    match x
        .dereference(o)
        .map_err(|e| e.context("structure element C: dereference"))?
    {
        Object::Null | Object::Name(_) => Ok(()),
        Object::Array(a) => {
            let a = a.clone();
            for (i, e) in a.iter().enumerate() {
                match x
                    .dereference(e)
                    .map_err(|e| e.context(format!("structure element C[{i}]: dereference")))?
                {
                    Object::Null | Object::Name(_) | Object::Integer(_) => {}
                    other => {
                        return Err(unsupported(other).context(format!("structure element C[{i}]")));
                    }
                }
            }
            Ok(())
        }
        other => Err(unsupported(other).context("structure element C")),
    }
}

/// `validateStructElementDictEntryPg`.
fn validate_struct_element_dict_entry_pg(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    if let Some(ir) = d.indirect_ref_entry(b"Pg") {
        process_struct_element_dict_pg_entry(x, ir)
            .map_err(|e| e.context("structure element Pg"))?;
    }
    Ok(())
}

/// `validateStructElementDictEntryS` (14.7.2), relaxed: `S` is optional, and an integer is
/// accepted and replaced by its name in the stored element (`raw` is where `d` was found).
fn validate_struct_element_dict_entry_s(
    x: &mut XRefTable,
    d: &Dict,
    raw: Option<&Object>,
    dict_name: &str,
) -> Result<(), Error> {
    if validate_name_entry(x, d, dict_name, "S", OPTIONAL, Version::V10, None).is_ok() {
        return Ok(());
    }
    let i = validate_integer_entry(x, d, dict_name, "S", OPTIONAL, Version::V10, None)?;
    if let Some(i) = i {
        store_dict_entry(x, raw, b"S", Object::Name(i.to_string().into_bytes()));
    }
    Ok(())
}

/// `validateStructElementDictPart1`: `S`, `ID`, `Pg`, `K` and `A`. (`P` is not checked in relaxed
/// mode.)
fn validate_struct_element_dict_part1(
    v: &mut Validator<'_>,
    d: &Dict,
    raw: Option<&Object>,
    dict_name: &str,
    use_ids: bool,
    depth: u64,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    validate_struct_element_dict_entry_s(v.x, d, raw, dict_name)?;

    if use_ids {
        // ID: optional, byte string
        validate_string_entry(v.x, d, dict_name, "ID", OPTIONAL, Version::V10, None)?;
    }

    validate_struct_element_dict_entry_pg(v.x, d)?;

    // K: optional, the children of this structure element.
    if let Some(o) = d.find(b"K") {
        validate_struct_element_dict_entry_k_depth(v, o, use_ids, depth, visit)?;
    }

    // A: optional, attribute objects.
    if let Some(o) = d.find(b"A") {
        validate_struct_element_dict_entry_a(v.x, o)?;
    }
    Ok(())
}

/// `validateStructElementLang`: relaxed validation accepts a name for `Lang` (a digested spec
/// violation).
fn validate_struct_element_lang(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    if let Some(o) = d.find(b"Lang") {
        let is_name = matches!(x.dereference(o)?, Object::Name(_));
        if is_name {
            return x.validate_version(&format!("dict={dict_name} entry=Lang"), since);
        }
    }
    validate_string_entry(x, d, dict_name, "Lang", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateStructElementDictPart2`: `C`, `R`, `T`, `Lang`, `Alt`, `E` and `ActualText`.
fn validate_struct_element_dict_part2(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // C: optional, name or array
    if let Some(o) = d.find(b"C") {
        validate_struct_element_dict_entry_c(x, o)?;
    }

    // R: optional, integer >= 0
    validate_integer_entry(
        x,
        d,
        dict_name,
        "R",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i >= 0),
    )?;

    // T: optional, text string
    validate_string_entry(x, d, dict_name, "T", OPTIONAL, Version::V10, None)?;

    // Lang: optional, text string, since 1.4 (1.3 in relaxed mode)
    validate_struct_element_lang(x, d, dict_name, Version::V13)?;

    // Alt: optional, text string
    validate_string_entry(x, d, dict_name, "Alt", OPTIONAL, Version::V10, None)?;

    // E: optional, text string, since 1.5 (1.4 in relaxed mode)
    validate_string_entry(x, d, dict_name, "E", OPTIONAL, Version::V14, None)?;

    // ActualText: optional, text string, since 1.4 (1.3 in relaxed mode)
    validate_string_entry(x, d, dict_name, "ActualText", OPTIONAL, Version::V13, None)?;
    Ok(())
}

/// `validateStructElementDictContext`: the structure element `d`, which is the object `raw`, as
/// the value of an ID tree (`use_ids`).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_struct_element_dict_context(
    v: &mut Validator<'_>,
    d: &Dict,
    raw: Option<&Object>,
    use_ids: bool,
) -> Result<(), Error> {
    let mut visit = StructureTraversal::default();
    validate_struct_element_dict_depth(v, d, raw, use_ids, 0, &mut visit)
}

/// `validateStructElementDictDepth` (table 323).
fn validate_struct_element_dict_depth(
    v: &mut Validator<'_>,
    d: &Dict,
    raw: Option<&Object>,
    use_ids: bool,
    depth: u64,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    check_validation_tree(v, "structure tree", depth)?;
    let owner = ref_num(raw);
    if visit.already_validated(owner, depth) {
        return Ok(());
    }

    let dict_name = "StructElementDict";

    validate_struct_element_dict_part1(v, d, raw, dict_name, use_ids, depth, visit)?;
    validate_struct_element_dict_part2(v.x, d, dict_name)?;
    visit.mark_validated(owner, depth);
    Ok(())
}

/// `validateStructTreeRootDictEntryKArrayElement`: relaxed validation digests an element that is
/// no structure element.
fn validate_struct_tree_root_dict_entry_k_array_element(
    v: &mut Validator<'_>,
    raw: &Object,
    index: usize,
    use_ids: bool,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    let context = object_context(&format!("structure tree root K[{index}]"), Some(raw));
    let n = enter_structure_tree_object(v, visit, raw).map_err(|e| e.context(context.clone()))?;
    let r = (|| -> Result<(), Error> {
        let o =
            v.x.dereference(raw)
                .map_err(|e| e.context(format!("{context}: dereference")))?
                .clone();
        let Object::Dict(d) = &o else {
            return Ok(());
        };
        let (t, _) =
            v.x.dereference_name_entry(d, b"Type")
                .map_err(|e| e.context(format!("{context}: Type")))?;
        if matches!(t.as_deref(), None | Some(b"StructElem")) {
            validate_struct_element_dict_depth(v, d, Some(raw), use_ids, 1, visit)
                .map_err(|e| e.context(context.clone()))?;
        }
        Ok(())
    })();
    visit.leave(n);
    r
}

/// `validateStructTreeRootDictEntryKDepth`: the immediate child or children of the structure
/// tree root, a structure element or an array of them.
fn validate_struct_tree_root_dict_entry_k_depth(
    v: &mut Validator<'_>,
    raw: &Object,
    use_ids: bool,
    visit: &mut StructureTraversal,
) -> Result<(), Error> {
    let context = object_context("structure tree root K", Some(raw));
    let n = enter_structure_tree_object(v, visit, raw).map_err(|e| e.context(context.clone()))?;
    let r = (|| -> Result<(), Error> {
        let o =
            v.x.dereference(raw)
                .map_err(|e| e.context(format!("{context}: dereference")))?
                .clone();
        match &o {
            Object::Dict(d) => {
                let (t, _) =
                    v.x.dereference_name_entry(d, b"Type")
                        .map_err(|e| e.context(format!("{context}: Type")))?;
                if matches!(t.as_deref(), None | Some(b"StructElem")) {
                    validate_struct_element_dict_depth(v, d, Some(raw), use_ids, 1, visit)
                        .map_err(|e| e.context(context.clone()))?;
                }
                Ok(())
            }
            Object::Array(a) => {
                for (i, e) in a.iter().enumerate() {
                    validate_struct_tree_root_dict_entry_k_array_element(v, e, i, use_ids, visit)?;
                }
                Ok(())
            }
            // Relaxed: null, and an object of another type, are digested.
            _ => Ok(()),
        }
    })();
    visit.leave(n);
    r
}

/// `validateStructTreeRootKContext`: the `K` entry of the structure tree root, or a value of its
/// parent tree.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_struct_tree_root_k_context(
    v: &mut Validator<'_>,
    o: &Object,
    use_ids: bool,
) -> Result<(), Error> {
    let mut visit = StructureTraversal::default();
    validate_struct_tree_root_dict_entry_k_depth(v, o, use_ids, &mut visit)
}

/// `processStructTreeClassMapDict`: every value is a dictionary or an array of dictionaries.
fn process_struct_tree_class_map_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    for (name, o) in d.iter() {
        let name = String::from_utf8_lossy(name);

        match x
            .dereference(o)
            .map_err(|e| e.context(format!("structure tree ClassMap {name}: dereference")))?
        {
            Object::Null | Object::Dict(_) => {}
            Object::Array(a) => {
                let a = a.clone();
                for (i, e) in a.iter().enumerate() {
                    x.dereference_dict(e).map_err(|err| {
                        err.context(format!(
                            "structure tree ClassMap {name}[{i}]: dereference dict"
                        ))
                    })?;
                }
            }
            other => {
                return Err(unsupported(other).context(format!("structure tree ClassMap {name}")));
            }
        }
    }
    Ok(())
}

/// `validateStructTreeRootDictEntryParentTree`: relaxed validation accepts an empty dictionary.
fn validate_struct_tree_root_dict_entry_parent_tree(
    v: &mut Validator<'_>,
    ir: Ref,
    use_ids: bool,
) -> Result<(), Error> {
    let r = Object::Ref(ir);
    let d =
        v.x.dereference_dict(&r)
            .map_err(|e| e.context("structure tree ParentTree: dereference"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };
    if d.is_empty() {
        return Ok(());
    }

    validate_number_tree(v, "StructTree", &d, true, use_ids, Some(&r))?;
    Ok(())
}

/// `validateStructTreeRootType`.
fn validate_struct_tree_root_type(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let (t, _) = x
        .dereference_name_entry(d, b"Type")
        .map_err(|e| e.context("structure tree root Type"))?;
    if t.as_deref() != Some(b"StructTreeRoot".as_slice()) {
        return Err(Error::Validation(
            "structure tree root: missing Type StructTreeRoot".to_string(),
        ));
    }
    Ok(())
}

/// `validateStructureHierarchy` (structure_hierarchy.go:36): checks the `K` hierarchy for cycles
/// and depth before the semantic validation. A node reached twice is digested (relaxed).
fn validate_structure_hierarchy(v: &mut Validator<'_>, root: &Dict) -> Result<(), Error> {
    v.check()?;
    validate_struct_tree_root_type(v.x, root)?;
    // Index values are references, not additional ownership paths.
    let raw = root.find(b"K").cloned().unwrap_or(Object::Null);
    let mut visit = StructureTraversal::default();
    let mut seen = BTreeSet::new();
    structure_hierarchy_object(
        v,
        &raw,
        1,
        &mut visit,
        &mut seen,
        true,
        "structure tree root K",
    )
}

/// `structureHierarchyObject`.
fn structure_hierarchy_object(
    v: &mut Validator<'_>,
    raw: &Object,
    depth: u64,
    visit: &mut StructureTraversal,
    seen: &mut BTreeSet<i64>,
    allow_array: bool,
    label: &str,
) -> Result<(), Error> {
    let r = (|| -> Result<(), Error> {
        v.check()?;
        let n = enter_structure_tree_object(v, visit, raw)?;
        let r = (|| -> Result<(), Error> {
            let o = v.x.dereference(raw)?.clone();
            match &o {
                Object::Dict(d) => structure_hierarchy_dict(v, d, n, depth, visit, seen),
                // Nested arrays are invalid K children; the semantic validation handles them.
                Object::Array(a) if allow_array => {
                    for (i, child) in a.iter().enumerate() {
                        structure_hierarchy_object(
                            v,
                            child,
                            depth,
                            visit,
                            seen,
                            false,
                            &format!("{label}[{i}]"),
                        )?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        })();
        visit.leave(n);
        r
    })();
    r.map_err(|e| wrap_recursion_error(object_context(label, Some(raw)), e))
}

/// `structureHierarchyDict`.
fn structure_hierarchy_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    n: i64,
    depth: u64,
    visit: &mut StructureTraversal,
    seen: &mut BTreeSet<i64>,
) -> Result<(), Error> {
    let (t, _) = v.x.dereference_name_entry(d, b"Type")?;
    if t.as_deref().is_some_and(|t| t != b"StructElem") {
        return Ok(());
    }
    check_validation_tree(v, "structure tree", depth)?;
    // Relaxed: a node that belongs to two parents is digested, and its subtree is not walked
    // again.
    if n != 0 && !seen.insert(n) {
        return Ok(());
    }
    let Some(raw) = d.find(b"K") else {
        return Ok(());
    };
    structure_hierarchy_object(
        v,
        raw,
        depth.saturating_add(1),
        visit,
        seen,
        true,
        "structure element K",
    )
}

/// `validateStructTreeRootDict`.
fn validate_struct_tree_root_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "StructTreeRootDict";

    // required entry Type: name:StructTreeRoot
    validate_structure_hierarchy(v, d)?;

    let mut use_ids = false;

    // Optional entry IDTree: name tree, key=elementId value=struct element dict
    if let Some(ir) = d.indirect_ref_entry(b"IDTree") {
        let r = Object::Ref(ir);
        let d1 =
            v.x.dereference_dict(&r)
                .map_err(|e| e.context("structure tree IDTree: dereference"))?
                .cloned();
        if let Some(d1) = d1
            && !d1.is_empty()
        {
            validate_name_tree(v, "IDTree", &d1, true, Some(&r))
                .map_err(|e| e.context("structure tree IDTree"))?;
            use_ids = true;
        }
    }

    // Optional entry K: struct element dict or array of struct element dicts
    if let Some(o) = d.find(b"K") {
        validate_struct_tree_root_k_context(v, o, use_ids)?;
    }

    // Optional entry ParentTree: number tree, value=indRef of struct element dict or array of
    // struct element dicts
    if let Some(ir) = d.indirect_ref_entry(b"ParentTree") {
        validate_struct_tree_root_dict_entry_parent_tree(v, ir, use_ids)?;
    }

    // Optional entry ParentTreeNextKey: integer
    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "ParentTreeNextKey",
        OPTIONAL,
        Version::V10,
        None,
    )?;

    // Optional entry RoleMap: dict
    validate_dict_entry(v.x, d, dict_name, "RoleMap", OPTIONAL, Version::V10, None)?;

    // Optional entry ClassMap: dict
    let d1 = validate_dict_entry(v.x, d, dict_name, "ClassMap", OPTIONAL, Version::V10, None)?;
    if let Some(d1) = d1 {
        process_struct_tree_class_map_dict(v.x, &d1)?;
    }
    Ok(())
}

/// `validateStructTree` (structTree.go:986): the `StructTreeRoot` entry of the catalog (14.7.2).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_struct_tree(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let d = validate_dict_entry(
        v.x,
        root,
        "RootDict",
        "StructTreeRoot",
        required,
        since,
        None,
    )?;
    let Some(d) = d else {
        return Ok(());
    };
    validate_struct_tree_root_dict(v, &d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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

    fn name(s: &str) -> Object {
        Object::Name(s.as_bytes().to_vec())
    }

    fn r(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn elem(s: &str, kids: Option<Object>) -> Object {
        let mut d = dict(&[("Type", name("StructElem")), ("S", name(s))]);
        if let Some(k) = kids {
            d.set(b"K", k);
        }
        Object::Dict(d)
    }

    fn root_with(k: Object) -> Dict {
        dict(&[("Type", name("StructTreeRoot")), ("K", k)])
    }

    #[test]
    fn a_tree_of_elements_validates() {
        let mut x = table();
        x.ind_ref_for_object(
            1,
            elem(
                "Document",
                Some(Object::Array(vec![r(2), Object::Integer(3)])),
            ),
        );
        x.ind_ref_for_object(2, elem("P", Some(Object::Integer(0))));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let root = root_with(r(1));
        assert_eq!(validate_struct_tree_root_dict(&mut v, &root), Ok(()));
    }

    #[test]
    fn a_cycle_is_an_error() {
        let mut x = table();
        x.ind_ref_for_object(1, elem("Document", Some(r(2))));
        x.ind_ref_for_object(2, elem("P", Some(r(1))));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let root = root_with(r(1));
        let err = validate_struct_tree_root_dict(&mut v, &root).unwrap_err();
        assert!(err.is(Sentinel::StructureTreeCycle), "{err}");
    }

    #[test]
    fn the_root_needs_its_type() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let root = dict(&[("K", Object::Array(vec![]))]);
        let err = validate_struct_tree_root_dict(&mut v, &root).unwrap_err();
        assert!(
            err.to_string()
                .contains("structure tree root: missing Type StructTreeRoot"),
            "{err}"
        );
    }

    #[test]
    fn an_integer_s_becomes_a_name_in_the_stored_element() {
        let mut x = table();
        let mut e = dict(&[("Type", name("StructElem"))]);
        e.set(b"S", Object::Integer(7));
        x.ind_ref_for_object(1, Object::Dict(e));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let root = root_with(r(1));
        assert_eq!(validate_struct_tree_root_dict(&mut v, &root), Ok(()));
        let stored = v.x.dereference_dict(&r(1)).unwrap().cloned().unwrap();
        assert_eq!(stored.name_entry(b"S"), Some(b"7".as_slice()));
    }

    #[test]
    fn relaxed_validation_digests_foreign_k_entries() {
        let mut x = table();
        let other = Object::Dict(dict(&[("Type", name("Other"))]));
        let kids = Object::Array(vec![other, Object::Name(b"x".to_vec())]);
        x.ind_ref_for_object(1, elem("Document", Some(kids)));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        // The array element that is a name is an unsupported object; a dictionary of another
        // Type is digested.
        let root = root_with(r(1));
        let err = validate_struct_tree_root_dict(&mut v, &root).unwrap_err();
        assert!(err.to_string().contains("unsupported PDF object"), "{err}");
    }

    #[test]
    fn a_struct_element_context_checks_its_entries() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let mut d = dict(&[("Type", name("StructElem")), ("S", name("P"))]);
        d.set(b"R", Object::Integer(-1));
        assert!(validate_struct_element_dict_context(&mut v, &d, None, true).is_err());
        d.set(b"R", Object::Integer(1));
        assert!(validate_struct_element_dict_context(&mut v, &d, None, true).is_ok());
    }
}
