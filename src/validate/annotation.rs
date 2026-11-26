//! Annotations (Go: validate/annotation.go).
//!
//! Differences from Go, besides those listed in `validate/mod.rs`:
//!
//! * Go's validators change the stored annotation dictionary through the map they are given
//!   (`d["Contents"]`, `d["DA"]` and `d.Delete("P")` repairs). Here a validator that repairs
//!   takes `&mut Dict`, which is a copy, and whoever took the copy stores it back: the page
//!   walk writes back the annotation dictionaries and the `Annots` entry of the page nodes
//!   (reached through a reference), and an entry that is a dictionary (the popup, the reply
//!   target) is written back to its owner. A page node that is a direct dictionary in `Kids`
//!   keeps its old `Annots` entry.
//! * `ownerObjNr` is kept only where it decides something: the owner of the reply chain
//!   (`validate_irt_entry`), which is the key of the cycle check. Everywhere else it only labelled
//!   errors, and errors carry the object number of the page annotation in their context instead.
//! * The link collection (`collectFileSpecificationTarget`, `ValidateLinks` is off), the
//!   merge-only `Dests` registration of `validateActionOrDestination`, the notices and the
//!   media validation cache (`validateMediaObject`, a success cache whose only effect is to skip
//!   a repeat) are not ported.
//! * `validateAnnotationType` returns nothing: its flag only selects a notice.
//!
//! The functions of other packages that this file calls (the signatures WP 16 assumed, following
//! the stub conventions): `page::validate_measure_dict`, `action::validate_sound_dict_entry`,
//! `action::validate_movie_activation_dict`, `action::validate_uri_action_dict`,
//! `action::validate_additional_actions_with_owner`, `xobject::validate_xobject_type`,
//! `xobject::validate_image_stream_dict`, `file_spec::validate_file_spec_entry`,
//! `optional_content::validate_optional_content`, `form::validate_da_relaxed`,
//! `form::validate_appearance_dict`, `form::cache_sig` and
//! `three_d::validate_annotation_dict_3d`.

use super::object::{
    OPTIONAL, REQUIRED, dict_entry_context, validate_array_array_entry, validate_array_entry,
    validate_boolean_entry, validate_color_array_entry, validate_date_entry, validate_dict_entry,
    validate_entry, validate_ind_ref_entry, validate_integer_array_entry, validate_integer_entry,
    validate_name_array_entry, validate_name_entry, validate_name_or_string_entry,
    validate_number_array, validate_number_array_entry, validate_number_entry,
    validate_rectangle_entry, validate_string_entry, validate_string_or_stream_entry,
    validate_unit_interval_array_entry,
};
use super::{
    Validator, action, destination, file_spec, form, name_tree, optional_content, page, three_d,
    trapnet, xobject,
};
use crate::annotation::{self as renderer, PgAnnots};
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::array::remove_nulls;
use crate::object::text::hex_literal_to_string;
use crate::object::{Array, Dict, Object, Ref, dict_copy_bytes};
use std::collections::BTreeSet;

/// What one entry of a page's annotation array keeps: its slot in the cleaned array and in
/// `ind_refs`, and its record in the map, each `Vec` up to twice its count.
const ANNOT_RECORD_BYTES: u64 = 2
    * (size_of::<Object>()
        + size_of::<Ref>()
        + size_of::<i64>()
        + size_of::<renderer::Annotation>()) as u64;

/// 0xCDCDCDCD interpreted as a signed 32-bit integer.
const UNINITIALIZED_ANNOTATION_BORDER_RADIUS: i64 = -842_150_451;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `validationObjectNumber`: the number of the reference `o`, else `owner_obj_nr`.
fn object_number(owner_obj_nr: i64, o: Option<&Object>) -> i64 {
    match o {
        Some(Object::Ref(r)) => r.num,
        _ => owner_obj_nr,
    }
}

fn invalid_dict_entry(dict_name: &str, entry_name: &str) -> Error {
    Error::Validation(format!(
        "dict={dict_name} entry={entry_name} invalid dict entry"
    ))
}

/// Stores `d1` back where the entry `key` of `d` found it: in the object a reference points to,
/// or in `d` itself if the entry is a direct dictionary.
fn store_entry_dict(x: &mut XRefTable, d: &mut Dict, key: &[u8], d1: Dict) -> Result<(), Error> {
    let target = match d.find(key) {
        Some(Object::Ref(r)) => Some(*r),
        _ => None,
    };
    if let Some(r) = target {
        if let Some(stored) = x.dereference_dict_mut(r)? {
            *stored = d1;
        }
    } else if let Some(slot) = d.find_mut(key)
        && matches!(slot, Object::Dict(_))
    {
        *slot = Object::Dict(d1);
    }
    Ok(())
}

/// `validateRectangleEntry` with the repair of a direct array stored back into `d` (the
/// function itself repairs an array that is a reference).
fn validate_rect_entry_repair(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let a = validate_rectangle_entry(x, d, dict_name, entry_name, required, since, None)?;
    if let Some(a) = a
        && let Some(slot) = d.find_mut(entry_name.as_bytes())
        && matches!(slot, Object::Array(_))
    {
        *slot = Object::Array(a);
    }
    Ok(())
}

fn validate_border_effect_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    // see 12.5.4
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(());
    };

    let dict_name = "borderEffectDict";

    // S, optional, name, S or C
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"S" | b"C")),
    )?;

    // I, optional, number in the range 0 to 2 (0 to 3 if relaxed)
    validate_number_entry(
        x,
        &d1,
        dict_name,
        "I",
        OPTIONAL,
        Version::V10,
        Some(&|f: f64| (0.0..=3.0).contains(&f)),
    )?;

    Ok(())
}

fn validate_border_style_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    // see 12.5.4
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(());
    };

    let dict_name = "borderStyleDict";

    // Type, optional, name, "Border"
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Border"),
    )?;

    // W, optional, number, border width in points
    validate_number_entry(x, &d1, dict_name, "W", OPTIONAL, Version::V10, None)?;

    // S, optional, name, border style
    if let Err(e) = validate_name_entry(
        x,
        &d1,
        dict_name,
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"S" | b"D" | b"B" | b"I" | b"U" | b"A")),
    ) && !e.to_string().contains("invalid dict entry")
    {
        return Err(e);
    }
    // The PDF spec mandates interpreting undefined values as "S".

    // D, optional, dash array
    validate_number_array_entry(x, &d1, dict_name, "D", OPTIONAL, Version::V10, None)?;

    Ok(())
}

fn validate_icon_fit_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    // see table 247
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(());
    };

    let dict_name = "iconFitDict";

    // SW, optional, name, A,B,S,N
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "SW",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"A" | b"B" | b"S" | b"N")),
    )?;

    // S, optional, name, A,P
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"A" | b"P")),
    )?;

    // A, optional, array of 2 numbers between 0.0 and 1.0
    validate_unit_interval_array_entry(x, &d1, dict_name, "A", Version::V10, &[2])?;

    // FB, optional, bool, since V1.5
    validate_boolean_entry(x, &d1, dict_name, "FB", OPTIONAL, Version::V10, None)?;

    Ok(())
}

fn validate_appearance_characteristics_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    // see 12.5.6.19
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(());
    };

    let dict_name = "appCharDict";

    // R, optional, integer
    validate_integer_entry(x, &d1, dict_name, "R", OPTIONAL, Version::V10, None)?;

    // BC, optional, array of numbers, len=0,1,3,4
    validate_color_array_entry(x, &d1, dict_name, "BC", Version::V10)?;

    // BG, optional, array of numbers between 0.0 and 1.0, len=0,1,3,4
    validate_color_array_entry(x, &d1, dict_name, "BG", Version::V10)?;

    // CA, RC, AC, optional, text strings
    for key in ["CA", "RC", "AC"] {
        validate_string_entry(x, &d1, dict_name, key, OPTIONAL, Version::V10, None)?;
    }

    // I, RI, IX, optional, stream dicts
    for key in ["I", "RI", "IX"] {
        validate_stream_dict_entry_ignored(x, &d1, dict_name, key)?;
    }

    // IF, optional, icon fit dict,
    validate_icon_fit_dict_entry(x, &d1, dict_name, "IF", OPTIONAL, Version::V10)?;

    // TP, optional, integer 0..6
    validate_integer_entry(
        x,
        &d1,
        dict_name,
        "TP",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| (0..=6).contains(&i)),
    )?;

    Ok(())
}

/// `validateStreamDictEntry(.., OPTIONAL, V10, nil)` for a caller that ignores the stream.
fn validate_stream_dict_entry_ignored(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    super::object::validate_stream_dict_entry(
        x,
        d,
        dict_name,
        entry_name,
        OPTIONAL,
        Version::V10,
        None,
    )?;
    Ok(())
}

fn validate_text_annotation_state(
    dict_name: &str,
    state: Option<&str>,
    state_model: Option<&str>,
) -> Result<(), Error> {
    let Some(state) = state else {
        if let Some(sm) = state_model {
            return Err(Error::Validation(format!(
                "dict={dict_name} missing State for StateModel={sm}"
            )));
        }
        return Ok(());
    };
    let Some(state_model) = state_model else {
        return Err(Error::Panic("nil StateModel"));
    };

    if !matches!(state_model, "Marked" | "Review") {
        let msg = format!("dict={dict_name} entry=StateModel invalid dict entry: {state_model}");
        if state_model.is_empty() {
            return Err(Error::Validation(msg));
        }
        if state.is_empty() {
            return Err(Error::Validation(format!(
                "dict={dict_name} invalid State={state} for StateModel={state_model}"
            )));
        }
        return Ok(());
    }

    // Ensure that the state/model combo is valid.
    let valid = if state_model == "Marked" {
        matches!(state, "Marked" | "Unmarked")
    } else {
        matches!(
            state,
            "Accepted" | "Rejected" | "Cancelled" | "Completed" | "None"
        )
    };
    if !valid {
        return Err(Error::Validation(format!(
            "dict={dict_name} invalid State={state} for StateModel={state_model}"
        )));
    }

    Ok(())
}

fn validate_annotation_dict_text(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.4

    // Open, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Open", OPTIONAL, Version::V10, None)?;

    // Name, optional, name
    validate_name_entry(x, d, dict_name, "Name", OPTIONAL, Version::V10, None)?;

    // State, optional, text string, since V1.5 (1.4 if relaxed)
    let state = validate_string_entry(x, d, dict_name, "State", OPTIONAL, Version::V14, None)?;

    // StateModel, text string, since V1.5 (1.4 if relaxed)
    let state_model = validate_string_entry(
        x,
        d,
        dict_name,
        "StateModel",
        state.is_some(),
        Version::V14,
        None,
    )?;

    validate_text_annotation_state(dict_name, state.as_deref(), state_model.as_deref())
}

/// `validateActionOrDestination` for a link annotation: the `A` entry, else the `Dest` entry.
/// (Go also registers the destination name with the merge command, which is not ported.)
fn validate_action_or_destination(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // The action that shall be performed when this item is activated.
    let d1 = validate_dict_entry(v.x, d, dict_name, "A", OPTIONAL, Version::V11, None)?;
    if let Some(d1) = d1 {
        return action::validate_action_dict_object(
            v,
            &d1,
            d.find(b"A"),
            &format!("{dict_name}.A"),
        );
    }

    // A destination that shall be displayed when this item is activated.
    let raw_dest = d.find(b"Dest").cloned();
    let obj = validate_entry(v.x, d, dict_name, "Dest", OPTIONAL, Version::V10)?;
    if obj.is_none() {
        return Ok(());
    }

    destination::validate_destination(v.x, &raw_dest.unwrap_or(Object::Null), false)?;
    Ok(())
}

fn validate_uri_action_dict_entry(
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

    let dict_name = "URIActionDict";

    // Type, optional, name
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Action"),
    )?;

    // S, required, name, action Type
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "S",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"URI"),
    )?;

    action::validate_uri_action_dict(x, &d1, dict_name)
}

fn validate_annotation_dict_link(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.5

    // A or Dest, required either or. Relaxed validation digests an unresolved destination.
    let _ = validate_action_or_destination(v, d, dict_name);

    // H, optional, name, since V1.2
    validate_name_entry(v.x, d, dict_name, "H", OPTIONAL, Version::V12, None)?;

    // PA, optional, URI action dict, since V1.3
    validate_uri_action_dict_entry(v.x, d, dict_name, "PA", OPTIONAL, Version::V13)?;

    // QuadPoints, optional, number array, len= a multiple of 8, since V1.6 (1.3 if relaxed)
    validate_number_array_entry(
        v.x,
        d,
        dict_name,
        "QuadPoints",
        OPTIONAL,
        Version::V13,
        Some(&|a: &[Object]| a.len().is_multiple_of(8)),
    )?;

    // BS, optional, border style dict, since V1.6 (1.2 if relaxed)
    validate_border_style_dict(v.x, d, dict_name, "BS", OPTIONAL, Version::V12)
}

fn validate_free_text_da_name(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<bool, Error> {
    let Some(o) = d.find(b"DA") else {
        return Ok(false);
    };

    let n = match x.dereference(o)? {
        Object::Name(n) => n.clone(),
        _ => return Ok(false),
    };

    x.validate_version(&format!("dict={dict_name} entry=DA"), Version::V10)?;

    let s = lossy(&n);
    if !s.split_whitespace().any(|f| f == "Tf") || !form::validate_da_relaxed(&s) {
        return Err(Error::Validation(format!(
            "dict={dict_name} entry=DA invalid type types.Name"
        )));
    }

    Ok(true)
}

fn validate_ap_and_da(x: &mut XRefTable, d: &mut Dict, dict_name: &str) -> Result<bool, Error> {
    let mut required = REQUIRED;

    // An existing AP entry takes precedence over a DA entry.
    let d1 = validate_dict_entry(x, d, dict_name, "AP", OPTIONAL, Version::V12, None)?;
    if d1.is_some_and(|d1| !d1.is_empty()) {
        required = OPTIONAL;
    }

    let da_name = validate_free_text_da_name(x, d, dict_name)?;
    if da_name {
        return Ok(true);
    }

    // DA, required, string
    let da = validate_string_entry(
        x,
        d,
        dict_name,
        "DA",
        required,
        Version::V10,
        Some(&form::validate_da_relaxed),
    )?;
    if let Some(da) = da {
        // Repair
        d.set(b"DA", Object::String(da.into_bytes()));
    }

    Ok(false)
}

fn validate_annotation_dict_free_text_part1(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // Q, optional, integer, since V1.4 (1.3 if relaxed), 0,1,2
    validate_integer_entry(
        x,
        d,
        dict_name,
        "Q",
        OPTIONAL,
        Version::V13,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // RC, optional, text string or text stream, since V1.5 (1.3 if relaxed)
    validate_string_or_stream_entry(x, d, dict_name, "RC", OPTIONAL, Version::V13)?;

    // DS, optional, text string, since V1.5 (1.3 if relaxed)
    validate_string_entry(x, d, dict_name, "DS", OPTIONAL, Version::V13, None)?;

    // CL, optional, number array, since V1.6 (1.3 if relaxed), no length check if relaxed
    validate_number_array_entry(x, d, dict_name, "CL", OPTIONAL, Version::V13, None)?;

    Ok(())
}

fn validate_annotation_dict_free_text_part2(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // IT, optional, name, since V1.6 (1.3 if relaxed)
    validate_name_entry(
        x,
        d,
        dict_name,
        "IT",
        OPTIONAL,
        Version::V13,
        Some(&|s: &[u8]| {
            matches!(
                s,
                b"FreeText" | b"FreeTextCallout" | b"FreeTextTypeWriter" | b"FreeTextTypewriter"
            )
        }),
    )?;

    // BE, optional, border effect dict, since V1.6 (1.4 if relaxed)
    validate_border_effect_dict_entry(x, d, dict_name, "BE", OPTIONAL, Version::V14)?;

    // RD, optional, rectangle, since V1.6 (1.4 if relaxed)
    validate_rect_entry_repair(x, d, dict_name, "RD", OPTIONAL, Version::V14)?;

    // BS, optional, border style dict, since V1.6 (1.2 if relaxed)
    validate_border_style_dict(x, d, dict_name, "BS", OPTIONAL, Version::V12)?;

    // LE, optional, name, since V1.6 (1.4 if relaxed)
    validate_name_entry(x, d, dict_name, "LE", OPTIONAL, Version::V14, None)?;

    Ok(())
}

fn validate_annotation_dict_free_text(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.6
    validate_ap_and_da(x, d, dict_name)?;
    validate_annotation_dict_free_text_part1(x, d, dict_name)?;
    validate_annotation_dict_free_text_part2(x, d, dict_name)
}

fn validate_entry_measure(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let d1 = validate_dict_entry(x, d, dict_name, "Measure", required, since, None)?;
    if let Some(d1) = d1 {
        page::validate_measure_dict(x, &d1, since)?;
    }
    Ok(())
}

fn validate_annotation_dict_line_part1(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // L, required, array of numbers, len:4
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "L",
        REQUIRED,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 4),
    )?;

    // BS, optional, border style dict
    validate_border_style_dict(x, d, dict_name, "BS", OPTIONAL, Version::V10)?;

    // LE, optional, name array, since V1.4 (1.3 if relaxed), len:2
    validate_name_array_entry(
        x,
        d,
        dict_name,
        "LE",
        OPTIONAL,
        Version::V13,
        Some(&|a: &[Object]| a.len() == 2),
    )?;

    // IC, optional, number array, since V1.4 (1.3 if relaxed), len:0,1,3,4
    validate_color_array_entry(x, d, dict_name, "IC", Version::V13)?;

    // LLE, optional, number, since V1.6 (1.4 if relaxed), >= 0 if relaxed
    let lle = validate_number_entry(
        x,
        d,
        dict_name,
        "LLE",
        OPTIONAL,
        Version::V14,
        Some(&|f: f64| f >= 0.0),
    )?;

    // LL, required if LLE present, number, since V1.6 (1.4 if relaxed)
    validate_number_entry(x, d, dict_name, "LL", lle.is_some(), Version::V14, None)?;

    // Cap, optional, bool, since V1.6 (1.4 if relaxed)
    validate_boolean_entry(x, d, dict_name, "Cap", OPTIONAL, Version::V14, None)?;

    Ok(())
}

fn validate_annotation_dict_line_part2(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // IT, optional, name, since V1.6 (1.4 if relaxed)
    validate_name_entry(x, d, dict_name, "IT", OPTIONAL, Version::V14, None)?;

    // LLO, optional, number, since V1.7, >0
    validate_number_entry(
        x,
        d,
        dict_name,
        "LLO",
        OPTIONAL,
        Version::V17,
        Some(&|f: f64| f > 0.0),
    )?;

    // CP, optional, name, since V1.7 (1.5 if relaxed)
    validate_name_entry(
        x,
        d,
        dict_name,
        "CP",
        OPTIONAL,
        Version::V15,
        Some(&|s: &[u8]| matches!(s, b"Inline" | b"Top")),
    )?;

    // Measure, optional, measure dict, since V1.7
    validate_entry_measure(x, d, dict_name, OPTIONAL, Version::V17)?;

    // CO, optional, number array, since V1.7 (1.5 if relaxed), len=2
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "CO",
        OPTIONAL,
        Version::V15,
        Some(&|a: &[Object]| a.len() == 2),
    )?;

    Ok(())
}

fn validate_annotation_dict_line(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.7
    validate_annotation_dict_line_part1(x, d, dict_name)?;
    validate_annotation_dict_line_part2(x, d, dict_name)
}

fn validate_annotation_dict_circle_or_square(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.8

    // BS, optional, border style dict
    validate_border_style_dict(x, d, dict_name, "BS", OPTIONAL, Version::V10)?;

    // IC, optional, array, since V1.4 (1.3 if relaxed)
    validate_color_array_entry(x, d, dict_name, "IC", Version::V13)?;

    // BE, optional, border effect dict, since V1.5
    validate_border_effect_dict_entry(x, d, dict_name, "BE", OPTIONAL, Version::V15)?;

    // RD, optional, rectangle, since V1.5 (1.3 if relaxed)
    validate_rect_entry_repair(x, d, dict_name, "RD", OPTIONAL, Version::V13)
}

/// `validateEntryIT` of the polygon and polyline annotations (since V1.4 if relaxed).
fn validate_entry_it(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
) -> Result<(), Error> {
    let since = Version::V14;
    let version = x.version()?;

    // IT, optional, name, since V1.6
    let validate_intent = |s: &[u8]| {
        if version == since {
            return s == b"PolygonCloud";
        }
        version == Version::V17
            && matches!(
                s,
                b"PolygonCloud" | b"PolyLineDimension" | b"PolygonDimension"
            )
    };

    validate_name_entry(
        x,
        d,
        dict_name,
        "IT",
        required,
        since,
        Some(&validate_intent),
    )?;
    Ok(())
}

fn validate_annotation_dict_poly_line(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.9

    // Vertices, required, array of numbers
    validate_number_array_entry(x, d, dict_name, "Vertices", REQUIRED, Version::V10, None)?;

    // LE, optional, array of 2 names, meaningful only for polyline annotations.
    if dict_name == "PolyLine" {
        validate_name_array_entry(
            x,
            d,
            dict_name,
            "LE",
            OPTIONAL,
            Version::V10,
            Some(&|a: &[Object]| a.len() == 2),
        )?;
    }

    // BS, optional, border style dict
    validate_border_style_dict(x, d, dict_name, "BS", OPTIONAL, Version::V10)?;

    // IC, optional, array of numbers [0.0 .. 1.0], len:1,3,4
    validate_unit_interval_array_entry(x, d, dict_name, "IC", Version::V14, &[1, 3, 4])?;

    // BE, optional, border effect dict, meaningful only for polygon annotations
    if dict_name == "Polygon" {
        validate_border_effect_dict_entry(x, d, dict_name, "BE", OPTIONAL, Version::V10)?;
    }

    validate_entry_it(x, d, dict_name, OPTIONAL)
}

fn validate_text_markup_annotation(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.10

    // QuadPoints, required (optional if relaxed), number array, len: a multiple of 8
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "QuadPoints",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len().is_multiple_of(8)),
    )?;
    Ok(())
}

fn validate_annotation_dict_stamp(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.12

    // Name, optional, name
    validate_name_entry(x, d, dict_name, "Name", OPTIONAL, Version::V10, None)?;
    Ok(())
}

fn validate_annotation_dict_caret(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.11

    // RD, optional, rectangle, since V1.5 (1.4 if relaxed)
    validate_rect_entry_repair(x, d, dict_name, "RD", OPTIONAL, Version::V14)?;

    // Sy, optional, name (S is digested if relaxed)
    validate_name_entry(
        x,
        d,
        dict_name,
        "Sy",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"P" | b"None" | b"S")),
    )?;

    Ok(())
}

fn validate_annotation_dict_ink(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // see 12.5.6.13

    // InkList, required (optional if relaxed), array of stroked path arrays
    let a = validate_array_array_entry(x, d, dict_name, "InkList", OPTIONAL, Version::V10, None)?;
    for (i, o) in a.unwrap_or_default().iter().enumerate() {
        validate_number_array(x, o).map_err(|e| e.context(format!("{dict_name}.InkList[{i}]")))?;
    }

    // BS, optional, border style dict
    validate_border_style_dict(x, d, dict_name, "BS", OPTIONAL, Version::V10)
}

fn validate_annotation_dict_popup(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.14

    // Parent, optional, dict indRef
    let ir = validate_ind_ref_entry(x, d, dict_name, "Parent", OPTIONAL, Version::V10)?;
    if let Some(ir) = ir
        && x.dereference_dict_ref(ir)?.is_none()
    {
        return Ok(());
    }

    // Open, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Open", OPTIONAL, Version::V10, None)?;
    Ok(())
}

fn validate_annotation_dict_file_attachment(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.15

    // FS, required, file specification
    file_spec::validate_file_spec_entry(x, d, dict_name, "FS", REQUIRED, Version::V10)?;

    // Name, optional, name
    validate_name_or_string_entry(x, d, dict_name, "Name", OPTIONAL, Version::V10)
}

fn validate_annotation_dict_sound(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.16

    // Sound, required, stream dict
    action::validate_sound_dict_entry(x, d, dict_name, "Sound", REQUIRED, Version::V10)?;

    // Name, optional, name
    validate_name_entry(x, d, dict_name, "Name", OPTIONAL, Version::V10, None)?;
    Ok(())
}

fn validate_movie_poster(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    const DICT_NAME: &str = "movieDict";
    let Some(o) = validate_entry(v.x, d, DICT_NAME, "Poster", OPTIONAL, Version::V10)? else {
        return Ok(());
    };
    let mut sd = match o {
        Object::Bool(_) => return Ok(()),
        Object::Stream(sd) => *sd,
        other => {
            return Err(Error::Validation(format!(
                "dict={DICT_NAME} entry=Poster invalid type {}",
                type_name(&other)
            )));
        }
    };
    xobject::validate_xobject_type(v.x, &mut sd)?;
    validate_name_entry(
        v.x,
        &sd.dict,
        "posterStreamDict",
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Image"),
    )?;
    xobject::validate_image_stream_dict(v, &sd, false)
}

fn validate_movie_aspect(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    // see 13.4
    let a = validate_integer_array_entry(
        x,
        d,
        "movieDict",
        "Aspect",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    // A negative dimension is digested if relaxed.
    for value in a.unwrap_or_default() {
        if x.dereference_integer(&value)?.is_none() {
            return Err(Error::Validation(
                "movieDict.Aspect: expected two integers".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_movie_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    // see 13.4
    let dict_name = "movieDict";

    // F, required, file specification
    file_spec::validate_file_spec_entry(v.x, d, dict_name, "F", REQUIRED, Version::V10)?;

    // Aspect, optional, integer array, length 2
    validate_movie_aspect(v.x, d)?;

    // Rotate, optional, integer multiple of 90
    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "Rotate",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i % 90 == 0),
    )?;

    // Poster, optional boolean or stream
    validate_movie_poster(v, d)
}

fn validate_annotation_dict_movie(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.17 Movie Annotations
    // 13.4 Movies
    // The features described in this sub-clause are obsolescent and their use is no longer
    // recommended. They are superseded by the general multimedia framework described in 13.2.

    // T, optional, text string
    validate_string_entry(v.x, d, dict_name, "T", OPTIONAL, Version::V10, None)?;

    // Movie, required, movie dict
    let d1 = validate_dict_entry(v.x, d, dict_name, "Movie", REQUIRED, Version::V10, None)?;
    validate_movie_dict(v, &d1.unwrap_or_default())?;

    // A, optional, boolean or movie activation dict
    if let Some(raw) = d.find(b"A") {
        let o = v.x.dereference(raw)?.clone();
        match o {
            Object::Null | Object::Bool(_) => {}
            Object::Dict(a) => action::validate_movie_activation_dict(v.x, &a)?,
            other => {
                return Err(Error::Validation(format!(
                    "dict={dict_name} entry=A invalid type {}",
                    type_name(&other)
                )));
            }
        }
    }

    Ok(())
}

/// `validateActionDictObjectWithOrigin` for the `A` entry `d1` of `d`.
fn validate_annotation_action(
    v: &mut Validator<'_>,
    d: &Dict,
    d1: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    action::validate_action_dict_object(v, d1, d.find(b"A"), &format!("{dict_name}.A"))
}

fn validate_annotation_dict_widget(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.19

    // H, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "H",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"N" | b"I" | b"O" | b"P" | b"T" | b"A")),
    )?;

    // MK, optional, dict
    // An appearance characteristics dictionary that shall be used in constructing
    // a dynamic appearance stream specifying the annotation's visual presentation on the page.
    validate_appearance_characteristics_dict_entry(
        v.x,
        d,
        dict_name,
        "MK",
        OPTIONAL,
        Version::V10,
    )?;

    // A, optional, dict, since V1.1
    // An action that shall be performed when the annotation is activated.
    let d1 = validate_dict_entry(v.x, d, dict_name, "A", OPTIONAL, Version::V11, None)?;
    if let Some(d1) = d1 {
        validate_annotation_action(v, d, &d1, dict_name)?;
    }

    // AA, optional, dict, since V1.2
    // An additional-actions dictionary defining the annotation's behaviour in response to
    // various trigger events.
    action::validate_additional_actions_with_owner(
        v,
        d,
        dict_name,
        "AA",
        OPTIONAL,
        Version::V12,
        "fieldOrAnnot",
    )?;

    // BS, optional, border style dict, since V1.2
    // A border style dictionary specifying the width and dash pattern
    // that shall be used in drawing the annotation's border.
    validate_border_style_dict(v.x, d, dict_name, "BS", OPTIONAL, Version::V12)?;

    // Parent, dict, required if one of multiple children in a field.
    // An indirect reference to the widget annotation's parent field.
    validate_ind_ref_entry(v.x, d, dict_name, "Parent", OPTIONAL, Version::V10)?;
    Ok(())
}

fn validate_annotation_dict_screen_static(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // T, optional, text string
    validate_string_entry(x, d, dict_name, "T", OPTIONAL, Version::V10, None)?;

    // MK, optional, appearance characteristics dict
    validate_appearance_characteristics_dict_entry(x, d, dict_name, "MK", OPTIONAL, Version::V10)
}

fn validate_annotation_dict_screen(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.18
    validate_annotation_dict_screen_static(v.x, d, dict_name)?;

    // A, optional, action dict, since V1.0
    let d1 = validate_dict_entry(v.x, d, dict_name, "A", OPTIONAL, Version::V10, None)?;
    if let Some(d1) = d1 {
        validate_annotation_action(v, d, &d1, dict_name)?;
    }

    // AA, optional, additional-actions dict, since V1.2
    action::validate_additional_actions_with_owner(
        v,
        d,
        dict_name,
        "AA",
        OPTIONAL,
        Version::V12,
        "fieldOrAnnot",
    )
}

/// `validateRenditionScreenAnnotationStructure`: the structure of the screen annotation a
/// rendition action refers to. Go's `ownerObjNr` is dropped (it only labels errors).
///
/// # Errors
/// [`Error::Validation`] for a subtype other than `Screen`, and the errors of the entry
/// validators.
pub fn validate_rendition_screen_annotation_structure(
    v: &mut Validator<'_>,
    d: &mut Dict,
) -> Result<(), Error> {
    const DICT_NAME: &str = "annotDict";
    validate_annotation_type(v.x, d, DICT_NAME)?;
    let subtype = validate_annotation_dict_general(v, d, DICT_NAME)?;
    if subtype != b"Screen" {
        return Err(Error::Validation(format!(
            "dict={DICT_NAME} entry=Subtype invalid value {}",
            lossy(&subtype)
        )));
    }
    v.x.validate_version("Screen annotation", Version::V14)?;
    optional_content::validate_optional_content(v.x, d, DICT_NAME, "OC", OPTIONAL, Version::V13)?;
    validate_annotation_dict_screen_static(v.x, d, DICT_NAME)
}

/// `validateAppearDictEntry`: Go repairs the appearance dictionary in place, so a repaired
/// direct `AP` is set back into `d` (a referenced one is stored back by the callee).
fn validate_appear_dict_entry(
    v: &mut Validator<'_>,
    d: &mut Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    if validate_dict_entry(v.x, d, dict_name, "AP", required, since, None)?.is_some()
        && let Some(o) = d.find(b"AP").cloned()
        && let Some(d1) = form::validate_appearance_dict(v, &o)?
    {
        d.set(b"AP", Object::Dict(d1));
    }
    Ok(())
}

fn validate_annotation_dict_printer_mark(
    v: &mut Validator<'_>,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.20

    // MN, optional, name
    validate_name_entry(v.x, d, dict_name, "MN", OPTIONAL, Version::V10, None)?;

    // F, required integer, since V1.1, annotation flags
    validate_integer_entry(v.x, d, dict_name, "F", REQUIRED, Version::V11, None)?;

    // AP, required, appearance dict, since V1.2 (1.1 if relaxed)
    validate_appear_dict_entry(v, d, dict_name, REQUIRED, Version::V11)
}

fn validate_trap_net_font_fauxing(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let Some(a) =
        validate_array_entry(x, d, dict_name, "FontFauxing", OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };

    let mut found = false;
    for (i, o) in a.iter().enumerate() {
        if o.is_null() {
            continue;
        }

        let font_dict = x
            .dereference_dict(o)
            .map_err(|e| {
                e.context(format!(
                    "{dict_name}.FontFauxing[{i}]: dereference font dict"
                ))
            })?
            .cloned();
        let Some(font_dict) = font_dict else {
            continue;
        };

        let (t, _) = x
            .dereference_name_entry(&font_dict, b"Type")
            .map_err(|e| e.context(format!("{dict_name}.FontFauxing[{i}] Type")))?;
        if t.as_deref() != Some(b"Font".as_slice()) {
            return Err(invalid_dict_entry(dict_name, "FontFauxing"));
        }
        found = true;
    }

    if !found {
        return Err(invalid_dict_entry(dict_name, "FontFauxing"));
    }

    Ok(())
}

fn validate_annotation_dict_trap_net(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.21

    // LastModified, optional, date
    validate_date_entry(v.x, d, dict_name, "LastModified", OPTIONAL, Version::V10)?;

    // Version, optional, array
    validate_array_entry(v.x, d, dict_name, "Version", OPTIONAL, Version::V10, None)?;

    // AnnotStates, optional, array of names or null placeholders.
    trapnet::validate_trap_net_state_values(v, d, dict_name)?;

    // FontFauxing, optional, font dict array
    validate_trap_net_font_fauxing(v.x, d, dict_name)?;

    validate_integer_entry(v.x, d, dict_name, "F", REQUIRED, Version::V11, None)?;
    Ok(())
}

fn validate_fixed_print_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "fixedPrintDict";

    // Type, required, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"FixedPrint"),
    )?;

    // Matrix, optional, number array, length = 6
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "Matrix",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 6),
    )?;

    // H, optional, number
    validate_number_entry(x, d, dict_name, "H", OPTIONAL, Version::V10, None)?;

    // V, optional, number
    validate_number_entry(x, d, dict_name, "V", OPTIONAL, Version::V10, None)?;
    Ok(())
}

fn validate_annotation_dict_watermark(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.22

    // FixedPrint, optional, dict
    let fixed_print =
        validate_dict_entry(x, d, dict_name, "FixedPrint", OPTIONAL, Version::V10, None)?;
    match fixed_print {
        Some(fixed_print) => validate_fixed_print_dict(x, &fixed_print),
        None => Ok(()),
    }
}

/// `validateEntryIC` of the redact annotation: an optional array of three numbers in 0.0 to 1.0.
/// Go checks it inside the array validation, before the elements are known to be numbers.
fn validate_entry_ic(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // IC, optional, number array, length:3 [0.0 .. 1.0]
    let Some(a) = validate_number_array_entry(x, d, dict_name, "IC", OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };

    let mut valid = a.len() == 3;
    if valid {
        for o in &a {
            valid = match x.dereference(o) {
                Err(_) => false,
                Ok(Object::Integer(i)) => (0..=1).contains(i),
                Ok(Object::Float(f)) => (0.0..=1.0).contains(f),
                Ok(_) => true,
            };
            if !valid {
                break;
            }
        }
    }

    if !valid {
        return Err(invalid_dict_entry(dict_name, "IC"));
    }
    Ok(())
}

fn validate_annotation_dict_redact(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 12.5.6.23

    // QuadPoints, optional, len: a multiple of 8
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "QuadPoints",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len().is_multiple_of(8)),
    )?;

    // IC, optional, number array, length:3 [0.0 .. 1.0]
    validate_entry_ic(x, d, dict_name)?;

    // RO, optional, stream
    validate_stream_dict_entry_ignored(x, d, dict_name, "RO")?;

    // OverlayText, optional, text string
    validate_string_entry(x, d, dict_name, "OverlayText", OPTIONAL, Version::V10, None)?;

    // Repeat, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Repeat", OPTIONAL, Version::V10, None)?;

    // DA, required, byte string
    let da = validate_string_entry(
        x,
        d,
        dict_name,
        "DA",
        REQUIRED,
        Version::V10,
        Some(&form::validate_da_relaxed),
    )?;
    if let Some(da) = da {
        // Repair
        d.set(b"DA", Object::String(da.into_bytes()));
    }

    // Q, optional, integer
    validate_integer_entry(x, d, dict_name, "Q", OPTIONAL, Version::V10, None)?;
    Ok(())
}

fn validate_rich_media_annotation(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // see 13.7.2.1
    // See Adobe Supplement to ISO 32000, extension level 3, 9.6.1.
    let content = validate_dict_entry(
        v.x,
        d,
        dict_name,
        "RichMediaContent",
        REQUIRED,
        Version::V10,
        None,
    )?;
    validate_rich_media_content(v, &content.unwrap_or_default())
}

fn validate_rich_media_content(v: &mut Validator<'_>, content: &Dict) -> Result<(), Error> {
    // see 13.7.2.3
    validate_name_entry(
        v.x,
        content,
        "RichMediaContent",
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"RichMediaContent"),
    )?;

    let assets = validate_dict_entry(
        v.x,
        content,
        "RichMediaContent",
        "Assets",
        OPTIONAL,
        Version::V10,
        None,
    )?;
    let Some(assets) = assets else {
        return Ok(());
    };
    name_tree::validate_name_tree(v, "RichMediaAssets", &assets, true, content.find(b"Assets"))?;
    Ok(())
}

fn validate_ex_data_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "ExData";

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"ExData"),
    )?;

    validate_name_entry(
        x,
        d,
        dict_name,
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Markup3D"),
    )?;
    Ok(())
}

fn validate_popup_entry(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    dict_name: &str,
    entry_name: &str,
    required: bool,
) -> Result<(), Error> {
    // since V1.3 (1.2 if relaxed)
    let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, required, Version::V12, None)?;
    let Some(mut d1) = d1 else {
        return Ok(());
    };

    validate_name_entry(
        v.x,
        &d1,
        dict_name,
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Popup"),
    )?;

    let popup_obj_nr = object_number(owner_obj_nr, d.find(entry_name.as_bytes()));
    let res = validate_annotation_dict(v, &mut d1, popup_obj_nr);
    store_entry_dict(v.x, d, entry_name.as_bytes(), d1)?;
    res?;

    Ok(())
}

/// `annotationIRTTraversal`: the objects of the reply chain being validated.
type IrtTraversal = BTreeSet<i64>;

/// `annotationIRTTraversal.enter`.
fn irt_enter(visit: &mut IrtTraversal, obj_nr: i64) -> Result<(), Error> {
    if obj_nr <= 0 {
        return Ok(());
    }
    if !visit.insert(obj_nr) {
        return Err(Error::Sentinel(Sentinel::AnnotationIrtCycle).context(format!("obj#{obj_nr}")));
    }
    Ok(())
}

/// `annotationIRTTraversal.leave`.
fn irt_leave(visit: &mut IrtTraversal, obj_nr: i64) {
    if obj_nr > 0 {
        visit.remove(&obj_nr);
    }
}

fn validate_irt_entry(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    depth: u64,
    visit: &mut IrtTraversal,
) -> Result<(), Error> {
    let raw_entry = d.find(entry_name.as_bytes()).cloned();
    let irt_obj_nr = object_number(owner_obj_nr, raw_entry.as_ref());
    let entry_context = || dict_entry_context(dict_name, entry_name, raw_entry.as_ref());

    v.check()?;

    let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, required, since, None)
        .map_err(|e| e.context(entry_context()))?;
    let Some(mut d1) = d1 else {
        return Ok(());
    };

    v.x.check_recursion_depth("annotation IRT chain", depth)
        .map_err(|e| e.context(entry_context()))?;
    irt_enter(visit, irt_obj_nr).map_err(|e| e.context(entry_context()))?;

    let res = validate_annotation_dict_depth(v, &mut d1, irt_obj_nr, depth + 1, visit);
    irt_leave(visit, irt_obj_nr);
    let stored = store_entry_dict(v.x, d, entry_name.as_bytes(), d1);
    res.map_err(|e| e.context(entry_context()))?;
    stored
}

fn validate_markup_annotation_part1(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    dict_name: &str,
) -> Result<(), Error> {
    // T, optional, text string, since V1.1
    validate_string_entry(v.x, d, dict_name, "T", OPTIONAL, Version::V11, None)?;

    // Popup, optional, dict, since V1.3
    validate_popup_entry(v, d, owner_obj_nr, dict_name, "Popup", OPTIONAL)?;

    // CA, optional, number, since V1.4 (1.3 if relaxed)
    validate_number_entry(v.x, d, dict_name, "CA", OPTIONAL, Version::V13, None)?;

    // RC, optional, text string or stream, since V1.5 (1.3 if relaxed)
    validate_string_or_stream_entry(v.x, d, dict_name, "RC", OPTIONAL, Version::V13)?;

    // CreationDate, optional, date, since V1.5 (1.3 if relaxed)
    validate_date_entry(v.x, d, dict_name, "CreationDate", OPTIONAL, Version::V13)?;

    Ok(())
}

fn validate_markup_annotation_part2(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    dict_name: &str,
    depth: u64,
    visit: &mut IrtTraversal,
) -> Result<(), Error> {
    // IRT, optional, (in reply to) dict, since V1.5 (1.4 if relaxed)
    validate_irt_entry(
        v,
        d,
        owner_obj_nr,
        dict_name,
        "IRT",
        OPTIONAL,
        Version::V14,
        depth,
        visit,
    )?;

    // Subj, optional, text string, since V1.5 (1.3 if relaxed)
    validate_string_entry(v.x, d, dict_name, "Subj", OPTIONAL, Version::V13, None)?;

    // RT, optional, name, since V1.6 (1.4 if relaxed)
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "RT",
        OPTIONAL,
        Version::V14,
        Some(&|s: &[u8]| matches!(s, b"R" | b"Group")),
    )?;

    // IT, optional, name, since V1.6 (1.3 if relaxed)
    validate_name_entry(v.x, d, dict_name, "IT", OPTIONAL, Version::V13, None)?;

    // ExData, optional, dict, since V1.7
    let d1 = validate_dict_entry(v.x, d, dict_name, "ExData", OPTIONAL, Version::V17, None)?;
    if let Some(d1) = d1 {
        validate_ex_data_dict(v.x, &d1)?;
    }

    Ok(())
}

fn validate_markup_annotation(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    depth: u64,
    visit: &mut IrtTraversal,
) -> Result<(), Error> {
    let dict_name = "markupAnnot";
    validate_markup_annotation_part1(v, d, owner_obj_nr, dict_name)?;
    validate_markup_annotation_part2(v, d, owner_obj_nr, dict_name, depth, visit)
}

fn validate_entry_p(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let ir = validate_ind_ref_entry(x, d, dict_name, "P", required, since)?;
    let Some(ir) = ir else {
        return Ok(());
    };

    // check if this indRef points to a pageDict.
    let d1 = x.dereference_dict_ref(ir)?.cloned();
    let Some(d1) = d1 else {
        d.delete(b"P");
        return Ok(());
    };

    validate_name_entry(
        x,
        &d1,
        "pageDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Page"),
    )?;
    Ok(())
}

/// `validateDashPatternArray` (8.4.3.6); an all-zero array is digested if relaxed.
fn validate_dash_pattern_array(x: &mut XRefTable, arr: &[Object]) -> bool {
    for slot in arr {
        let f = match x.dereference(slot) {
            Ok(Object::Integer(i)) => *i as f64,
            Ok(Object::Float(f)) => *f,
            _ => return false,
        };
        if f < 0.0 {
            return false;
        }
    }
    true
}

/// `validateBorderArray`.
fn validate_border_array(x: &mut XRefTable, a: &[Object]) -> bool {
    if a.is_empty() {
        return true;
    }

    let Ok(version) = x.version() else {
        return false;
    };
    if version == Version::V10 {
        return a.len() == 3;
    }

    if !(a.len() == 3 || a.len() == 4) {
        return false;
    }

    for (i, o) in a.iter().enumerate() {
        if i == 3 {
            // Validate the optional dash array (see 12.5.2, Table 166).
            let dpa = match x.dereference_array(o) {
                Ok(Some(dpa)) => dpa.clone(),
                // Relaxed validation accepts a dash array that is no array.
                _ => return true,
            };

            if dpa.is_empty() {
                return true;
            }

            return validate_dash_pattern_array(x, &dpa);
        }

        let f = match x.dereference(o) {
            Ok(Object::Integer(i)) => *i as f64,
            Ok(Object::Float(f)) => *f,
            _ => return false,
        };

        if f < 0.0 {
            return false;
        }
    }

    true
}

/// `digestAnnotationBorderArray`: a border array whose corner radii are uninitialized memory and
/// whose width is 0 is accepted.
fn digest_annotation_border_array(x: &mut XRefTable, a: &[Object]) -> bool {
    if a.len() < 3 || a.len() > 4 {
        return false;
    }

    let Ok(Some(horizontal_radius)) = x.dereference_integer(&a[0]) else {
        return false;
    };
    if horizontal_radius != UNINITIALIZED_ANNOTATION_BORDER_RADIUS {
        return false;
    }

    let Ok(Some(vertical_radius)) = x.dereference_integer(&a[1]) else {
        return false;
    };
    if vertical_radius != UNINITIALIZED_ANNOTATION_BORDER_RADIUS {
        return false;
    }

    matches!(x.dereference_integer(&a[2]), Ok(Some(0)))
}

/// `validateAnnotationType`: the optional `Type`, which is `Annot`. Relaxed validation takes a
/// hex string that reads `Annot`.
fn validate_annotation_type(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // Type, optional, name
    if let Some(o) = d.find(b"Type") {
        let o = x.dereference(o)?.clone();
        if let Object::Hex(h) = &o {
            let s = hex_literal_to_string(h).map_err(|e| {
                e.context(format!("dict={dict_name} entry=Type: decode hex literal"))
            })?;
            if s == "Annot" {
                return x.validate_version(&format!("dict={dict_name} entry=Type"), Version::V10);
            }
        }
    }

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Annot"),
    )?;
    Ok(())
}

/// Go's `CheckAnnotationAppearance` (model/annotation.go:368): from PDF 2.0 on, an annotation
/// other than a popup, projection or link that is not empty needs an appearance dictionary.
pub(crate) fn check_annotation_appearance(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    // see 12.5.2, Table 166
    if x.version()? < Version::V20 {
        return Ok(());
    }
    let null = Object::Null;
    let subtype = x.dereference_name(d.find(b"Subtype").unwrap_or(&null), Version::V10, None)?;
    if matches!(subtype.as_slice(), b"Popup" | b"Projection" | b"Link") {
        return Ok(());
    }
    if x.dereference_dict(d.find(b"AP").unwrap_or(&null))?
        .is_some()
    {
        return Ok(());
    }
    let a = x
        .dereference_array(d.find(b"Rect").unwrap_or(&null))?
        .cloned()
        .unwrap_or_default();
    if a.len() != 4 {
        return Err(Error::Validation(
            "annotation Rect: expected four numbers".to_string(),
        ));
    }
    let r = renderer::rect_for_array(x, &a)?;
    if r.width() == 0.0 && r.height() == 0.0 {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "PDF 2.0 {} annotation requires an appearance dictionary (AP)",
        lossy(&subtype)
    )))
}

/// `validateAnnotationDictGeneralPart1`: the subtype.
fn validate_annotation_dict_general_part1(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
) -> Result<Vec<u8>, Error> {
    // Subtype, required, name
    let Some(subtype) =
        validate_name_entry(x, d, dict_name, "Subtype", REQUIRED, Version::V10, None)?
    else {
        return Err(Error::Panic("nil Subtype"));
    };

    // Rect, required, rectangle. Relaxed validation goes on after an invalid one.
    let _ = validate_rect_entry_repair(x, d, dict_name, "Rect", REQUIRED, Version::V10);

    // Contents, optional, text string
    if validate_string_entry(x, d, dict_name, "Contents", OPTIONAL, Version::V10, None).is_err() {
        let i = validate_integer_entry(x, d, dict_name, "Contents", OPTIONAL, Version::V10, None)?;
        if let Some(i) = i {
            // Repair
            d.set(b"Contents", Object::String(i.to_string().into_bytes()));
        }
    }

    // P, optional, indRef of page dict
    validate_entry_p(x, d, dict_name, OPTIONAL, Version::V10)?;

    // NM, optional, text string, since V1.4 (1.2 if relaxed)
    validate_string_entry(x, d, dict_name, "NM", OPTIONAL, Version::V12, None)?;

    Ok(subtype)
}

fn validate_annotation_dict_general_part2(
    v: &mut Validator<'_>,
    d: &mut Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // M, optional, date string in any format, since V1.1
    validate_string_entry(v.x, d, dict_name, "M", OPTIONAL, Version::V11, None)?;

    // F, optional integer, since V1.1, annotation flags
    validate_integer_entry(v.x, d, dict_name, "F", OPTIONAL, Version::V11, None)?;

    // AP, optional, appearance dict, since V1.2 (1.1 if relaxed)
    validate_appear_dict_entry(v, d, dict_name, OPTIONAL, Version::V11)?;

    // AS, optional, name, since V1.2
    validate_name_entry(v.x, d, dict_name, "AS", OPTIONAL, Version::V11, None)?;

    // Border, optional, array of numbers
    let has_bs = d.find(b"BS").is_some_and(|o| !o.is_null());
    if !has_bs || v.x.version()? < Version::V12 {
        let a = validate_array_entry(v.x, d, dict_name, "Border", OPTIONAL, Version::V10, None)?
            .unwrap_or_default();
        // Uninitialized corner radii are digested.
        if !digest_annotation_border_array(v.x, &a) && !validate_border_array(v.x, &a) {
            return Err(Error::Validation(
                "annotation border array: invalid value".to_string(),
            ));
        }
    }

    // C, optional array, of numbers, since V1.1
    validate_color_array_entry(v.x, d, dict_name, "C", Version::V11)?;

    // StructParent, optional, integer, since V1.3
    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "StructParent",
        OPTIONAL,
        Version::V13,
        None,
    )?;

    Ok(())
}

/// `validateAnnotationDictGeneral` (12.5.2): the subtype, and the entries every annotation has.
fn validate_annotation_dict_general(
    v: &mut Validator<'_>,
    d: &mut Dict,
    dict_name: &str,
) -> Result<Vec<u8>, Error> {
    let subtype = validate_annotation_dict_general_part1(v.x, d, dict_name)?;

    // Relaxed validation digests a missing appearance dictionary of PDF 2.0.
    let _ = check_annotation_appearance(v.x, d);

    validate_annotation_dict_general_part2(v, d, dict_name)?;
    Ok(subtype)
}

/// The version since which the (relaxed) validation accepts an annotation subtype, and whether
/// it is a markup annotation. `None` for a subtype that is not defined.
fn annotation_subtype_info(subtype: &[u8]) -> Option<(Version, bool)> {
    Some(match subtype {
        b"Text" => (Version::V10, true),
        b"Link" => (Version::V10, false),
        b"FreeText" => (Version::V12, true),
        b"Line" => (Version::V13, true),
        b"Polygon" | b"PolyLine" => (Version::V14, true),
        b"Highlight" | b"Underline" | b"StrikeOut" => (Version::V13, true),
        b"Squiggly" => (Version::V14, true),
        b"Square" | b"Circle" => (Version::V13, true),
        b"Stamp" => (Version::V13, true),
        b"Caret" => (Version::V14, true),
        b"Ink" => (Version::V13, true),
        b"Popup" => (Version::V12, false),
        b"FileAttachment" => (Version::V13, true),
        b"Sound" => (Version::V12, true),
        b"Movie" => (Version::V12, false),
        b"Widget" => (Version::V11, false),
        b"Screen" => (Version::V14, false),
        b"PrinterMark" => (Version::V14, false),
        b"TrapNet" => (Version::V13, false),
        b"Watermark" => (Version::V13, false),
        b"3D" => (Version::V16, false),
        b"Redact" => (Version::V17, true),
        b"RichMedia" => (Version::V14, false),
        _ => return None,
    })
}

fn validate_annotation_dict_concrete(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    dict_name: &str,
    subtype: &[u8],
    depth: u64,
    visit: &mut IrtTraversal,
) -> Result<(), Error> {
    // OC, optional, content group dict or content membership dict, since V1.5 (1.3 if relaxed)
    // Specifying the optional content properties for the annotation.
    optional_content::validate_optional_content(v.x, d, dict_name, "OC", OPTIONAL, Version::V13)?;

    // see table 169
    let Some((since, markup)) = annotation_subtype_info(subtype) else {
        v.x.custom_extensions = true;
        return Ok(());
    };
    let k = lossy(subtype);

    v.x.validate_version(&format!("{k} annotation"), since)?;

    if markup {
        validate_markup_annotation(v, d, owner_obj_nr, depth, visit)?;
    }

    match subtype {
        b"Text" => validate_annotation_dict_text(v.x, d, &k),
        b"Link" => validate_annotation_dict_link(v, d, &k),
        b"FreeText" => validate_annotation_dict_free_text(v.x, d, &k),
        b"Line" => validate_annotation_dict_line(v.x, d, &k),
        b"Polygon" | b"PolyLine" => validate_annotation_dict_poly_line(v.x, d, &k),
        b"Highlight" | b"Underline" | b"Squiggly" | b"StrikeOut" => {
            validate_text_markup_annotation(v.x, d, &k)
        }
        b"Square" | b"Circle" => validate_annotation_dict_circle_or_square(v.x, d, &k),
        b"Stamp" => validate_annotation_dict_stamp(v.x, d, &k),
        b"Caret" => validate_annotation_dict_caret(v.x, d, &k),
        b"Ink" => validate_annotation_dict_ink(v.x, d, &k),
        b"Popup" => validate_annotation_dict_popup(v.x, d, &k),
        b"FileAttachment" => validate_annotation_dict_file_attachment(v.x, d, &k),
        b"Sound" => validate_annotation_dict_sound(v.x, d, &k),
        b"Movie" => validate_annotation_dict_movie(v, d, &k),
        b"Widget" => validate_annotation_dict_widget(v, d, &k),
        b"Screen" => validate_annotation_dict_screen(v, d, &k),
        b"PrinterMark" => validate_annotation_dict_printer_mark(v, d, &k),
        b"TrapNet" => validate_annotation_dict_trap_net(v, d, &k),
        b"Watermark" => validate_annotation_dict_watermark(v.x, d, &k),
        b"3D" => three_d::validate_annotation_dict_3d(v, d, &k),
        b"Redact" => validate_annotation_dict_redact(v.x, d, &k),
        b"RichMedia" => validate_rich_media_annotation(v, d, &k),
        _ => Ok(()),
    }
}

/// `validateAnnotationDict`: validates the annotation dictionary `d` (owned by the object
/// `owner_obj_nr`, which keys the reply chain check) and says whether it is a trap network
/// annotation. A repair is made in `d`; whoever passes a copy stores it back.
///
/// # Errors
/// [`Error::Validation`], the cycle error [`Sentinel::AnnotationIrtCycle`] and the errors of the
/// lookups.
pub fn validate_annotation_dict(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
) -> Result<bool, Error> {
    validate_annotation_dict_depth(v, d, owner_obj_nr, 0, &mut IrtTraversal::new())
}

fn validate_annotation_dict_depth(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    depth: u64,
    visit: &mut IrtTraversal,
) -> Result<bool, Error> {
    let dict_name = "annotDict";

    validate_annotation_type(v.x, d, dict_name)?;

    let subtype = validate_annotation_dict_general(v, d, dict_name)?;

    validate_annotation_dict_concrete(v, d, owner_obj_nr, dict_name, &subtype, depth, visit)?;

    Ok(subtype == b"TrapNet")
}

/// `validatePageAnnotationDict` (trapnet_states.go:54): the annotation, and for a trap network
/// annotation the count of its states. `preceding_count` is the number of annotations kept
/// before it.
fn validate_page_annotation_dict(
    v: &mut Validator<'_>,
    d: &mut Dict,
    owner_obj_nr: i64,
    preceding_count: usize,
) -> Result<bool, Error> {
    let trap_net = validate_annotation_dict(v, d, owner_obj_nr)?;
    if trap_net {
        // The page walk requires TrapNet to be last. Count retained annotations, excluding this
        // annotation and removed entries.
        trapnet::validate_trap_net_state_count(v, d, preceding_count)?;
    }
    Ok(trap_net)
}

/// `annotationValidatedAsFormField`: a widget that the form field validation already validated.
fn annotation_validated_as_form_field(x: &XRefTable, ind_ref: Option<Ref>) -> bool {
    ind_ref.is_some_and(|r| matches!(x.is_valid(r), Ok(true)))
}

/// `pageAnnotationError`.
fn page_annotation_error(err: Error, ind_ref: Option<Ref>, i: usize, phase: &str) -> Error {
    match ind_ref {
        Some(r) => err.context(format!("page annotation obj#{}: {phase}", r.num)),
        None => err.context(format!("page annotation array[{i}]: {phase}")),
    }
}

/// `misplacedOutlineItem`: an outline item that found its way into the annotation array of a
/// page (relaxed validation drops it).
fn misplaced_outline_item(x: &mut XRefTable, d: &Dict) -> bool {
    if d.has_entry(b"Subtype") || !d.has_entry(b"Title") {
        return false;
    }

    let Ok(root_dict) = x.catalog() else {
        return false;
    };
    let Some(outline_root) = root_dict.indirect_ref_entry(b"Outlines") else {
        return false;
    };

    let max_depth = x.max_recursion_depth();
    let mut parent = d.indirect_ref_entry(b"Parent");
    let mut visited = BTreeSet::new();
    let mut depth = 0;
    while let Some(p) = parent
        && depth <= max_depth
    {
        if p == outline_root {
            return true;
        }
        if !visited.insert(p.num) {
            return false;
        }
        let Ok(Some(parent_dict)) = x.dereference_dict_ref(p) else {
            return false;
        };
        parent = parent_dict.indirect_ref_entry(b"Parent");
        depth += 1;
    }
    false
}

/// `annotationArrayEntry`: a dictionary of a page annotation array, with its reference and the
/// increment it was written in if it is an indirect object.
struct AnnotationArrayEntry {
    dict: Dict,
    ind_ref: Option<Ref>,
    incr: i64,
}

/// `annotationArrayEntryForValidation`: `None` for an entry that is dropped (a missing object
/// or an empty dictionary).
fn annotation_array_entry_for_validation(
    x: &mut XRefTable,
    o: &Object,
) -> Result<Option<AnnotationArrayEntry>, Error> {
    if let Object::Ref(r) = o {
        let (d, incr) = x
            .dereference_dict_with_incr(o)
            .map_err(|e| e.context("page annotation: dereference"))?;
        // A corrupt (unknown object) or an empty annotation dictionary is removed.
        return Ok(match d.cloned() {
            Some(d) if !d.is_empty() => Some(AnnotationArrayEntry {
                dict: d,
                ind_ref: Some(*r),
                incr,
            }),
            _ => None,
        });
    }

    // Relaxed validation digests a page annotation array without indirect references.
    match o {
        Object::Dict(d) => Ok(Some(AnnotationArrayEntry {
            dict: d.clone(),
            ind_ref: None,
            incr: 0,
        })),
        _ => Err(Error::Validation(
            "page annotation array: expected indirect references".to_string(),
        )),
    }
}

/// Stores the annotation dictionary `d` back into the object `r`.
fn store_ref_dict(x: &mut XRefTable, r: Ref, d: &Dict) -> Result<(), Error> {
    if let Some(stored) = x.dereference_dict_mut(r)? {
        *stored = d.clone();
    }
    Ok(())
}

/// `validateAnnotationsArray`: validates the annotations of the current page, records them in
/// `vstate.page_annots` and returns the array without the annotations that were dropped.
fn validate_annotations_array(
    v: &mut Validator<'_>,
    a: &[Object],
    owner_obj_nr: i64,
) -> Result<Array, Error> {
    // a ... array of indrefs to annotation dicts.
    let cur_page = v.x.cur_page;
    v.x.vstate.page_annots.insert(cur_page, PgAnnots::new());

    // an optional TrapNetAnnotation has to be the final entry in this list.
    let mut has_trap_net = false;

    let mut clean_annots_arr = Array::new();

    for (i, o) in a.iter().enumerate() {
        v.check()?;

        if has_trap_net {
            return Err(Error::Validation(
                "page annotation array: TrapNet must be the last entry".to_string(),
            ));
        }

        let Some(entry) = annotation_array_entry_for_validation(v.x, o)? else {
            continue;
        };
        let AnnotationArrayEntry {
            mut dict,
            ind_ref,
            incr,
        } = entry;

        if misplaced_outline_item(v.x, &dict) {
            // removed outline item from page annotation array
            continue;
        }

        if let Some(r) = ind_ref
            && r.num > 0
        {
            // detectSignature
            form::cache_sig(v.x, &dict, "formFieldDict", false, r.num, incr)
                .map_err(|e| page_annotation_error(e, ind_ref, i, "signature"))?;
        }

        let annot_obj_nr = ind_ref.map_or(owner_obj_nr, |r| r.num);
        if !annotation_validated_as_form_field(v.x, ind_ref) {
            let res =
                validate_page_annotation_dict(v, &mut dict, annot_obj_nr, clean_annots_arr.len());
            let stored = match ind_ref {
                Some(r) => store_ref_dict(v.x, r, &dict),
                None => Ok(()),
            };
            has_trap_net = res.map_err(|e| page_annotation_error(e, ind_ref, i, "validate"))?;
            stored.map_err(|e| page_annotation_error(e, ind_ref, i, "validate"))?;
        }

        // Collect annotation.
        let ann = renderer::annotation(v.x, &dict)
            .map_err(|e| page_annotation_error(e, ind_ref, i, "render"))?;

        // The cleaned array may be stored in the page and the record is kept in
        // `vstate.page_annots`, both for the table's life.
        let direct = if ind_ref.is_none() {
            dict_copy_bytes(&dict)
        } else {
            0
        };
        v.x.held.grow_by(ANNOT_RECORD_BYTES + direct)?;
        clean_annots_arr.push(match ind_ref {
            Some(_) => o.clone(),
            None => Object::Dict(dict),
        });

        // addAnnotation
        let annots =
            v.x.vstate
                .page_annots
                .entry(cur_page)
                .or_default()
                .entry(ann.kind)
                .or_default();
        let obj_nr = match ind_ref {
            Some(r) => {
                annots.ind_refs.push(r);
                r.num
            }
            None => -(i as i64),
        };
        annots.map.insert(obj_nr, ann);
    }

    Ok(clean_annots_arr)
}

/// What the validation of the annotations of a page does to its `Annots` entry.
enum AnnotsEdit {
    Keep,
    Remove,
    Set(Array),
}

fn validate_page_annotations_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
) -> Result<AnnotsEdit, Error> {
    let annots_obj_nr = object_number(owner_obj_nr, d.find(b"Annots"));
    let a = validate_array_entry(v.x, d, "pageDict", "Annots", OPTIONAL, Version::V10, None)?;
    let Some(a) = a else {
        return Ok(AnnotsEdit::Keep);
    };

    let a = remove_nulls(a);

    if a.is_empty() {
        // removed empty annotation array
        return Ok(AnnotsEdit::Remove);
    }

    let cleaned_annots = validate_annotations_array(v, &a, annots_obj_nr)?;

    if cleaned_annots.is_empty() {
        // removed empty annotation array
        return Ok(AnnotsEdit::Remove);
    }

    Ok(AnnotsEdit::Set(cleaned_annots))
}

/// `validatePageAnnotations` (12.5). Annotation validation may repair the array: the result says
/// how.
fn validate_page_annotations(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
) -> Result<AnnotsEdit, Error> {
    let res = validate_page_annotations_entry(v, d, owner_obj_nr);
    // clearRenditionPageAnnotations
    v.st.rendition_pages.annotations.remove(&owner_obj_nr);
    res
}

/// Applies the repair of the `Annots` entry to the page node `o`, if it is a reference.
fn apply_annots_edit(x: &mut XRefTable, o: &Object, edit: AnnotsEdit) -> Result<(), Error> {
    let Object::Ref(r) = o else {
        return Ok(());
    };
    match edit {
        AnnotsEdit::Keep => {}
        AnnotsEdit::Remove => {
            if let Some(stored) = x.dereference_dict_mut(*r)? {
                stored.delete(b"Annots");
            }
        }
        AnnotsEdit::Set(a) => {
            if let Some(stored) = x.dereference_dict_mut(*r)? {
                stored.set(b"Annots", Object::Array(a));
            }
        }
    }
    Ok(())
}

/// `pageAnnotationWalkKidContext`.
fn page_annotation_walk_kid_context(o: &Object, i: usize) -> String {
    match o {
        Object::Ref(r) => format!("page tree annotation walk: kid obj#{}", r.num),
        _ => format!("page tree annotation walk: kid[{i}]"),
    }
}

/// `validatePagesAnnotations` (annotation.go:2317): validates the annotations of the page tree
/// under the page node `d`, from page `cur_page` on, and returns the number of the last page.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_pages_annotations(
    v: &mut Validator<'_>,
    d: &Dict,
    cur_page: i64,
) -> Result<i64, Error> {
    let mut cur_page = cur_page;

    // Iterate over page tree.
    let kids_array = d.array_entry(b"Kids").cloned().unwrap_or_default();

    for (i, o) in kids_array.iter().enumerate() {
        v.check()?;

        if o.is_null() {
            continue;
        }

        let kid_context = page_annotation_walk_kid_context(o, i);

        let kid =
            v.x.dereference_dict(o)
                .map_err(|e| match o {
                    Object::Ref(_) => e.context(format!("{kid_context}: dereference")),
                    _ => e.context("page tree annotation walk: dereference kid"),
                })?
                .cloned();
        let Some(kid) = kid else {
            return Err(Error::Validation(format!(
                "{kid_context}: page node is null"
            )));
        };

        let (dict_type, _) =
            v.x.dereference_name_entry(&kid, b"Type")
                .map_err(|e| e.context(format!("{kid_context}: page node Type")))?;
        let Some(dict_type) = dict_type else {
            return Err(Error::Validation(format!(
                "{kid_context}: missing page node Type"
            )));
        };

        match dict_type.as_slice() {
            b"Pages" => {
                // Recurse over pagetree
                cur_page = validate_pages_annotations(v, &kid, cur_page)
                    .map_err(|e| e.context(kid_context))?;
            }

            b"Page" => {
                cur_page += 1;
                v.x.cur_page = cur_page;
                let edit = validate_page_annotations(v, &kid, object_number(0, Some(o)))
                    .map_err(|e| e.context(format!("page {cur_page}")))?;
                apply_annots_edit(v.x, o, edit)?;
            }

            other => {
                return Err(Error::Validation(format!(
                    "{kid_context}: unexpected page node Type {}",
                    lossy(other)
                )));
            }
        }
    }

    Ok(cur_page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::AnnotationType;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::array::new_number_array;
    use std::sync::atomic::AtomicBool;

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x.cur_page = 1;
        x
    }

    fn reference(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn text_annotation() -> Dict {
        let mut d = Dict::default();
        d.set(b"Type", Object::name("Annot"));
        d.set(b"Subtype", Object::name("Text"));
        d.set(
            b"Rect",
            Object::Array(new_number_array(&[0.0, 0.0, 1.0, 1.0])),
        );
        d.set(b"Contents", Object::string("annotation"));
        d
    }

    /// Go: `annotationCancellationXRefTable`.
    fn table_with_annotations(n: i64) -> (XRefTable, Array) {
        let mut x = table();
        let mut a = Array::new();
        for num in 1..=n {
            x.table
                .insert(num, Entry::gen0(Object::Dict(text_annotation())));
            a.push(reference(num));
        }
        (x, a)
    }

    #[test]
    fn annotations_array_stops_when_cancelled() {
        let (mut x, a) = table_with_annotations(4);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(matches!(
            validate_annotations_array(&mut v, &a, 10),
            Err(Error::Cancelled)
        ));
        assert!(v.x.vstate.page_annots[&1].is_empty());
    }

    #[test]
    fn annotations_array_records_the_annotations() {
        let (mut x, a) = table_with_annotations(3);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let clean = validate_annotations_array(&mut v, &a, 10).unwrap();
        assert_eq!(clean.len(), 3);
        let texts = &v.x.vstate.page_annots[&1][&AnnotationType::Text];
        assert_eq!(texts.ind_refs.len(), 3);
        assert_eq!(texts.map.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn a_trap_net_has_to_be_last() {
        let (mut x, _) = table_with_annotations(1);
        let mut trap = Dict::default();
        trap.set(b"Subtype", Object::name("TrapNet"));
        trap.set(
            b"Rect",
            Object::Array(new_number_array(&[0.0, 0.0, 1.0, 1.0])),
        );
        trap.set(b"F", Object::Integer(0));
        x.table.insert(2, Entry::gen0(Object::Dict(trap)));
        let cancel = AtomicBool::new(false);

        let mut v = Validator::new(&mut x, &cancel);
        let ok = vec![reference(1), reference(2)];
        assert_eq!(
            validate_annotations_array(&mut v, &ok, 10).unwrap().len(),
            2
        );

        let bad = vec![reference(2), reference(1)];
        let e = validate_annotations_array(&mut v, &bad, 10).unwrap_err();
        assert_eq!(
            e,
            Error::Validation("page annotation array: TrapNet must be the last entry".to_string())
        );
    }

    #[test]
    fn a_reply_cycle_is_an_error() {
        let mut x = table();
        for (num, other) in [(1, 2), (2, 1)] {
            let mut d = text_annotation();
            d.set(b"IRT", reference(other));
            x.table.insert(num, Entry::gen0(Object::Dict(d)));
        }
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let mut d = text_annotation();
        d.set(b"IRT", reference(2));
        let e = validate_annotation_dict(&mut v, &mut d, 1).unwrap_err();
        assert!(e.is(Sentinel::AnnotationIrtCycle));
    }

    #[test]
    fn a_direct_annotation_is_repaired_and_keyed_by_its_negated_index() {
        let mut x = table();
        let mut d = text_annotation();
        d.set(b"Contents", Object::Integer(42));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let a = vec![Object::Dict(text_annotation()), Object::Dict(d)];
        let clean = validate_annotations_array(&mut v, &a, 10).unwrap();
        let Object::Dict(repaired) = &clean[1] else {
            panic!("expected a dictionary");
        };
        assert!(matches!(repaired.find(b"Contents"), Some(Object::String(s)) if s == b"42"));
        let texts = &v.x.vstate.page_annots[&1][&AnnotationType::Text];
        assert!(texts.ind_refs.is_empty());
        assert_eq!(texts.map.keys().copied().collect::<Vec<_>>(), vec![-1, 0]);
    }

    /// pdf20/utf8stringAndAnnotation.pdf without its first `stream` keyword: `/AP << /N 3 0 R >>`
    /// refers to a dictionary, not a stream. Go deletes `N` from the direct `AP` dictionary.
    #[test]
    fn a_corrupt_normal_appearance_is_deleted_from_a_direct_ap() {
        let mut x = table();
        let mut form = Dict::default();
        form.set(b"Type", Object::name("XObject"));
        form.set(b"Subtype", Object::name("Form"));
        x.table.insert(3, Entry::gen0(Object::Dict(form)));
        let mut ap = Dict::default();
        ap.set(b"N", reference(3));
        let mut d = text_annotation();
        d.set(b"AP", Object::Dict(ap));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_annotation_dict(&mut v, &mut d, 1).unwrap();
        assert!(matches!(d.find(b"AP"), Some(Object::Dict(ap)) if ap.is_empty()));
    }

    #[test]
    fn the_border_array_rules() {
        let mut x = table();
        assert!(validate_border_array(&mut x, &[]));
        assert!(validate_border_array(
            &mut x,
            &new_number_array(&[0.0, 0.0, 1.0])
        ));
        assert!(!validate_border_array(
            &mut x,
            &new_number_array(&[0.0, 0.0])
        ));
        assert!(!validate_border_array(
            &mut x,
            &new_number_array(&[0.0, -1.0, 1.0])
        ));
        let mut dashed = new_number_array(&[0.0, 0.0, 1.0]);
        dashed.push(Object::Array(new_number_array(&[3.0, 1.0])));
        assert!(validate_border_array(&mut x, &dashed));
        let uninitialized = [
            Object::Integer(UNINITIALIZED_ANNOTATION_BORDER_RADIUS),
            Object::Integer(UNINITIALIZED_ANNOTATION_BORDER_RADIUS),
            Object::Integer(0),
        ];
        assert!(digest_annotation_border_array(&mut x, &uninitialized));
        assert!(!digest_annotation_border_array(
            &mut x,
            &new_number_array(&[0.0, 0.0, 0.0])
        ));
    }

    #[test]
    fn the_text_state_must_fit_the_model() {
        let ok = |s: Option<&str>, m: Option<&str>| validate_text_annotation_state("Text", s, m);
        assert_eq!(ok(None, None), Ok(()));
        assert!(ok(None, Some("Marked")).is_err());
        assert_eq!(ok(Some("Marked"), Some("Marked")), Ok(()));
        assert!(ok(Some("Accepted"), Some("Marked")).is_err());
        assert_eq!(ok(Some("Accepted"), Some("Review")), Ok(()));
        // Relaxed validation takes another model with a state.
        assert_eq!(ok(Some("x"), Some("Custom")), Ok(()));
        assert!(ok(Some(""), Some("Custom")).is_err());
        assert!(ok(Some("x"), Some("")).is_err());
    }

    #[test]
    fn an_outline_item_in_the_annotations_is_misplaced() {
        let mut x = table();
        let mut catalog = Dict::default();
        catalog.set(b"Outlines", reference(5));
        x.root_dict = Some(catalog);
        let mut parent = Dict::default();
        parent.set(b"Parent", reference(5));
        x.table.insert(6, Entry::gen0(Object::Dict(parent)));
        let mut item = Dict::default();
        item.set(b"Title", Object::string("t"));
        item.set(b"Parent", reference(6));
        assert!(misplaced_outline_item(&mut x, &item));
        item.set(b"Subtype", Object::name("Text"));
        assert!(!misplaced_outline_item(&mut x, &item));
    }
}
