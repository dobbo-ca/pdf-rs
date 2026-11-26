//! Pages (Go: validate/page.go): the page tree, the page dictionary, the resource dictionary and
//! the dictionaries of the page entries (transitions, viewports and measures, box colour
//! information, separation information).
//!
//! Differences from Go, besides those listed in [`super::object`]:
//!
//! * Go validates the page nodes through the maps the table stores, and changes them in place.
//!   Here a node is validated as a copy, which is stored back into its table entry when the
//!   validation of the node is done (also if it failed). What Go changes: the `Kids` array of a
//!   node (null and zero entries are dropped, and an indirect array becomes a direct one), the
//!   `Count` of the root, the `Parent` of the kids of a root that is repaired, the
//!   page `Contents` entry that is empty, the `Resources` dictionaries that have keys that are
//!   not allowed, and the transition style `Replace`.
//! * `hasResources` is not carried through the page tree walk: only strict validation reads it.
//!   The resource dictionaries are validated as in Go.
//! * A resource dictionary that is direct in a page or page node is repaired in the copy of
//!   that node. [`validate_resource_dict`] cannot do that for its caller, which gets the
//!   dictionary as an [`Object`]: a caller that stores the repaired dictionary uses
//!   [`validate_resource_dict_repaired`] (a resource dictionary that is a reference is changed in
//!   the table either way). The same goes for a direct `Trans` dictionary and for the direct
//!   rectangles of the page boxes.
//! * The traversal record that Go keeps in the `context.Context` is [`PassState::resources`]: the
//!   outermost call creates it and removes it when it returns. Its depth is
//!   [`PassState::resource_depth`].
//! * [`validate_group_entry`] ignores the version it gets: relaxed validation takes 1.2.
//! * `XRefTable.PageThumbs` is [`ValidateState::page_thumbs`].
//!
//! # Calls into the other validation packages
//!
//! `validateResourceCategory` calls, with the version of the category:
//!
//! | Category | Function | Package |
//! |---|---|---|
//! | `ExtGState` | `extgstate::validate_extgstate_resource_dict(v, o, since) -> Result<(), Error>` | 20 |
//! | `Font` | `font::validate_font_resource_dict_repaired(v, o, since) -> Result<Option<Dict>, Error>` | 18 |
//! | `XObject` | `xobject::validate_xobject_resource_dict(v, o, since) -> Result<(), Error>` | 19 |
//! | `Properties` | `property::validate_properties_resource_dict` | 13 |
//! | `ColorSpace` | `colorspace::validate_color_space_resource_dict(v, o, since) -> Result<Option<Dict>, Error>` | 19 |
//! | `Pattern` | `pattern::validate_pattern_resource_dict` | 20 |
//! | `Shading` | `shading::validate_shading_resource_dict` | 20 |
//!
//! The colour space and font functions return the repaired copy of a direct dictionary, and
//! the others return nothing. The page dictionary also calls
//! `action::validate_additional_actions_with_owner(v, d, dict_name, entry_name, required, since, source)`
//! (package 17), `xobject::validate_group_attributes_dict(v, o)` and
//! `xobject::validate_xobject_stream_dict(v, o)` (package 19) and
//! `colorspace::validate_color_space_array_subset` (package 19).
//!
//! # What the other packages call
//!
//! `validate_resource_dict` (patterns, properties, forms, fonts, XObjects),
//! `validate_transition_dict` (the `Trans` action), `validate_measure_dict` (annotations) and
//! `validate_group_entry` (XObjects).
//!
//! [`PassState::resources`]: super::state::PassState
//! [`PassState::resource_depth`]: super::state::PassState
//! [`ValidateState::page_thumbs`]: super::state::ValidateState

use super::metadata::validate_metadata;
use super::object::{
    GraphDepths, OPTIONAL, REQUIRED, ValidatedGraph, current_graph_depths, validate_array_entry,
    validate_boolean_entry, validate_date_entry, validate_dict_entry, validate_ind_ref_array_entry,
    validate_integer_array_entry, validate_integer_entry, validate_name_array_entry,
    validate_name_entry, validate_name_or_string_entry, validate_number_array_entry,
    validate_number_entry, validate_rectangle_entry, validate_stream_dict_entry,
    validate_string_entry,
};
use super::{
    Validator, action, colorspace, extgstate, font, pattern, property, shading,
    validate_piece_info, xobject,
};
use crate::error::Error;
use crate::ledger::{Bytes, Held};
use crate::model::deref::type_name;
use crate::model::{PageTreeVisit, Version, XRefTable};
use crate::object::{Array, Dict, Object, Rect, Ref, array_copy_bytes, dict_copy_bytes};
use std::collections::BTreeSet;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `types.MemberOf` for names.
fn member_of(s: &[u8], set: &[&str]) -> bool {
    set.iter().any(|m| m.as_bytes() == s)
}

/// The error of a Go nil dereference.
fn nil_deref() -> Error {
    Error::Panic("nil pointer dereference")
}

/// The resource categories in the order they are validated, with their versions
/// (`resourceDictValidators`).
const RESOURCE_CATEGORIES: [(&str, Version); 7] = [
    ("ExtGState", Version::V10),
    ("Font", Version::V10),
    ("XObject", Version::V10),
    ("Properties", Version::V10),
    ("ColorSpace", Version::V10),
    ("Pattern", Version::V10),
    ("Shading", Version::V13),
];

/// The keys a resource dictionary keeps; relaxed validation adds `Encoding` and `ProcSets`.
/// Since 1.4 `ProcSet` is obsolete.
const ALLOWED_RESOURCE_KEYS: [&str; 10] = [
    "ExtGState",
    "Font",
    "XObject",
    "Properties",
    "ColorSpace",
    "Pattern",
    "ProcSet",
    "Shading",
    "Encoding",
    "ProcSets",
];

/// `resourceTraversal`: the resource dictionaries on the path being validated, and those that
/// were validated already.
#[derive(Debug, Clone, Default)]
pub struct ResourceTraversal {
    active: BTreeSet<i64>,
    validated: ValidatedGraph,
}

/// `validateResourceCategory`: the category dictionary `o`. The result is the repaired copy if
/// `o` is a direct dictionary that the validator changed.
fn validate_resource_category(
    v: &mut Validator<'_>,
    name: &str,
    o: &mut Object,
    since: Version,
) -> Result<Option<Dict>, Error> {
    match name {
        "ExtGState" => extgstate::validate_extgstate_resource_dict(v, o, since).map(|()| None),
        "Font" => font::validate_font_resource_dict_repaired(v, o, since),
        "XObject" => xobject::validate_xobject_resource_dict(v, o, since).map(|()| None),
        "Properties" => property::validate_properties_resource_dict(v, o, since).map(|()| None),
        "ColorSpace" => colorspace::validate_color_space_resource_dict(v, o, since),
        "Pattern" => pattern::validate_pattern_resource_dict(v, o, since).map(|()| None),
        "Shading" => shading::validate_shading_resource_dict(v, o, since).map(|()| None),
        _ => Ok(None),
    }
}

/// `resourceTraversalFromContext`: runs `f` with the traversal of the pass, creating it if this
/// is the outermost call.
fn with_traversal<T>(v: &mut Validator<'_>, f: impl FnOnce(&mut Validator<'_>) -> T) -> T {
    let created = v.st.resources.is_none();
    if created {
        v.st.resources = Some(ResourceTraversal::default());
    }
    let r = f(v);
    if created {
        v.st.resources = None;
    }
    r
}

/// `validateResourceDict` (page.go:88): validates the resource dictionary `o` is or refers to
/// and says whether there is one.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an invalid category, the errors of the
/// dereference, and the depth error of a resource graph that is too deep.
pub fn validate_resource_dict(v: &mut Validator<'_>, o: &Object) -> Result<bool, Error> {
    validate_resource_dict_repaired(v, o).map(|(has_resources, _)| has_resources)
}

/// [`validate_resource_dict`], and the repaired copy of the dictionary if `o` is a direct
/// dictionary that lost keys. (A reference is changed in the table.)
///
/// # Errors
/// As [`validate_resource_dict`].
pub fn validate_resource_dict_repaired(
    v: &mut Validator<'_>,
    o: &Object,
) -> Result<(bool, Option<Dict>), Error> {
    with_traversal(v, |v| validate_traversal(v, o))
}

/// `resourceTraversal.validate`.
fn validate_traversal(v: &mut Validator<'_>, o: &Object) -> Result<(bool, Option<Dict>), Error> {
    v.check()?;
    let depths = current_graph_depths(&v.st);
    let (obj_nr, indirect) = match o {
        Object::Ref(r) => (r.num, true),
        _ => (0, false),
    };
    if indirect {
        let entered =
            v.st.resources
                .as_mut()
                .is_none_or(|t| t.active.insert(obj_nr));
        if !entered {
            // A cycle.
            return Ok((true, None));
        }
    }
    let r = validate_entered(v, o, obj_nr, depths);
    if indirect && let Some(t) = v.st.resources.as_mut() {
        t.active.remove(&obj_nr);
    }
    r
}

fn validate_entered(
    v: &mut Validator<'_>,
    o: &Object,
    obj_nr: i64,
    depths: GraphDepths,
) -> Result<(bool, Option<Dict>), Error> {
    let depth = v.st.resource_depth.saturating_add(1);
    v.x.check_recursion_depth("resource graph", u64::try_from(depth).unwrap_or(0))?;
    if v.st
        .resources
        .as_ref()
        .is_some_and(|t| t.validated.already_validated(obj_nr, depths))
    {
        return Ok((true, None));
    }
    v.st.resource_depth = depth;
    let r = validate_resources(v, o, obj_nr, depths);
    v.st.resource_depth = v.st.resource_depth.saturating_sub(1);
    r
}

fn validate_resources(
    v: &mut Validator<'_>,
    o: &Object,
    obj_nr: i64,
    depths: GraphDepths,
) -> Result<(bool, Option<Dict>), Error> {
    let d = v.x.dereference_dict(o)?.cloned();
    let Some(mut d) = d else {
        return Ok((false, None));
    };

    let mut changed = false;
    for (name, since) in RESOURCE_CATEGORIES {
        let Some(mut category) = d.find(name.as_bytes()).cloned() else {
            continue;
        };
        let repaired = validate_resource_category(v, name, &mut category, since)
            .map_err(|e| e.context(format!("resource dict {name}")))?;
        if let Some(repaired) = repaired {
            d.set(name.as_bytes(), Object::Dict(repaired));
            changed = true;
        }
    }

    let before = d.len();
    d.0.retain(|k, _| {
        ALLOWED_RESOURCE_KEYS
            .iter()
            .any(|allowed| allowed.as_bytes() == k.as_slice())
    });
    changed |= d.len() != before;

    let mut repaired = None;
    if changed {
        match o {
            Object::Ref(r) => store_dict(v.x, *r, d)?,
            _ => repaired = Some(d),
        }
    }
    if let Some(t) = v.st.resources.as_mut() {
        t.validated.mark_validated(obj_nr, depths);
    }
    Ok((true, repaired))
}

/// Stores `d` as the dictionary of the object `r` (Go changes the stored map in place).
fn store_dict(x: &mut XRefTable, r: Ref, d: Dict) -> Result<(), Error> {
    if let Some(stored) = x.dereference_dict_mut(r)? {
        *stored = d;
    }
    Ok(())
}

/// Stores `nd` as the value of the dictionary entry `key` of `d`: in the table if the entry is a
/// reference, else in `d`.
fn store_entry_dict(x: &mut XRefTable, d: &mut Dict, key: &[u8], nd: Dict) -> Result<(), Error> {
    if let Some(Object::Ref(r)) = d.find(key) {
        let r = *r;
        return store_dict(x, r, nd);
    }
    if let Some(slot) = d.find_mut(key)
        && matches!(slot, Object::Dict(_))
    {
        *slot = Object::Dict(nd);
    }
    Ok(())
}

/// `validateContents`: `o` is the dereferenced `Contents` entry of `d`. An empty array, string
/// or dictionary is removed from `d`.
fn validate_contents(x: &mut XRefTable, o: &Object, d: &mut Dict) -> Result<(), Error> {
    match o {
        // No further processing.
        Object::Stream(_) => {}

        // Process the array of content stream dicts.
        Object::Array(a) => {
            let mut has_contents = false;
            for obj in a {
                let (sd, _) = x
                    .dereference_stream_dict(obj)
                    .map_err(|e| e.context("page contents"))?;
                if sd.is_some() {
                    has_contents = true;
                }
            }
            if !has_contents {
                // Digest the empty array.
                d.delete(b"Contents");
            }
        }

        Object::String(s) => {
            if !String::from_utf8_lossy(s).trim().is_empty() {
                return Err(Error::Validation(format!(
                    "page contents: expected stream dict or array, got {}",
                    type_name(o)
                )));
            }
            // Digest the empty string literal.
            d.delete(b"Contents");
        }

        Object::Dict(c) => {
            if !c.is_empty() {
                return Err(Error::Validation(format!(
                    "page contents: expected stream dict or array, got {}",
                    type_name(o)
                )));
            }
            // Digest the empty dict.
            d.delete(b"Contents");
        }

        other => {
            return Err(Error::Validation(format!(
                "page contents: expected stream dict or array, got {}",
                type_name(other)
            )));
        }
    }
    Ok(())
}

/// `validatePageContents`.
fn validate_page_contents(x: &mut XRefTable, d: &mut Dict) -> Result<(), Error> {
    let Some(raw) = d.find(b"Contents").cloned() else {
        return Ok(());
    };
    let o = x
        .dereference(&raw)
        .map_err(|e| e.context("page contents"))?
        .clone();
    if o.is_null() {
        return Ok(());
    }
    validate_contents(x, &o, d)
}

/// `validatePageResources`: relaxed validation digests a missing resource dictionary (a page
/// without `Resources` of its own or an inherited one).
fn validate_page_resources(v: &mut Validator<'_>, d: &mut Dict) -> Result<(), Error> {
    let Some(o) = d.find(b"Resources").cloned() else {
        return Ok(());
    };
    let (_, repaired) =
        validate_resource_dict_repaired(v, &o).map_err(|e| e.context("page dict Resources"))?;
    if let Some(repaired) = repaired {
        d.set(b"Resources", Object::Dict(repaired));
    }
    Ok(())
}

/// `validatePageEntryMediaBox` and its siblings: the rectangle entry `key` of `d`. A direct
/// array that had a null coordinate repaired is stored back into `d`.
fn validate_rect(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
    key: &str,
    required: bool,
    since: Version,
) -> Result<Option<Array>, Error> {
    let a = validate_rectangle_entry(x, d, dict_name, key, required, since, None)?;
    if let Some(a) = &a
        && let Some(slot) = d.find_mut(key.as_bytes())
        && matches!(slot, Object::Array(_))
    {
        *slot = Object::Array(a.clone());
    }
    Ok(a)
}

/// `validateBoxStyleDictEntry` (14.11.2.2).
fn validate_box_style_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(());
    };

    let dict_name = "boxStyleDict";

    // C, number array with 3 elements, optional
    validate_number_array_entry(
        x,
        &d1,
        dict_name,
        "C",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 3),
    )?;

    // W, number, optional
    validate_number_entry(x, &d1, dict_name, "W", OPTIONAL, since, None)?;

    // S, name, optional
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "S",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["S", "D"])),
    )?;

    // D, array, optional, since V1.3, dashArray
    validate_integer_array_entry(x, &d1, dict_name, "D", OPTIONAL, since, None)?;
    Ok(())
}

/// `validatePageBoxColorInfo` (14.11.2.2).
fn validate_page_box_color_info(
    x: &mut XRefTable,
    page_dict: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d) = validate_dict_entry(
        x,
        page_dict,
        "pageDict",
        "BoxColorInfo",
        required,
        since,
        None,
    )?
    else {
        return Ok(());
    };
    for name in ["CropBox", "BleedBox", "TrimBox", "ArtBox"] {
        validate_box_style_dict_entry(x, &d, "boxColorInfoDict", name, OPTIONAL, since)?;
    }
    Ok(())
}

/// `validateGroupEntry`: the `Group` entry of `d`, a group attributes dictionary. Relaxed
/// validation takes version 1.2, whatever the caller says.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_group_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    required: bool,
    _since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(v.x, d, dict_name, "Group", required, Version::V12, None)?
    else {
        return Ok(());
    };
    xobject::validate_group_attributes_dict(v, &Object::Dict(d1)).map_err(|e| e.context("Group"))
}

/// `validatePageEntryThumb`: the thumbnail is an image XObject. The page records it.
fn validate_page_entry_thumb(
    v: &mut Validator<'_>,
    d: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(mut sd) =
        validate_stream_dict_entry(v.x, d, "pagesDict", "Thumb", required, since, None)?
    else {
        return Ok(());
    };
    // Go validates the stream with the map it shares with the table: the repairs (an inferred
    // `Subtype`) are stored.
    let res = xobject::validate_xobject_contents_inner(v, &mut sd);
    if let Some(ir) = d.indirect_ref_entry(b"Thumb") {
        xobject::persist_stream_entries(v.x, ir, &sd.dict);
    }
    res.map_err(|e| e.context("page Thumb"))?;

    // Go dereferences the nil reference of a direct stream.
    let r = d.indirect_ref_entry(b"Thumb").ok_or_else(nil_deref)?;
    let cur_page = v.x.cur_page;
    v.x.vstate.page_thumbs.insert(cur_page, r);
    Ok(())
}

/// `validateTransitionDictEntryDi`: a number from a short list, or the name `None`.
fn validate_transition_dict_entry_di(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let Some(raw) = d.find(b"Di") else {
        return Ok(());
    };
    if let Object::Ref(r) = raw
        && x.find_table_entry_for_ind_ref(*r).is_none_or(|e| e.free)
    {
        return Err(Error::Validation(
            "transition dict: entry Di missing indirect target".to_string(),
        ));
    }

    let valid_number = |f: f64| [0.0, 90.0, 180.0, 270.0, 315.0].contains(&f);
    match x.dereference(raw)? {
        Object::Null => Ok(()),
        Object::Integer(i) => {
            if valid_number(*i as f64) {
                return Ok(());
            }
            Err(Error::Validation(
                "transition dict: entry Di number value undefined".to_string(),
            ))
        }
        Object::Float(f) => {
            if valid_number(*f) {
                return Ok(());
            }
            Err(Error::Validation(
                "transition dict: entry Di number value undefined".to_string(),
            ))
        }
        Object::Name(n) => {
            if n.as_slice() == b"None" {
                return Ok(());
            }
            Err(Error::Validation(
                "transition dict: entry Di name value undefined".to_string(),
            ))
        }
        other => Err(Error::Validation(format!(
            "transition dict: entry Di invalid type {}",
            type_name(other)
        ))),
    }
}

/// `repairTransitionStyle`: relaxed validation converts the style `Replace` to `R`.
fn repair_transition_style(x: &mut XRefTable, d: &mut Dict) -> Result<(), Error> {
    let (s, _) = x.dereference_name_entry(d, b"S")?;
    if matches!(s.as_deref(), Some(b"Replace")) {
        d.update(b"S", Object::name("R"));
    }
    Ok(())
}

/// `validateTransitionDict` (12.4.4): the transition dictionary `d`, in which relaxed validation
/// repairs the style `Replace`. A caller that keeps `d` stores it back.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_transition_dict(x: &mut XRefTable, d: &mut Dict) -> Result<(), Error> {
    let dict_name = "transitionDict";
    repair_transition_style(x, d).map_err(|e| e.context(format!("{dict_name}.S")))?;

    // S, name, optional
    let mut styles = vec!["Split", "Blinds", "Box", "Wipe", "Dissolve", "Glitter", "R"];
    if x.version()? >= Version::V15 {
        styles.extend(["Fly", "Push", "Cover", "Uncover", "Fade"]);
    }
    let trans_style = validate_name_entry(
        x,
        d,
        dict_name,
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| member_of(s, &styles)),
    )?;
    let fly = matches!(trans_style.as_deref(), Some(b"Fly"));

    // D, optional, number > 0
    validate_number_entry(
        x,
        d,
        dict_name,
        "D",
        OPTIONAL,
        Version::V10,
        Some(&|f: f64| f > 0.0),
    )?;

    // Dm, optional, name, see 12.4.4
    let split_or_blinds = matches!(trans_style.as_deref(), Some(b"Split" | b"Blinds"));
    validate_name_entry(
        x,
        d,
        dict_name,
        "Dm",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| member_of(s, &["H", "V"]) && split_or_blinds),
    )?;

    // M, optional, name
    let m_style = matches!(trans_style.as_deref(), Some(b"Split" | b"Box" | b"Fly"));
    validate_name_entry(
        x,
        d,
        dict_name,
        "M",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| member_of(s, &["I", "O"]) && m_style),
    )?;

    // Di, optional, number or name
    validate_transition_dict_entry_di(x, d)?;

    // SS, optional, number, since V1.5
    if fly {
        validate_number_entry(x, d, dict_name, "SS", OPTIONAL, Version::V15, None)?;
    }

    // B, optional, boolean, since V1.5
    validate_boolean_entry(
        x,
        d,
        dict_name,
        "B",
        OPTIONAL,
        Version::V15,
        Some(&|_: bool| fly),
    )?;
    Ok(())
}

/// `validatePageEntryTrans`: a direct dictionary is repaired in `d`, a referenced one in the
/// table.
fn validate_page_entry_trans(
    x: &mut XRefTable,
    d: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(mut t) = validate_dict_entry(x, d, "pagesDict", "Trans", required, since, None)?
    else {
        return Ok(());
    };
    let r = validate_transition_dict(x, &mut t);
    let stored = store_entry_dict(x, d, b"Trans", t);
    r?;
    stored
}

/// `validatePageEntryTabs`: relaxed validation also accepts a string.
fn validate_page_entry_tabs(
    x: &mut XRefTable,
    d: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    const TABS: [&str; 5] = ["R", "C", "S", "A", "W"];
    let r = validate_name_entry(
        x,
        d,
        "pagesDict",
        "Tabs",
        required,
        since,
        Some(&|s: &[u8]| member_of(s, &TABS)),
    );
    if r.is_ok() {
        return Ok(());
    }
    validate_string_entry(
        x,
        d,
        "pagesDict",
        "Tabs",
        required,
        since,
        Some(&|s: &str| member_of(s.as_bytes(), &TABS)),
    )
    .map(|_| ())
}

/// `validatePageEntryPresSteps` (12.4.4.2): not implemented in Go either, a dictionary is an
/// error.
fn validate_page_entry_pres_steps(
    x: &mut XRefTable,
    d: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    if validate_dict_entry(x, d, "pagesDict", "PresSteps", required, since, None)?.is_none() {
        return Ok(());
    }
    Err(Error::Validation(
        "presentation steps dict: missing supported entry NA".to_string(),
    ))
}

/// `validateNumberFormatDict` (table 262).
fn validate_number_format_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "numberFormatDict";

    // Type, name, optional
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"NumberFormat"),
    )?;

    // U, text string, required
    validate_string_entry(x, d, dict_name, "U", REQUIRED, since, None)?;

    // C, number, required
    validate_number_entry(x, d, dict_name, "C", REQUIRED, since, None)?;

    // F, name, optional
    let format = validate_name_entry(
        x,
        d,
        dict_name,
        "F",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["D", "F", "R", "T"])),
    )?;

    // D, integer, optional
    let precision =
        validate_integer_entry(x, d, dict_name, "D", OPTIONAL, since, Some(&|i: i64| i > 0))?;
    if let Some(p) = precision
        && matches!(format.as_deref(), None | Some(b"D"))
        && p % 10 != 0
    {
        return Err(Error::Validation(format!(
            "{dict_name}.D: decimal precision must be a multiple of 10"
        )));
    }

    // FD, bool, optional
    validate_boolean_entry(x, d, dict_name, "FD", OPTIONAL, since, None)?;

    // RT, RD, PS and SS, text strings, optional
    for name in ["RT", "RD", "PS", "SS"] {
        validate_string_entry(x, d, dict_name, name, OPTIONAL, since, None)?;
    }

    // O, name, optional
    validate_name_entry(
        x,
        d,
        dict_name,
        "O",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["S", "P"])),
    )?;
    Ok(())
}

/// `validateNumberFormatArrayEntry`: a non empty array of number format dictionaries.
fn validate_number_format_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(
        x,
        d,
        dict_name,
        entry_name,
        required,
        since,
        Some(&|a: &[Object]| !a.is_empty()),
    )?
    else {
        return Ok(());
    };
    for (i, o) in a.iter().enumerate() {
        let nf = x
            .dereference_dict(o)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?
            .cloned();
        let Some(nf) = nf else {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: missing number format dict"
            )));
        };
        validate_number_format_dict(x, &nf, since)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?;
    }
    Ok(())
}

/// `validateRectilinearMeasureDict` (table 261).
fn validate_rectilinear_measure_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "rectilinearMeasureDict";

    // R, text string, required, scale ratio
    validate_string_entry(x, d, dict_name, "R", REQUIRED, since, None)?;

    // X, number format array, required, for measurement of change along the x axis and, if Y
    // is not present, along the y axis as well.
    validate_number_format_array_entry(x, d, dict_name, "X", REQUIRED, since)?;

    // Y, number format array, required when the x and y scales have different units or
    // conversion factors.
    validate_number_format_array_entry(x, d, dict_name, "Y", OPTIONAL, since)?;

    // D, number format array, required, for measurement of distance in any direction.
    validate_number_format_array_entry(x, d, dict_name, "D", REQUIRED, since)?;

    // A, number format array, required, for measurement of area.
    validate_number_format_array_entry(x, d, dict_name, "A", REQUIRED, since)?;

    // T, number format array, optional, for measurement of angles.
    validate_number_format_array_entry(x, d, dict_name, "T", OPTIONAL, since)?;

    // S, number format array, optional, for measurement of the slope of a line.
    validate_number_format_array_entry(x, d, dict_name, "S", OPTIONAL, since)?;

    // O, number array, optional, two numbers: the origin of the measurement coordinate system
    // in default user space coordinates.
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "O",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )?;

    // CYX, number, optional, a factor that converts the largest units along the y axis to the
    // largest units along the x axis.
    validate_number_entry(x, d, dict_name, "CYX", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMeasureCoordinateSystemDict`.
fn validate_measure_coordinate_system_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "measureCoordinateSystemDict";

    let cs_type = validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        REQUIRED,
        since,
        Some(&|s: &[u8]| member_of(s, &["GEOGCS", "PROJCS"])),
    )?;

    let epsg = validate_integer_entry(
        x,
        d,
        dict_name,
        "EPSG",
        OPTIONAL,
        since,
        Some(&|i: i64| i > 0),
    )?;

    let wkt = validate_string_entry(
        x,
        d,
        dict_name,
        "WKT",
        OPTIONAL,
        since,
        Some(&|s: &str| s.is_ascii()),
    )?;
    if wkt.as_ref().is_some_and(String::is_empty) {
        return Err(Error::Validation(
            "measure coordinate system dict: WKT must not be empty".to_string(),
        ));
    }

    if epsg.is_none() && wkt.is_none() {
        return Err(Error::Validation(
            "measure coordinate system dict: one of EPSG or WKT required".to_string(),
        ));
    }
    if x.version()? == Version::V20
        && matches!(cs_type.as_deref(), Some(b"GEOGCS"))
        && epsg.is_some()
        && wkt.is_some()
    {
        return Err(Error::Validation(
            "measure coordinate system dict: EPSG and WKT are mutually exclusive".to_string(),
        ));
    }
    Ok(())
}

/// `validateMeasureCoordinateSystemEntry`.
fn validate_measure_coordinate_system_entry(
    x: &mut XRefTable,
    d: &Dict,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let context = |e: Error| e.context(format!("geospatialMeasureDict.{entry_name}"));
    let d1 = validate_dict_entry(
        x,
        d,
        "geospatialMeasureDict",
        entry_name,
        required,
        since,
        None,
    )
    .map_err(context)?;
    let Some(d1) = d1 else {
        return Ok(());
    };
    validate_measure_coordinate_system_dict(x, &d1, since).map_err(context)
}

/// `validateMeasureUnitSquareArray`: relaxed validation allows a tolerance of 1e-5 around the
/// unit square.
fn validate_measure_unit_square_array(
    x: &mut XRefTable,
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    const TOLERANCE: f64 = 1e-5;
    let (min, max) = (-TOLERANCE, 1.0 + TOLERANCE);
    for (i, o) in a.iter().enumerate() {
        let f = x
            .dereference_number(o)
            .map_err(|e| e.context(format!("dict={dict_name} entry={entry_name} index={i}")))?;
        if !(min..=max).contains(&f) {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid value at index {i}: {f}"
            )));
        }
    }
    Ok(())
}

/// `validateMeasureDisplayUnits`: the units of length, area and angle.
fn validate_measure_display_units(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    const VALID_UNITS: [&[&str]; 3] = [
        &["M", "KM", "FT", "USFT", "MI", "NM"],
        &["SQM", "HA", "SQKM", "SQFT", "A", "SQMI"],
        &["DEG", "GRD"],
    ];
    for (i, o) in a.iter().enumerate() {
        let name = match x
            .dereference(o)
            .map_err(|e| e.context(format!("geospatialMeasureDict.PDU index={i}")))?
        {
            Object::Name(n) => n.clone(),
            other => {
                return Err(Error::Validation(format!(
                    "geospatialMeasureDict.PDU invalid type at index {i}: {}",
                    type_name(other)
                )));
            }
        };
        if !VALID_UNITS
            .get(i)
            .is_some_and(|units| member_of(&name, units))
        {
            return Err(Error::Validation(format!(
                "geospatialMeasureDict.PDU invalid value at index {i}: /{}",
                lossy(&name)
            )));
        }
    }
    Ok(())
}

/// `validateGeospatialMeasureDictPart1`.
fn validate_geospatial_measure_dict_part1(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "geospatialMeasureDict";

    let bounds = validate_number_array_entry(
        x,
        d,
        dict_name,
        "Bounds",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() >= 6 && a.len().is_multiple_of(2)),
    )?;
    if let Some(bounds) = bounds {
        validate_measure_unit_square_array(x, &bounds, dict_name, "Bounds")?;
    }

    validate_measure_coordinate_system_entry(x, d, "GCS", REQUIRED, since)?;
    validate_measure_coordinate_system_entry(x, d, "DCS", OPTIONAL, since)?;

    let pdu = validate_name_array_entry(
        x,
        d,
        dict_name,
        "PDU",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 3),
    )?;
    if let Some(pdu) = pdu {
        validate_measure_display_units(x, &pdu)?;
    }
    Ok(())
}

/// `validateGeospatialMeasureDictPart2`.
fn validate_geospatial_measure_dict_part2(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "geospatialMeasureDict";
    let validate_pairs = |a: &[Object]| a.len() >= 4 && a.len().is_multiple_of(2);

    let gpts = validate_number_array_entry(
        x,
        d,
        dict_name,
        "GPTS",
        REQUIRED,
        since,
        Some(&validate_pairs),
    )?;
    let lpts = validate_number_array_entry(
        x,
        d,
        dict_name,
        "LPTS",
        OPTIONAL,
        since,
        Some(&validate_pairs),
    )?;
    if let Some(lpts) = lpts {
        if lpts.len() != gpts.map_or(0, |g| g.len()) {
            return Err(Error::Validation(format!(
                "{dict_name}: LPTS and GPTS array lengths differ"
            )));
        }
        validate_measure_unit_square_array(x, &lpts, dict_name, "LPTS")?;
    }

    // PCSM, since V2.0; relaxed validation accepts V1.7.
    let pcsm_version = if x.version()? == Version::V17 {
        Version::V17
    } else {
        Version::V20
    };
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "PCSM",
        OPTIONAL,
        pcsm_version,
        Some(&|a: &[Object]| a.len() == 12),
    )?;
    Ok(())
}

/// `validateGeospatialMeasureDict`.
fn validate_geospatial_measure_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    validate_geospatial_measure_dict_part1(x, d, since)?;
    validate_geospatial_measure_dict_part2(x, d, since)
}

/// `validateMeasureDict` (12.9): a rectilinear (`RL`, the default) or geospatial (`GEO`)
/// measure. Other subtypes are permitted but have unknown schemas.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_measure_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "measureDict";

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Measure"),
    )?;

    let subtype = validate_name_entry(x, d, dict_name, "Subtype", OPTIONAL, since, None)?;
    match subtype.as_deref() {
        None | Some(b"RL") => validate_rectilinear_measure_dict(x, d, since),
        Some(b"GEO") => validate_geospatial_measure_dict(x, d, since),
        _ => Ok(()),
    }
}

/// `validateViewportDict` (table 260).
fn validate_viewport_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "viewportDict";

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Viewport"),
    )?;

    validate_rectangle_entry(x, d, dict_name, "BBox", REQUIRED, since, None)?;

    validate_string_entry(x, d, dict_name, "Name", OPTIONAL, since, None)?;

    // Measure, optional, dict
    let d1 = validate_dict_entry(x, d, dict_name, "Measure", OPTIONAL, since, None)?;
    if let Some(d1) = d1 {
        validate_measure_dict(x, &d1, since)?;
    }
    Ok(())
}

/// `validatePageEntryVP`: an array of viewport dictionaries (null entries are skipped).
fn validate_page_entry_vp(
    x: &mut XRefTable,
    d: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, "pagesDict", "VP", required, since, None)? else {
        return Ok(());
    };
    for o in &a {
        if o.is_null() {
            continue;
        }
        let vp = x.dereference_dict(o)?.cloned();
        let Some(vp) = vp else {
            continue;
        };
        validate_viewport_dict(x, &vp, since)?;
    }
    Ok(())
}

/// `validatePageEntrySeparationInfo` (14.11.4).
fn validate_page_entry_separation_info(
    v: &mut Validator<'_>,
    pages_dict: &Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d) = validate_dict_entry(
        v.x,
        pages_dict,
        "pagesDict",
        "SeparationInfo",
        required,
        since,
        None,
    )?
    else {
        return Ok(());
    };

    let dict_name = "separationDict";

    validate_ind_ref_array_entry(v.x, &d, dict_name, "Pages", REQUIRED, since, None)?;

    validate_name_or_string_entry(v.x, &d, dict_name, "DeviceColorant", required, since)?;

    let a = validate_array_entry(v.x, &d, dict_name, "ColorSpace", OPTIONAL, since, None)?;
    if let Some(a) = a {
        colorspace::validate_color_space_array_subset(v, &a, &["Separation", "DeviceN"])
            .map_err(|e| e.context("separationDict.ColorSpace"))?;
    }
    Ok(())
}

/// `handlePieceInfo`: the page-piece dictionary and the modification date.
fn handle_piece_info(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // Relaxed validation takes version 1.0 for the page-piece dictionary.
    validate_piece_info(x, d, dict_name, "PieceInfo", OPTIONAL, Version::V10)?;

    // LastModified
    validate_date_entry(x, d, dict_name, "LastModified", OPTIONAL, Version::V13)?;
    Ok(())
}

/// The page dictionary entries `validatePageDict` validates after the additional actions, in
/// order, with their versions for relaxed validation.
const PAGE_ENTRIES: &[(&str, Version)] = &[
    ("CropBox", Version::V10),
    ("BleedBox", Version::V12),
    ("TrimBox", Version::V10),
    ("ArtBox", Version::V12),
    ("BoxColorInfo", Version::V14),
    ("Rotate", Version::V10),
    ("Group", Version::V14),
    ("Thumb", Version::V10),
    ("B", Version::V11),
    ("Dur", Version::V11),
    ("Trans", Version::V11),
    ("Metadata", Version::V14),
    ("StructParents", Version::V10),
    ("ID", Version::V13),
    ("PZ", Version::V13),
    ("SeparationInfo", Version::V13),
    ("Tabs", Version::V12),
    ("TemplateInstantiated", Version::V15),
    ("PresSteps", Version::V15),
    ("UserUnit", Version::V16),
    ("VP", Version::V14),
];

/// One entry of [`PAGE_ENTRIES`].
fn validate_page_entry(
    v: &mut Validator<'_>,
    d: &mut Dict,
    name: &str,
    since: Version,
) -> Result<(), Error> {
    const DICT_NAME: &str = "pagesDict";
    match name {
        "CropBox" | "BleedBox" | "TrimBox" | "ArtBox" => {
            validate_rect(v.x, d, DICT_NAME, name, OPTIONAL, since)?;
        }
        "BoxColorInfo" => validate_page_box_color_info(v.x, d, OPTIONAL, since)?,
        "Rotate" => {
            validate_integer_entry(
                v.x,
                d,
                DICT_NAME,
                "Rotate",
                OPTIONAL,
                since,
                Some(&|i: i64| i % 90 == 0),
            )?;
        }
        "Group" => validate_group_entry(v, d, "pageDict", OPTIONAL, since)?,
        "Thumb" => validate_page_entry_thumb(v, d, OPTIONAL, since)?,
        "B" => {
            // Only makes sense if the catalog has a "Threads" entry and bead dicts.
            validate_ind_ref_array_entry(v.x, d, DICT_NAME, "B", OPTIONAL, since, None)?;
        }
        "Dur" => {
            validate_number_entry(v.x, d, DICT_NAME, "Dur", OPTIONAL, since, None)?;
        }
        "Trans" => validate_page_entry_trans(v.x, d, OPTIONAL, since)?,
        "Metadata" => validate_metadata(v.x, d, OPTIONAL, since)?,
        "StructParents" => {
            validate_integer_entry(v.x, d, DICT_NAME, "StructParents", OPTIONAL, since, None)?;
        }
        "ID" => {
            validate_string_entry(v.x, d, DICT_NAME, "ID", OPTIONAL, since, None)?;
        }
        "PZ" => {
            // Preferred zoom factor, number.
            validate_number_entry(v.x, d, DICT_NAME, "PZ", OPTIONAL, since, None)?;
        }
        "SeparationInfo" => validate_page_entry_separation_info(v, d, OPTIONAL, since)?,
        "Tabs" => validate_page_entry_tabs(v.x, d, OPTIONAL, since)?,
        "TemplateInstantiated" => {
            validate_name_entry(
                v.x,
                d,
                DICT_NAME,
                "TemplateInstantiated",
                OPTIONAL,
                since,
                None,
            )?;
        }
        "PresSteps" => validate_page_entry_pres_steps(v.x, d, OPTIONAL, since)?,
        "UserUnit" => {
            // Positive number, since V1.6; relaxed validation takes V1.3.
            validate_number_entry(
                v.x,
                d,
                DICT_NAME,
                "UserUnit",
                OPTIONAL,
                Version::V13,
                Some(&|f: f64| f > 0.0),
            )?;
        }
        "VP" => validate_page_entry_vp(v.x, d, OPTIONAL, since)?,
        _ => {}
    }
    Ok(())
}

/// `validatePageDict`: the page dictionary `d`, which is changed as Go changes it. The result is
/// the page's `MediaBox`, empty if it has none.
fn validate_page_dict(
    v: &mut Validator<'_>,
    d: &mut Dict,
    has_media_box: bool,
) -> Result<Array, Error> {
    let dict_name = "pageDict";

    if d.indirect_ref_entry(b"Parent").is_none() {
        return Err(Error::Validation("page dict: missing parent".to_string()));
    }

    // Contents
    validate_page_contents(v.x, d)?;

    // Resources
    validate_page_resources(v, d)?;

    // MediaBox
    let media_box = validate_rect(v.x, d, dict_name, "MediaBox", !has_media_box, Version::V10)?
        .unwrap_or_default();

    // PieceInfo
    handle_piece_info(v.x, d, dict_name)?;

    // AA
    action::validate_additional_actions_with_owner(
        v,
        d,
        dict_name,
        "AA",
        OPTIONAL,
        Version::V11,
        "page",
    )
    .map_err(|e| e.context("pageDict.AA"))?;

    for (name, since) in PAGE_ENTRIES {
        validate_page_entry(v, d, name, *since)?;
    }

    Ok(media_box)
}

fn page_tree_page_error(err: Error, obj_nr: i64) -> Error {
    err.context(format!("page tree: page obj#{obj_nr}"))
}

fn page_tree_kid_error(err: Error, obj_nr: i64) -> Error {
    err.context(format!("page tree: kid obj#{obj_nr}"))
}

fn page_tree_node_entry_error(err: Error, obj_nr: i64, entry_name: &str) -> Error {
    err.context(format!("page tree: node obj#{obj_nr} {entry_name}"))
}

/// `validatePagesDictGeneralEntries`: the resources, `MediaBox`, `CropBox` and `Rotate` of a
/// page tree node. The result is the node's `MediaBox`, empty if it has none.
fn validate_pages_dict_general_entries(
    v: &mut Validator<'_>,
    d: &mut Dict,
    obj_nr: i64,
) -> Result<Array, Error> {
    // Resources: optional, dict
    if let Some(o) = d.find(b"Resources").cloned() {
        let (_, repaired) = validate_resource_dict_repaired(v, &o)
            .map_err(|e| e.context(format!("page tree: node obj#{obj_nr} Resources")))?;
        if let Some(repaired) = repaired {
            d.set(b"Resources", Object::Dict(repaired));
        }
    }

    // MediaBox: optional, rectangle
    let media_box = validate_rect(v.x, d, "pageDict", "MediaBox", OPTIONAL, Version::V10)
        .map_err(|e| page_tree_node_entry_error(e, obj_nr, "MediaBox"))?
        .unwrap_or_default();

    // CropBox: optional, rectangle
    validate_rect(v.x, d, "pagesDict", "CropBox", OPTIONAL, Version::V10)
        .map_err(|e| page_tree_node_entry_error(e, obj_nr, "CropBox"))?;

    // Rotate: optional, integer
    validate_integer_entry(
        v.x,
        d,
        "pagesDict",
        "Rotate",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i % 90 == 0),
    )
    .map_err(|e| page_tree_node_entry_error(e, obj_nr, "Rotate"))?;

    Ok(media_box)
}

/// `dictTypeForPageNodeDict`: the `Type` of a page node.
fn dict_type_for_page_node_dict(
    x: &mut XRefTable,
    d: &Dict,
    obj_nr: i64,
) -> Result<Vec<u8>, Error> {
    let (t, _) = x
        .dereference_name_entry(d, b"Type")
        .map_err(|e| e.context(format!("page tree: node obj#{obj_nr} Type")))?;
    t.ok_or_else(|| Error::Validation(format!("page tree: node obj#{obj_nr} missing Type")))
}

/// `pagesDictKids`: a copy of the `Kids` array of a page tree node, `None` if there is none.
/// The copy is charged to the ledger twice over (it and the kids a caller keeps beside it) until
/// the returned charge drops.
fn pages_dict_kids(x: &mut XRefTable, d: &Dict) -> Result<Option<(Array, Held)>, Error> {
    let Some(o) = d.find(b"Kids") else {
        return Ok(None);
    };
    let mem = x.mem().clone();
    let Some(a) = x.dereference_array(o)? else {
        return Ok(None);
    };
    let held = mem.charge(2 * array_copy_bytes(a))?;
    Ok(Some((a.clone(), held)))
}

/// `RectForArray`: the rectangle of a four element array of numbers.
fn rect_for_array(x: &mut XRefTable, a: &[Object]) -> Result<Rect, Error> {
    let [llx, lly, urx, ury] = a else {
        return Err(Error::Panic("index out of range"));
    };
    let llx = x.dereference_number(llx)?;
    let lly = x.dereference_number(lly)?;
    let urx = x.dereference_number(urx)?;
    let ury = x.dereference_number(ury)?;
    Ok(Rect::new(llx, lly, urx, ury))
}

/// `EmptyPage`: a page with an empty content stream, stored as object `obj_nr`.
fn empty_page(
    v: &mut Validator<'_>,
    parent: Ref,
    media_box: Option<Rect>,
    obj_nr: i64,
) -> Result<Ref, Error> {
    let mut sd = v.x.new_stream_dict_for_buf(Bytes::new(Vec::new(), 0, None));
    // Go passes a nil buffer, so the content stays nil after Encode.
    sd.content = None;
    sd.encode(v.x.mem(), v.cancel)
        .map_err(|e| e.context("empty page: encode content stream"))?;

    // A4 unless there is a media box.
    let arr = media_box
        .unwrap_or_else(|| Rect::for_dim(595.0, 842.0))
        .to_array();

    let contents =
        v.x.ind_ref_for_new_object(Object::Stream(Box::new(sd)))
            .map_err(|e| e.context("empty page: create content object"))?;

    let mut page = Dict::default();
    page.set(b"Type", Object::name("Page"));
    page.set(b"Parent", Object::Ref(parent));
    page.set(b"Resources", Object::Dict(Dict::default()));
    page.set(b"MediaBox", Object::Array(arr));
    page.set(b"Contents", Object::Ref(contents));
    Ok(v.x.ind_ref_for_object(obj_nr, Object::Dict(page)))
}

/// `detectPageNodeDict`: the dictionary of the kid `ir`. Relaxed validation replaces an empty
/// one with a blank page that has the media box in effect.
fn detect_page_node_dict(
    v: &mut Validator<'_>,
    ir: Ref,
    parent_obj_nr: i64,
    media_box: &[Object],
    page_nr: i64,
) -> Result<Dict, Error> {
    let d =
        v.x.dereference_dict_ref(ir)
            .map_err(|e| e.context(format!("page tree: kid obj#{}: dereference", ir.num)))?
            .cloned();
    if let Some(d) = d
        && !d.is_empty()
    {
        return Ok(d);
    }

    let media_box = if media_box.is_empty() {
        None
    } else {
        Some(rect_for_array(v.x, media_box)?)
    };
    let parent = Ref {
        num: parent_obj_nr,
        generation: 0,
    };
    empty_page(v, parent, media_box, ir.num)
        .map_err(|e| e.context(format!("page tree: corrupt page {page_nr}")))?;

    let d =
        v.x.dereference_dict_ref(ir)
            .map_err(|e| {
                e.context(format!(
                    "page tree: repaired kid obj#{}: dereference",
                    ir.num
                ))
            })?
            .cloned();
    Ok(d.unwrap_or_default())
}

/// `processPagesKids`: validates the kids of the page tree node `parent_obj_nr`. The result is
/// the array of the kids that are references to objects other than 0.
fn process_pages_kids(
    v: &mut Validator<'_>,
    kids: &[Object],
    parent_obj_nr: i64,
    media_box: &[Object],
    cur_page: &mut i64,
    depth: u64,
    visit: &mut PageTreeVisit,
) -> Result<Array, Error> {
    let mut media_box = media_box.to_vec();
    let mut a = Array::new();

    for (i, o) in kids.iter().enumerate() {
        v.check()?;

        if o.is_null() {
            continue;
        }

        let Object::Ref(ir) = o else {
            return Err(Error::Validation(format!(
                "page tree: parent obj#{parent_obj_nr} kid[{i}]: expected indirect reference, got {}",
                type_name(o)
            )));
        };
        let ir = *ir;
        let obj_nr = ir.num;
        if obj_nr == 0 {
            continue;
        }

        let mut page_node =
            detect_page_node_dict(v, ir, parent_obj_nr, &media_box, cur_page.saturating_add(1))?;

        a.push(Object::Ref(ir));

        // Relaxed validation digests a parent link that points to another node.
        if page_node.indirect_ref_entry(b"Parent").is_none() {
            return Err(Error::Validation(format!(
                "page tree: node obj#{obj_nr}: missing parent node, expected obj#{parent_obj_nr}"
            )));
        }

        let dict_type = dict_type_for_page_node_dict(v.x, &page_node, obj_nr)?;
        match dict_type.as_slice() {
            b"Pages" => {
                let r = validate_pages_dict_depth(
                    v,
                    &mut page_node,
                    obj_nr,
                    &media_box,
                    cur_page,
                    depth.saturating_add(1),
                    visit,
                );
                let stored = store_dict(v.x, ir, page_node);
                r.map_err(|e| page_tree_kid_error(e, obj_nr))?;
                stored?;
            }

            b"Page" => {
                v.st.rendition_pages.pages.insert(obj_nr);
                *cur_page = cur_page.saturating_add(1);
                v.x.cur_page = *cur_page;
                let r = validate_page_dict(v, &mut page_node, !media_box.is_empty());
                let stored = store_dict(v.x, ir, page_node);
                let d_media_box = r.map_err(|e| page_tree_page_error(e, obj_nr))?;
                stored?;
                if media_box.is_empty() {
                    media_box = d_media_box;
                }
                v.x.set_valid(ir)
                    .map_err(|e| e.context(format!("page tree: page obj#{obj_nr}: mark valid")))?;
            }

            other => {
                return Err(Error::Validation(format!(
                    "page tree: node obj#{obj_nr} unexpected dict type: {}",
                    lossy(other)
                )));
            }
        }
    }

    Ok(a)
}

/// `validatePagesDictDepth`: the page tree node `d` (changed as Go changes it), which is the
/// object `obj_nr`, at `depth` of the page tree. `media_box` is the inherited media box.
fn validate_pages_dict_depth(
    v: &mut Validator<'_>,
    d: &mut Dict,
    obj_nr: i64,
    media_box: &[Object],
    cur_page: &mut i64,
    depth: u64,
    visit: &mut PageTreeVisit,
) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth("page tree", depth)?;
    visit.enter(obj_nr)?;
    let r = validate_pages_node(v, d, obj_nr, media_box, cur_page, depth, visit);
    visit.leave(obj_nr);
    r
}

fn validate_pages_node(
    v: &mut Validator<'_>,
    d: &mut Dict,
    obj_nr: i64,
    media_box: &[Object],
    cur_page: &mut i64,
    depth: u64,
    visit: &mut PageTreeVisit,
) -> Result<(), Error> {
    let d_media_box = validate_pages_dict_general_entries(v, d, obj_nr)?;
    let media_box: &[Object] = if d_media_box.is_empty() {
        media_box
    } else {
        &d_media_box
    };

    let kids =
        pages_dict_kids(v.x, d).map_err(|e| e.context("page tree: dereference \"Kids\" entry"))?;
    let Some((kids, _copy)) = kids else {
        return Err(Error::Validation(format!(
            "page tree: node obj#{obj_nr} corrupt \"Kids\" entry"
        )));
    };

    if kids.is_empty() {
        return Ok(());
    }

    match process_pages_kids(v, &kids, obj_nr, media_box, cur_page, depth, visit) {
        Ok(a) => {
            d.set(b"Kids", Object::Array(a));
            Ok(())
        }
        Err(e) => {
            d.set(b"Kids", Object::Array(Array::new()));
            Err(e)
        }
    }
}

/// `repairPagesDict`: the `Pages` entry of the catalog is no reference. Relaxed validation moves
/// the page root into a new object, sets the `Parent` of its kids and the catalog entry. The
/// result is the page root and the reference to its object.
fn repair_pages_dict(
    v: &mut Validator<'_>,
    obj: &Object,
    root_dict: &mut Dict,
) -> Result<(Dict, Ref), Error> {
    let d =
        v.x.dereference_dict(obj)
            .map_err(|e| e.context("page tree repair: dereference page root"))?
            .cloned();
    let Some(d) = d else {
        return Err(Error::Validation(
            "page tree repair: cannot dereference page node".to_string(),
        ));
    };

    let ind_ref =
        v.x.ind_ref_for_new_object(Object::Dict(d.clone()))
            .map_err(|e| e.context("page tree repair: create page root reference"))?;

    root_dict.set(b"Pages", Object::Ref(ind_ref));

    // Patch kids.parents.
    let kids = pages_dict_kids(v.x, &d)
        .map_err(|e| e.context("page tree repair: dereference \"Kids\" entry"))?;
    let Some((kids, _copy)) = kids else {
        return Err(Error::Validation(
            "page tree repair: corrupt \"Kids\" entry".to_string(),
        ));
    };

    for (i, o) in kids.iter().enumerate() {
        v.check()?;

        if o.is_null() {
            continue;
        }

        let Object::Ref(ir) = o else {
            return Err(Error::Validation(format!(
                "page tree repair: kid[{i}]: expected indirect reference, got {}",
                type_name(o)
            )));
        };

        if ir.num == 0 {
            continue;
        }

        let kid = v
            .x
            .dereference_dict_mut(*ir)
            .map_err(|e| e.context(format!("page tree repair: kid obj#{}: dereference", ir.num)))?;
        let Some(kid) = kid else {
            return Err(Error::Validation(format!(
                "page tree repair: corrupt page node obj#{}",
                ir.num
            )));
        };
        kid.set(b"Parent", Object::Ref(ind_ref));
    }

    Ok((d, ind_ref))
}

/// `validateOrRepairPageTreeCount`: relaxed validation sets the `Count` of the root to the
/// number of pages found.
fn validate_or_repair_page_tree_count(x: &mut XRefTable, page_root: &mut Dict, actual_count: i64) {
    if actual_count == x.page_count {
        return;
    }
    page_root.set(b"Count", Object::Integer(actual_count));
    x.page_count = actual_count;
}

/// `validatePages` (page.go:1658): validates the page tree under the `Pages` entry of the
/// catalog and returns the dictionary of the root page node. A `Pages` entry that is no
/// reference is repaired in `root_dict`.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] and the errors of the lookups, and the page tree
/// errors of [`PageTreeVisit`].
pub fn validate_pages(v: &mut Validator<'_>, root_dict: &mut Dict) -> Result<Dict, Error> {
    v.check()?;
    let Some(obj) = root_dict.find(b"Pages") else {
        return Err(Error::Validation(
            "page tree root: missing \"Pages\"".to_string(),
        ));
    };
    // The copies of the page root are charged while the tree is validated.
    let mem = v.x.mem().clone();
    let _obj_copy = mem.charge(obj.copy_bytes())?;
    let obj = obj.clone();

    let (mut page_root, root_ref, _copy) = match &obj {
        Object::Ref(ir) => {
            let d =
                v.x.dereference_dict_ref(*ir)
                    .map_err(|e| e.context("page tree root: dereference"))?;
            let Some(d) = d else {
                return Err(Error::Validation(
                    "page tree root: cannot dereference page node".to_string(),
                ));
            };
            let held = mem.charge(dict_copy_bytes(d))?;
            (d.clone(), *ir, held)
        }
        other => {
            // The dereferenced copy, and the one that becomes the new object.
            let held = mem.charge(2 * other.copy_bytes())?;
            let (d, r) = repair_pages_dict(v, other, root_dict)?;
            (d, r, held)
        }
    };
    let obj_nr = root_ref.num;

    let Some(count) = page_root.find(b"Count").cloned() else {
        return Err(Error::Validation(
            "page tree root: missing \"Count\"".to_string(),
        ));
    };
    let count_context = if matches!(count, Object::Ref(_)) {
        format!("page tree root: obj#{obj_nr}")
    } else {
        "page tree root".to_string()
    };
    let i =
        v.x.dereference_integer(&count)
            .map_err(|e| e.context(format!("{count_context}: corrupt \"Count\"")))?;
    let Some(i) = i else {
        return Err(Error::Validation(format!(
            "{count_context}: corrupt \"Count\""
        )));
    };

    v.x.page_count = i;

    let mut pc = 0;
    let r = validate_pages_dict_depth(
        v,
        &mut page_root,
        obj_nr,
        &[],
        &mut pc,
        0,
        &mut PageTreeVisit::new(),
    );
    if r.is_ok() {
        validate_or_repair_page_tree_count(v.x, &mut page_root, pc);
    }
    let stored = store_dict(v.x, root_ref, page_root.clone());
    r?;
    stored?;

    Ok(page_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::Sentinel;
    use crate::model::Entry;
    use std::sync::atomic::AtomicBool;

    fn r(n: i64) -> Object {
        Object::Ref(Ref {
            num: n,
            generation: 0,
        })
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn ints(a: &[i64]) -> Object {
        Object::Array(a.iter().map(|i| Object::Integer(*i)).collect())
    }

    fn table(version: Version, objects: Vec<(i64, Object)>) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(version);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn stored_dict(x: &XRefTable, n: i64) -> Dict {
        match x.table.get(&n).and_then(|e| e.object.as_ref()) {
            Some(crate::model::EntryObject::Obj(Object::Dict(d))) => d.clone(),
            _ => Dict::default(),
        }
    }

    // TestRectangleMissingCoordinates (relaxed).
    #[test]
    fn a_rectangle_with_null_coordinates_is_repaired() {
        for version in [Version::V14, Version::V17, Version::V20] {
            for entry_name in ["MediaBox", "BBox"] {
                // A direct array with a null coordinate.
                let mut x = table(version, vec![]);
                let a = vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Null,
                ];
                let mut d = dict(&[(entry_name, Object::Array(a))]);
                let got = validate_rect(
                    &mut x,
                    &mut d,
                    "testDict",
                    entry_name,
                    REQUIRED,
                    Version::V10,
                )
                .expect("valid");
                let zeros = vec![Object::Integer(0); 4];
                assert_eq!(format!("{:?}", got.expect("array")), format!("{zeros:?}"));
                assert_eq!(
                    format!("{:?}", d.find(entry_name.as_bytes())),
                    format!("{:?}", Some(Object::Array(zeros)))
                );

                // An indirect array: the repair is stored in the table.
                let a = vec![
                    Object::Null,
                    Object::Integer(2),
                    Object::Float(3.5),
                    Object::Integer(4),
                ];
                let mut x = table(version, vec![(9, Object::Array(a))]);
                let d = dict(&[(entry_name, r(9))]);
                validate_rectangle_entry(
                    &mut x,
                    &d,
                    "testDict",
                    entry_name,
                    REQUIRED,
                    Version::V10,
                    None,
                )
                .expect("valid");
                let want = vec![
                    Object::Integer(0),
                    Object::Integer(2),
                    Object::Float(3.5),
                    Object::Integer(4),
                ];
                match x.table.get(&9).and_then(|e| e.object.as_ref()) {
                    Some(crate::model::EntryObject::Obj(Object::Array(got))) => {
                        assert_eq!(format!("{got:?}"), format!("{want:?}"));
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    // TestRectangleRejectsInvalidShapeAndType (relaxed).
    #[test]
    fn a_malformed_rectangle_is_rejected() {
        for a in [
            vec![Object::Integer(0); 3],
            vec![Object::Integer(0); 5],
            vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(0),
                Object::name("nan"),
            ],
        ] {
            let mut x = table(Version::V14, vec![]);
            let d = dict(&[("MediaBox", Object::Array(a))]);
            assert!(
                validate_rectangle_entry(
                    &mut x,
                    &d,
                    "pageDict",
                    "MediaBox",
                    REQUIRED,
                    Version::V10,
                    None
                )
                .is_err()
            );
        }
    }

    /// `validatePageResourcesTestCase`: a pages node with one kid, in relaxed validation.
    fn page_resources_case(
        parent_resources: Option<Object>,
        page_resources: Option<Object>,
        contents: bool,
    ) -> Result<(), Error> {
        let mut page = dict(&[("Type", Object::name("Page")), ("Parent", r(2))]);
        if let Some(o) = page_resources {
            page.set(b"Resources", o);
        }
        if contents {
            page.set(b"Contents", Object::Stream(Box::default()));
        }
        let mut x = table(Version::V17, vec![(3, Object::Dict(page))]);
        let mut pages = dict(&[
            ("Type", Object::name("Pages")),
            ("Count", Object::Integer(1)),
            ("Kids", Object::Array(vec![r(3)])),
            ("MediaBox", ints(&[0, 0, 100, 100])),
        ]);
        if let Some(o) = parent_resources {
            pages.set(b"Resources", o);
        }
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let mut pc = 0;
        validate_pages_dict_depth(
            &mut v,
            &mut pages,
            2,
            &[],
            &mut pc,
            0,
            &mut PageTreeVisit::new(),
        )
    }

    // TestPageResourcesRelaxedDigestsMissingDictionary.
    #[test]
    fn relaxed_validation_digests_a_missing_resource_dictionary() {
        let empty = || Some(Object::Dict(Dict::default()));
        for (name, parent, page, contents) in [
            ("local", None, empty(), false),
            ("inherited", empty(), None, false),
            (
                "local overrides inherited null",
                Some(Object::Null),
                empty(),
                false,
            ),
            ("missing", None, None, true),
            ("missing without contents", None, None, false),
            ("local null", None, Some(Object::Null), false),
            ("inherited null", Some(Object::Null), None, false),
            (
                "local null overrides inherited",
                empty(),
                Some(Object::Null),
                false,
            ),
        ] {
            assert!(
                page_resources_case(parent, page, contents).is_ok(),
                "{name}"
            );
        }
    }

    // TestPageResourcesRejectsInvalidType.
    #[test]
    fn a_resource_dictionary_of_another_type_is_rejected() {
        let local = page_resources_case(None, Some(Object::Integer(7)), false);
        assert!(local.expect_err("local").is(Sentinel::ExpectedDict));
        let inherited = page_resources_case(Some(Object::Integer(7)), None, false);
        assert!(inherited.expect_err("inherited").is(Sentinel::ExpectedDict));
    }

    #[test]
    fn a_resource_dictionary_loses_the_keys_that_are_not_allowed() {
        let res = dict(&[
            ("ProcSet", Object::Array(vec![])),
            ("Encoding", Object::Dict(Dict::default())),
            ("Bogus", Object::Integer(1)),
        ]);
        let mut x = table(Version::V17, vec![(5, Object::Dict(res.clone()))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);

        // A reference is changed in the table.
        assert!(validate_resource_dict(&mut v, &r(5)).expect("valid"));
        let stored = stored_dict(v.x, 5);
        assert!(stored.find(b"Bogus").is_none());
        assert!(stored.find(b"ProcSet").is_some() && stored.find(b"Encoding").is_some());
        // The traversal is gone once the outermost call returns.
        assert!(v.st.resources.is_none());
        assert_eq!(v.st.resource_depth, -1);

        // A direct dictionary comes back repaired.
        let (has, repaired) =
            validate_resource_dict_repaired(&mut v, &Object::Dict(res)).expect("valid");
        assert!(has);
        let repaired = repaired.expect("repaired");
        assert!(repaired.find(b"Bogus").is_none());

        // A null resource dictionary is no dictionary.
        assert!(!validate_resource_dict(&mut v, &Object::Null).expect("valid"));
    }

    // pdf20/SimplePDF2.0.pdf truncated (parity t3): the font of a direct Font dictionary is
    // missing, so the repaired resource dictionary refers to null (Go: fixFontObjNr).
    #[test]
    fn a_missing_font_of_a_direct_font_dictionary_becomes_null() {
        let mut x = table(Version::V17, vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let res = dict(&[("Font", Object::Dict(dict(&[("F1", r(7))])))]);
        let (has, repaired) =
            validate_resource_dict_repaired(&mut v, &Object::Dict(res)).expect("valid");
        assert!(has);
        let repaired = repaired.expect("repaired");
        let Some(Object::Dict(font)) = repaired.find(b"Font") else {
            panic!("Font dict");
        };
        assert!(matches!(font.find(b"F1"), Some(Object::Null)));
    }

    #[test]
    fn a_resource_dictionary_that_refers_to_itself_is_fine() {
        let mut x = table(Version::V17, vec![(5, Object::Dict(Dict::default()))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        v.st.resources = Some(ResourceTraversal::default());
        if let Some(t) = v.st.resources.as_mut() {
            t.active.insert(5);
        }
        assert!(validate_resource_dict(&mut v, &r(5)).expect("valid"));
    }

    #[test]
    fn empty_contents_are_removed() {
        for contents in [
            Object::Array(vec![]),
            Object::string("  \n"),
            Object::Dict(Dict::default()),
        ] {
            let mut x = table(Version::V17, vec![]);
            let mut d = dict(&[("Contents", contents)]);
            validate_page_contents(&mut x, &mut d).expect("valid");
            assert!(d.find(b"Contents").is_none());
        }
        // Content that is there stays, and a non empty string or dictionary is an error.
        let mut x = table(Version::V17, vec![(4, Object::Stream(Box::default()))]);
        let mut d = dict(&[("Contents", Object::Array(vec![r(4)]))]);
        validate_page_contents(&mut x, &mut d).expect("valid");
        assert!(d.find(b"Contents").is_some());
        for bad in [
            Object::string("x"),
            Object::Dict(dict(&[("A", Object::Null)])),
            Object::Integer(1),
        ] {
            let mut d = dict(&[("Contents", bad)]);
            assert!(validate_page_contents(&mut x, &mut d).is_err());
        }
    }

    #[test]
    fn the_transition_style_replace_becomes_r() {
        let mut x = table(Version::V17, vec![]);
        let mut d = dict(&[("S", Object::name("Replace")), ("D", Object::Integer(2))]);
        validate_transition_dict(&mut x, &mut d).expect("valid");
        assert_eq!(d.name_entry(b"S"), Some(b"R".as_slice()));

        // Dm needs the style Split or Blinds, B needs Fly.
        let mut d = dict(&[("S", Object::name("Wipe")), ("Dm", Object::name("H"))]);
        assert!(validate_transition_dict(&mut x, &mut d).is_err());
        let mut d = dict(&[("S", Object::name("Split")), ("Dm", Object::name("H"))]);
        assert!(validate_transition_dict(&mut x, &mut d).is_ok());
        let mut d = dict(&[("S", Object::name("Fly")), ("B", Object::Bool(true))]);
        assert!(validate_transition_dict(&mut x, &mut d).is_ok());
        let mut d = dict(&[("S", Object::name("Split")), ("B", Object::Bool(true))]);
        assert!(validate_transition_dict(&mut x, &mut d).is_err());

        // Di: a number from the list, or None.
        for (di, ok) in [
            (Object::Integer(90), true),
            (Object::Integer(91), false),
            (Object::Float(315.0), true),
            (Object::name("None"), true),
            (Object::name("Other"), false),
            (Object::Bool(true), false),
        ] {
            let mut d = dict(&[("Di", di)]);
            assert_eq!(validate_transition_dict(&mut x, &mut d).is_ok(), ok);
        }
    }

    #[test]
    fn a_direct_trans_dictionary_is_repaired_in_the_page() {
        let mut x = table(Version::V17, vec![]);
        let mut d = dict(&[(
            "Trans",
            Object::Dict(dict(&[("S", Object::name("Replace"))])),
        )]);
        validate_page_entry_trans(&mut x, &mut d, OPTIONAL, Version::V11).expect("valid");
        assert_eq!(
            d.dict_entry(b"Trans").and_then(|t| t.name_entry(b"S")),
            Some(b"R".as_slice())
        );

        // A referenced one is repaired in the table.
        let mut x = table(
            Version::V17,
            vec![(7, Object::Dict(dict(&[("S", Object::name("Replace"))])))],
        );
        let mut d = dict(&[("Trans", r(7))]);
        validate_page_entry_trans(&mut x, &mut d, OPTIONAL, Version::V11).expect("valid");
        assert_eq!(stored_dict(&x, 7).name_entry(b"S"), Some(b"R".as_slice()));
    }

    #[test]
    fn measure_dicts() {
        let mut x = table(Version::V17, vec![]);
        let nf = |u: &str| {
            Object::Dict(dict(&[
                ("U", Object::string(u)),
                ("C", Object::Integer(1)),
                ("F", Object::name("D")),
                ("D", Object::Integer(100)),
            ]))
        };
        let rl = dict(&[
            ("R", Object::string("1 in = 1 mile")),
            ("X", Object::Array(vec![nf("mi")])),
            ("D", Object::Array(vec![nf("mi")])),
            ("A", Object::Array(vec![nf("sq mi")])),
        ]);
        assert!(validate_measure_dict(&mut x, &rl, Version::V17).is_ok());

        // An empty number format array and a precision that is no multiple of 10.
        let mut bad = rl.clone();
        bad.set(b"X", Object::Array(vec![]));
        assert!(validate_measure_dict(&mut x, &bad, Version::V17).is_err());
        let mut bad = rl.clone();
        bad.set(
            b"X",
            Object::Array(vec![Object::Dict(dict(&[
                ("U", Object::string("mi")),
                ("C", Object::Integer(1)),
                ("D", Object::Integer(15)),
            ]))]),
        );
        assert!(validate_measure_dict(&mut x, &bad, Version::V17).is_err());

        // Subtypes other than RL and GEO are not looked at.
        let other = dict(&[("Subtype", Object::name("XYZ"))]);
        assert!(validate_measure_dict(&mut x, &other, Version::V17).is_ok());

        let gcs = Object::Dict(dict(&[
            ("Type", Object::name("GEOGCS")),
            ("EPSG", Object::Integer(4326)),
        ]));
        let geo = dict(&[
            ("Subtype", Object::name("GEO")),
            ("GCS", gcs),
            ("GPTS", ints(&[0, 0, 1, 0])),
            (
                "PDU",
                Object::Array(vec![
                    Object::name("KM"),
                    Object::name("SQKM"),
                    Object::name("DEG"),
                ]),
            ),
        ]);
        assert!(validate_measure_dict(&mut x, &geo, Version::V17).is_ok());

        let mut bad = geo.clone();
        bad.set(b"LPTS", ints(&[0, 0, 1, 0, 1, 1]));
        assert!(validate_measure_dict(&mut x, &bad, Version::V17).is_err());
        let mut bad = geo.clone();
        bad.set(
            b"PDU",
            Object::Array(vec![
                Object::name("KM"),
                Object::name("KM"),
                Object::name("DEG"),
            ]),
        );
        assert!(validate_measure_dict(&mut x, &bad, Version::V17).is_err());
        let mut bad = geo;
        bad.set(b"GPTS", ints(&[0, 0, 1]));
        assert!(validate_measure_dict(&mut x, &bad, Version::V17).is_err());
    }

    /// A catalog, a root `Pages` node 2 with the kids `kids`, and the given objects.
    fn tree(kids: Vec<Object>, count: i64, objects: Vec<(i64, Object)>) -> (XRefTable, Dict) {
        let pages = dict(&[
            ("Type", Object::name("Pages")),
            ("Count", Object::Integer(count)),
            ("Kids", Object::Array(kids)),
            ("MediaBox", ints(&[0, 0, 200, 100])),
            ("Resources", Object::Dict(Dict::default())),
        ]);
        let mut all = vec![(2, Object::Dict(pages))];
        all.extend(objects);
        let mut x = table(Version::V17, all);
        // New objects go to the end of the table.
        x.table.insert(0, Entry::free_head());
        x.size = Some(10);
        (
            x,
            dict(&[("Type", Object::name("Catalog")), ("Pages", r(2))]),
        )
    }

    fn page(parent: i64) -> Object {
        Object::Dict(dict(&[
            ("Type", Object::name("Page")),
            ("Parent", r(parent)),
        ]))
    }

    #[test]
    fn the_page_tree_is_walked_and_its_count_repaired() {
        let (mut x, mut root) = tree(
            vec![r(3), Object::Null, r(0), r(4)],
            7,
            vec![(3, page(2)), (4, page(2))],
        );
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let page_root = validate_pages(&mut v, &mut root).expect("valid");

        // The count is the number of pages found, the nodes' kids lose the null and zero entries.
        assert_eq!(page_root.int_entry(b"Count"), Some(2));
        assert_eq!(v.x.page_count, 2);
        assert_eq!(v.x.cur_page, 2);
        assert_eq!(page_root.array_entry(b"Kids").map(Vec::len), Some(2));
        let stored = stored_dict(v.x, 2);
        assert_eq!(stored.int_entry(b"Count"), Some(2));
        assert_eq!(stored.array_entry(b"Kids").map(Vec::len), Some(2));
        // The pages are marked valid and known to the rendition index.
        assert!(
            v.x.is_valid(Ref {
                num: 3,
                generation: 0
            })
            .expect("entry")
        );
        assert!(v.st.rendition_pages.pages.contains(&3));
        assert!(v.st.rendition_pages.pages.contains(&4));
    }

    #[test]
    fn a_page_without_a_parent_is_an_error() {
        let orphan = Object::Dict(dict(&[("Type", Object::name("Page"))]));
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, orphan)]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_err());

        // A parent that is another node is digested.
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, page(9))]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_ok());
    }

    #[test]
    fn a_kid_that_is_no_reference_or_has_another_type_is_an_error() {
        let cancel = AtomicBool::new(false);
        let (mut x, mut root) = tree(vec![Object::Integer(3)], 1, vec![]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_err());

        let other = Object::Dict(dict(&[("Type", Object::name("Foo")), ("Parent", r(2))]));
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, other)]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_err());

        let untyped = Object::Dict(dict(&[("Parent", r(2))]));
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, untyped)]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_err());
    }

    #[test]
    fn a_page_tree_cycle_and_a_shared_node_are_errors() {
        let cancel = AtomicBool::new(false);
        // Node 3 is a Pages node with itself as a kid.
        let looping = Object::Dict(dict(&[
            ("Type", Object::name("Pages")),
            ("Parent", r(2)),
            ("Count", Object::Integer(0)),
            ("Kids", Object::Array(vec![r(3)])),
        ]));
        let (mut x, mut root) = tree(vec![r(3)], 0, vec![(3, looping)]);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_pages(&mut v, &mut root).expect_err("cycle");
        assert!(err.is(Sentinel::PageTreeCycle));

        // The same Pages node under two parents.
        let node = Object::Dict(dict(&[
            ("Type", Object::name("Pages")),
            ("Parent", r(2)),
            ("Count", Object::Integer(0)),
            ("Kids", Object::Array(vec![])),
        ]));
        let (mut x, mut root) = tree(vec![r(3), r(3)], 0, vec![(3, node)]);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_pages(&mut v, &mut root).expect_err("duplicate");
        assert!(err.is(Sentinel::PageTreeDuplicate));
    }

    #[test]
    fn a_corrupt_kid_is_replaced_with_a_blank_page() {
        let cancel = AtomicBool::new(false);
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, Object::Dict(Dict::default()))]);
        let mut v = Validator::new(&mut x, &cancel);
        validate_pages(&mut v, &mut root).expect("repaired");
        let blank = stored_dict(v.x, 3);
        assert_eq!(blank.name_entry(b"Type"), Some(b"Page".as_slice()));
        assert_eq!(blank.indirect_ref_entry(b"Parent").map(|p| p.num), Some(2));
        // The blank page has the media box of its parent node.
        let media_box = blank.array_entry(b"MediaBox").cloned().expect("MediaBox");
        assert_eq!(media_box.len(), 4);
        assert!(matches!(media_box[2], Object::Float(f) if f == 200.0));
        let contents = blank.indirect_ref_entry(b"Contents").expect("Contents");
        // Go's content stream has nil content and the 8 byte Flate encoding of nothing.
        match v.x.table.get(&contents.num).and_then(|e| e.object.as_ref()) {
            Some(crate::model::EntryObject::Obj(Object::Stream(sd))) => {
                assert!(sd.content.is_none());
                assert_eq!(sd.raw.as_ref().map(|r| r.len()), Some(8));
            }
            _ => panic!("no content stream"),
        }
    }

    #[test]
    fn a_pages_entry_that_is_no_reference_is_moved_into_an_object() {
        let cancel = AtomicBool::new(false);
        let (mut x, _) = tree(vec![r(3)], 1, vec![(3, page(2))]);
        let direct = Object::Dict(dict(&[
            ("Type", Object::name("Pages")),
            ("Count", Object::Integer(1)),
            ("Kids", Object::Array(vec![r(3)])),
            ("MediaBox", ints(&[0, 0, 100, 100])),
            ("Resources", Object::Dict(Dict::default())),
        ]));
        let mut root = dict(&[("Type", Object::name("Catalog")), ("Pages", direct)]);
        let mut v = Validator::new(&mut x, &cancel);
        let page_root = validate_pages(&mut v, &mut root).expect("repaired");
        assert_eq!(page_root.int_entry(b"Count"), Some(1));

        let new_ref = root.indirect_ref_entry(b"Pages").expect("reference");
        assert_eq!(new_ref.num, 10);
        // The kid points to the new object.
        assert_eq!(
            stored_dict(v.x, 3)
                .indirect_ref_entry(b"Parent")
                .map(|p| p.num),
            Some(10)
        );
        assert_eq!(stored_dict(v.x, 10).int_entry(b"Count"), Some(1));
    }

    #[test]
    fn the_root_needs_pages_and_count() {
        let cancel = AtomicBool::new(false);
        let mut x = table(Version::V17, vec![]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut Dict::default()).is_err());

        let (mut x, mut root) = tree(vec![], 0, vec![]);
        if let Some(Object::Dict(d)) = x.table.get_mut(&2).and_then(|e| match &mut e.object {
            Some(crate::model::EntryObject::Obj(o)) => Some(o),
            _ => None,
        }) {
            d.delete(b"Count");
        }
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_err());

        // A root with an empty Kids array has nothing to validate.
        let (mut x, mut root) = tree(vec![], 0, vec![]);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pages(&mut v, &mut root).is_ok());
    }

    #[test]
    fn a_cancelled_pass_stops() {
        let (mut x, mut root) = tree(vec![r(3)], 1, vec![(3, page(2))]);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(matches!(
            validate_pages(&mut v, &mut root),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn the_inferred_subtype_of_a_thumbnail_is_stored() {
        let thumb = dict(&[
            ("Width", Object::Integer(2)),
            ("Height", Object::Integer(2)),
            ("ColorSpace", Object::name("DeviceRGB")),
            ("BitsPerComponent", Object::Integer(16)),
        ]);
        let mut x = table(
            Version::V17,
            vec![(
                7,
                Object::Stream(Box::new(crate::object::StreamDict::new(
                    thumb,
                    0,
                    None,
                    None,
                    vec![],
                ))),
            )],
        );
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_page_entry_thumb(&mut v, &dict(&[("Thumb", r(7))]), OPTIONAL, Version::V10)
            .expect("valid");
        let Some(crate::model::EntryObject::Obj(Object::Stream(sd))) =
            x.table.get(&7).and_then(|e| e.object.as_ref())
        else {
            panic!("not a stream");
        };
        assert_eq!(sd.dict.name_entry(b"Subtype"), Some(b"Image".as_slice()));
    }

    #[test]
    fn a_thumbnail_validated_first_as_an_xobject_keeps_the_inferred_subtype() {
        // Go's validateStreamDictEntry skips a stream that is valid already, so /Thumb adds
        // nothing; the XObject path that validated it first stored the repair.
        let thumb = dict(&[
            ("Width", Object::Integer(2)),
            ("Height", Object::Integer(2)),
            ("ColorSpace", Object::name("DeviceRGB")),
            ("BitsPerComponent", Object::Integer(8)),
        ]);
        let mut x = table(
            Version::V17,
            vec![(
                7,
                Object::Stream(Box::new(crate::object::StreamDict::new(
                    thumb,
                    0,
                    None,
                    None,
                    vec![],
                ))),
            )],
        );
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        xobject::validate_xobject_stream_dict(&mut v, &r(7)).expect("valid xobject");
        validate_page_entry_thumb(&mut v, &dict(&[("Thumb", r(7))]), OPTIONAL, Version::V10)
            .expect("valid thumb");
        let Some(crate::model::EntryObject::Obj(Object::Stream(sd))) =
            x.table.get(&7).and_then(|e| e.object.as_ref())
        else {
            panic!("not a stream");
        };
        assert_eq!(sd.dict.name_entry(b"Subtype"), Some(b"Image".as_slice()));
    }
}
