//! Actions (Go: validate/action.go, form_actions.go).
//!
//! Relaxed validation only. Differences from Go, besides those listed in `validate/mod.rs`:
//!
//! * The active content origins and notices (`activeContentOrigin`, `collectActionActiveContent`,
//!   `recordMultimediaCompatibility`) and the link collection are not ported, so the `origin`
//!   parameters are gone. `validateAdditionalActions` and `validateAdditionalActionsWithOwner`
//!   differ only in the owner object number, which only labels errors; both names exist
//!   ([`validate_additional_actions`], [`validate_additional_actions_with_owner`]).
//! * `validateJavaScript` neutralizes its `required` parameter in relaxed mode, so it has none.
//! * Go's `validateGoToEActionDict` repairs the stored action dictionary (a `Dest` entry becomes
//!   `D`). The action validators take `&Dict`, so [`validate_action_dict_object_depth`] makes the
//!   repair on its copy and stores it back if the action is an object (a reference). A direct
//!   action dictionary keeps its `Dest`; whoever owns that dictionary would have to store it.
//!   The repairs that the annotation and transition validators make in the dictionaries they
//!   are given (`AN` of a rendition action, `Trans`, `TA`) are stored back the same way.
//! * `ActionVisit` is `model.ActionVisit` (recursion.go:273); the target chain of an embedded
//!   go-to action has its own set of object numbers (Go's `targetTraversal`).
//! * `form_actions.go` is here: the state it keeps in the context is
//!   [`PassState::form_action_pages`](super::state::PassState::form_action_pages).
//!
//! # Calls into other packages
//!
//! * `three_d::validate_3d_view_selector(v, d, dict_name, entry_name, required, kind)` with
//!   `three_d::ViewSelectorKind::Action` (WP 21), for the `GoTo3DView` action; Go's owner object
//!   number is dropped. This does not exist yet.
//! * `annotation::validate_annotation_dict`, `annotation::validate_rendition_screen_annotation_structure`,
//!   `media::validate_rendition_dict`, `media::validate_rendition_page_membership`,
//!   `media::rendition_page_annotations`, `page::validate_transition_dict` and
//!   `optional_content::validate_optional_content_group_dict`, as they are written now.

use super::Validator;
use super::destination::{
    validate_action_destination_entry, validate_remote_action_destination_entry,
};
use super::file_spec::{validate_file_spec_entry, validate_url_spec_entry};
use super::object::{
    OPTIONAL, REQUIRED, array_cardinality_error, dict_entry_context, object_context,
    validate_array_entry, validate_boolean_entry, validate_dict_entry, validate_entry,
    validate_ind_ref_entry, validate_int_or_string_entry, validate_integer_array_entry,
    validate_integer_entry, validate_integer_for_object, validate_name_entry,
    validate_number_array_entry, validate_number_entry, validate_number_for_object,
    validate_rectangle_entry, validate_stream_dict_entry, validate_stream_dict_for_object,
    validate_string_entry,
};
use super::{annotation, media, optional_content, page, three_d};
use crate::error::{Error, Sentinel};
use crate::model::deref::{text, type_name};
use crate::model::{PageTreeVisit, Version, XRefTable};
use crate::object::string::{hex_decode, unescape};
use crate::object::text::{hex_literal_to_string, string_literal_to_string};
use crate::object::{Dict, Object, Ref, StreamDict};
use std::collections::{BTreeMap, BTreeSet};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `validationObjectNumber(0, o)`: the object number of a reference, else 0.
fn ref_num(o: Option<&Object>) -> i64 {
    match o {
        Some(Object::Ref(r)) => r.num,
        _ => 0,
    }
}

/// `model.WrapRecursionError`: adds `context` to `e` unless `e` is a recursion limit error.
fn wrap_recursion_error(context: String, e: Error) -> Error {
    if e.is(Sentinel::MaxRecursionDepth) {
        e
    } else {
        e.context(context)
    }
}

/// Stores `d`, which a validator repaired, in the object `raw` refers to. A direct dictionary is
/// the caller's copy and stays as it is.
fn store_repaired_dict(x: &mut XRefTable, raw: Option<&Object>, d: &Dict) -> Result<(), Error> {
    if let Some(Object::Ref(r)) = raw
        && let Some(slot) = x.dereference_dict_mut(*r)?
    {
        *slot = d.clone();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Go-to actions

/// `validateGoToActionDict` (12.6.4.2): `D` is optional in relaxed mode.
fn validate_go_to_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_action_destination_entry(x, d, dict_name, "D", OPTIONAL, Version::V10)
}

/// `validateGoToRActionDict` (12.6.4.3).
fn validate_go_to_r_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_file_spec_entry(x, d, dict_name, "F", REQUIRED, Version::V11)?;
    validate_remote_action_destination_entry(x, d, dict_name, "D")?;
    validate_boolean_entry(x, d, dict_name, "NewWindow", OPTIONAL, Version::V12, None)?;
    Ok(())
}

/// `targetTraversal.enter`: the object numbers of the target chain that is being validated.
fn enter_target(visit: &mut BTreeSet<i64>, obj_nr: i64) -> Result<(), Error> {
    if obj_nr <= 0 {
        return Ok(());
    }
    if !visit.insert(obj_nr) {
        return Err(Error::Sentinel(Sentinel::TargetCycle).context(format!("obj#{obj_nr}")));
    }
    Ok(())
}

/// `validateTargetDictEntry` (table 202): the `T` entry of an embedded go-to action.
fn validate_target_dict_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_target_dict_entry_depth(
        v,
        d,
        dict_name,
        entry_name,
        required,
        since,
        0,
        &mut BTreeSet::new(),
    )
}

fn validate_target_dict_entry_depth(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    depth: u64,
    visit: &mut BTreeSet<i64>,
) -> Result<(), Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let target_obj_nr = ref_num(raw.as_ref());
    let context = dict_entry_context(dict_name, entry_name, raw.as_ref());

    v.check()?;

    let d1 = match validate_dict_entry(v.x, d, dict_name, entry_name, required, since, None) {
        Err(e) => return Err(e.context(context)),
        Ok(None) => return Ok(()),
        Ok(Some(d1)) => d1,
    };
    v.x.check_recursion_depth("embedded target chain", depth)
        .map_err(|e| e.context(context.clone()))?;
    enter_target(visit, target_obj_nr).map_err(|e| e.context(context.clone()))?;

    let r = validate_target_dict(v, &d1, &context, depth, visit);
    if target_obj_nr > 0 {
        visit.remove(&target_obj_nr);
    }
    r
}

/// The entries of a target dictionary; `context` names the entry that holds it.
fn validate_target_dict(
    v: &mut Validator<'_>,
    d1: &Dict,
    context: &str,
    depth: u64,
    visit: &mut BTreeSet<i64>,
) -> Result<(), Error> {
    let dict_name = "targetDict";

    // R, required, name
    validate_name_entry(
        v.x,
        d1,
        dict_name,
        "R",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"P" || s == b"C"),
    )
    .map_err(|e| e.context(format!("{context}: R")))?;

    // N, optional, byte string
    validate_string_entry(v.x, d1, dict_name, "N", OPTIONAL, Version::V10, None)
        .map_err(|e| e.context(format!("{context}: N")))?;

    // P, optional, integer or byte string
    validate_int_or_string_entry(v.x, d1, dict_name, "P", OPTIONAL, Version::V10)
        .map_err(|e| e.context(format!("{context}: P")))?;

    // A, optional, integer or text string
    validate_int_or_string_entry(v.x, d1, dict_name, "A", OPTIONAL, Version::V10)
        .map_err(|e| e.context(format!("{context}: A")))?;

    // T, optional, target dict
    validate_target_dict_entry_depth(
        v,
        d1,
        dict_name,
        "T",
        OPTIONAL,
        Version::V10,
        depth + 1,
        visit,
    )
    .map_err(|e| e.context(context.to_string()))
}

/// `validateGoToEActionDict` (12.6.4.4). Returns whether the dictionary has to be repaired: if
/// `D` is no remote destination but `Dest` is one, relaxed validation moves `Dest` to `D`.
fn validate_go_to_e_action_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<bool, Error> {
    // F, optional, file specification
    let f = validate_file_spec_entry(v.x, d, dict_name, "F", OPTIONAL, Version::V11)?;

    // D, required, name, byte string or array
    let repaired = validate_remote_action_destination_entry(v.x, d, dict_name, "D").is_err()
        && validate_remote_action_destination_entry(v.x, d, dict_name, "Dest").is_ok();

    // NewWindow, optional, boolean, since V1.1 in relaxed mode
    validate_boolean_entry(v.x, d, dict_name, "NewWindow", OPTIONAL, Version::V11, None)?;

    // T, required unless entry F is present, target dict
    validate_target_dict_entry(v, d, dict_name, "T", f.is_none(), Version::V10)?;
    Ok(repaired)
}

// ---------------------------------------------------------------------------------------------
// Launch, thread and URI actions

/// `validateWinDict` (table 204).
fn validate_win_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "winDict";
    validate_string_entry(x, d, dict_name, "F", REQUIRED, Version::V10, None)?;
    validate_string_entry(x, d, dict_name, "D", OPTIONAL, Version::V10, None)?;
    validate_string_entry(x, d, dict_name, "O", OPTIONAL, Version::V10, None)?;
    validate_string_entry(x, d, dict_name, "P", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateLaunchActionDict` (12.6.4.5).
fn validate_launch_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_file_spec_entry(x, d, dict_name, "F", OPTIONAL, Version::V11)?;
    // Win, optional, dict. Mac and Unix are undefined dictionaries.
    let win = validate_dict_entry(x, d, dict_name, "Win", OPTIONAL, Version::V10, None)?;
    if let Some(win) = win {
        validate_win_dict(x, &win)?;
    }
    Ok(())
}

/// `validateDestinationThreadEntry` (table 205): a thread dictionary, the title of the thread or
/// its index.
fn validate_destination_thread_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::Dict(_) | Object::String(_) | Object::Integer(_) => Ok(()),
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected thread dict, string or integer, got {}",
            type_name(&other)
        ))),
    }
}

/// `validateDestinationBeadEntry` (table 205): a bead dictionary or the index of the bead.
fn validate_destination_bead_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::Dict(_) | Object::Integer(_) => Ok(()),
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected bead dict or integer, got {}",
            type_name(&other)
        ))),
    }
}

/// `validateThreadActionDict` (12.6.4.6).
fn validate_thread_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_file_spec_entry(x, d, dict_name, "F", OPTIONAL, Version::V11)?;
    validate_destination_thread_entry(x, d, dict_name, "D", REQUIRED, Version::V10)?;
    validate_destination_bead_entry(x, d, dict_name, "B", OPTIONAL, Version::V10)
}

/// `validateURIActionDict` (12.6.4.7).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_uri_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_string_entry(x, d, dict_name, "URI", REQUIRED, Version::V10, None)?;
    validate_boolean_entry(x, d, dict_name, "IsMap", OPTIONAL, Version::V10, None)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Sound and movie actions

/// `soundStreamDictEntry`: the sound stream of the entry, which may also be a direct stream
/// that `validate_stream_dict_entry` has skipped.
fn sound_stream_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<StreamDict>, Error> {
    let sd = validate_stream_dict_entry(x, d, dict_name, entry_name, required, since, None)?;
    if sd.is_some() {
        return Ok(sd);
    }
    match d.find(entry_name.as_bytes()) {
        None | Some(Object::Null) => Ok(None),
        Some(o) => validate_stream_dict_for_object(x, o).map(Some),
    }
}

/// `validateSoundSamplingRate` (13.3): `R` is required without `F`; relaxed validation only
/// notes a missing one.
fn validate_sound_sampling_rate(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let rate = validate_entry(x, d, "soundDict", "R", OPTIONAL, Version::V10)?;
    if rate.is_some() {
        validate_number_entry(
            x,
            d,
            "soundDict",
            "R",
            OPTIONAL,
            Version::V10,
            Some(&|f: f64| f > 0.0),
        )?;
    }
    Ok(())
}

/// `validateSoundDictEntry` (13.3).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_sound_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(sd) = sound_stream_dict_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    let dict_name = "soundDict";

    // Type, optional, name
    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Sound"),
    )?;

    // F, optional, file specification for self-describing external sound data.
    validate_file_spec_entry(x, &sd.dict, dict_name, "F", OPTIONAL, Version::V12)?;

    // R, required without F, number - sampling rate
    validate_sound_sampling_rate(x, &sd.dict)?;

    // C, optional, integer - number of sound channels
    validate_integer_entry(
        x,
        &sd.dict,
        dict_name,
        "C",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i > 0),
    )?;

    // B, optional, integer - bits per sample value per channel
    validate_integer_entry(
        x,
        &sd.dict,
        dict_name,
        "B",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| i > 0),
    )?;

    // E, optional, name - encoding format
    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "E",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"Raw" | b"Signed" | b"muLaw" | b"ALaw")),
    )?;

    // CO, optional, name - sound compression format
    validate_name_entry(x, &sd.dict, dict_name, "CO", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateSoundActionDict` (12.6.4.8).
fn validate_sound_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_sound_dict_entry(x, d, dict_name, "Sound", REQUIRED, Version::V10)?;

    // Volume, optional, number: -1.0 .. +1.0
    validate_number_entry(
        x,
        d,
        dict_name,
        "Volume",
        OPTIONAL,
        Version::V10,
        Some(&|f: f64| (-1.0..=1.0).contains(&f)),
    )?;

    for entry_name in ["Synchronous", "Repeat", "Mix"] {
        validate_boolean_entry(x, d, dict_name, entry_name, OPTIONAL, Version::V10, None)?;
    }
    Ok(())
}

/// `validateMovieTimeValue`: a non-negative integer or an 8-byte string.
fn validate_movie_time_value(x: &mut XRefTable, o: &Object, context: &str) -> Result<(), Error> {
    let target = x
        .dereference(o)
        .map_err(|e| e.context(format!("{context}: dereference")))?;
    let valid = match target {
        Object::Integer(i) => *i >= 0,
        Object::String(s) => unescape(s).len() == 8,
        Object::Hex(h) => {
            hex_decode(h)
                .map_err(|e| e.context(format!("{context}: decode string")))?
                .len()
                == 8
        }
        _ => {
            return Err(Error::Validation(format!(
                "{context}: expected non-negative integer or 8-byte string"
            )));
        }
    };
    if valid {
        Ok(())
    } else {
        Err(Error::Validation(format!("{context}: invalid time value")))
    }
}

/// `validateMovieStartOrDurationEntry`: a time value, or a time value and a positive time scale.
fn validate_movie_start_or_duration_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    let Object::Array(a) = &o else {
        return validate_movie_time_value(x, &o, &format!("{dict_name}.{entry_name}"));
    };
    let [time, scale] = a.as_slice() else {
        return Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected array length 2, got {}",
            a.len()
        )));
    };
    validate_movie_time_value(x, time, &format!("{dict_name}.{entry_name}[0]"))?;
    validate_integer_for_object(x, scale, Some(&|i: i64| i > 0))?;
    Ok(())
}

/// `validateMoviePositiveIntegerArrayEntry`: an optional array of 2 positive integers.
fn validate_movie_positive_integer_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    let a = validate_integer_array_entry(
        x,
        d,
        dict_name,
        entry_name,
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    for o in a.iter().flatten() {
        validate_integer_for_object(x, o, Some(&|i: i64| i > 0))?;
    }
    Ok(())
}

/// `validateMovieUnitIntervalArrayEntry`: an optional array of 2 numbers from 0 to 1. This holds
/// in relaxed mode too.
fn validate_movie_unit_interval_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    let a = validate_number_array_entry(
        x,
        d,
        dict_name,
        entry_name,
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    for o in a.iter().flatten() {
        let f = match validate_number_for_object(x, o)? {
            Object::Integer(i) => i as f64,
            Object::Float(f) => f,
            _ => 0.0,
        };
        if !(0.0..=1.0).contains(&f) {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid dict entry: {f}"
            )));
        }
    }
    Ok(())
}

/// `validateMovieActivationDict` (13.4, table 296).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_movie_activation_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "movieActivationDict";

    validate_movie_start_or_duration_entry(x, d, dict_name, "Start", OPTIONAL, Version::V10)?;
    validate_movie_start_or_duration_entry(x, d, dict_name, "Duration", OPTIONAL, Version::V10)?;

    // Rate, optional, number
    validate_number_entry(x, d, dict_name, "Rate", OPTIONAL, Version::V10, None)?;

    // Volume, optional, number: -1.0 .. +1.0
    validate_number_entry(
        x,
        d,
        dict_name,
        "Volume",
        OPTIONAL,
        Version::V10,
        Some(&|f: f64| (-1.0..=1.0).contains(&f)),
    )?;

    // ShowControls, optional, boolean
    validate_boolean_entry(
        x,
        d,
        dict_name,
        "ShowControls",
        OPTIONAL,
        Version::V10,
        None,
    )?;

    // Mode, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Mode",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"Once" | b"Open" | b"Repeat" | b"Palindrome")),
    )?;

    // Synchronous, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Synchronous", OPTIONAL, Version::V10, None)?;

    // FWScale, optional, array of 2 positive integers
    validate_movie_positive_integer_array_entry(x, d, dict_name, "FWScale")?;

    // FWPosition, optional, array of 2 numbers [0.0 .. 1.0]
    validate_movie_unit_interval_array_entry(x, d, dict_name, "FWPosition")
}

/// `validateMovieActionAnnotation`: `Annotation` is a reference to a movie annotation.
fn validate_movie_action_annotation(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let Some(ir) = validate_ind_ref_entry(x, d, dict_name, "Annotation", REQUIRED, Version::V10)?
    else {
        return Ok(());
    };
    let annotation = x
        .dereference_dict_ref(ir)
        .map_err(|e| {
            e.context(format!(
                "{dict_name}.Annotation: dereference movie annotation dict"
            ))
        })?
        .cloned();
    let Some(annotation) = annotation else {
        return Err(Error::Validation(
            "movie action: missing required entry \"T\" or \"Annotation\"".to_string(),
        ));
    };
    validate_name_entry(
        x,
        &annotation,
        "annotDict",
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Movie"),
    )?;
    Ok(())
}

/// `validateMovieActionTarget` (12.6.4.10): `T` or `Annotation`. Relaxed validation accepts a
/// movie action with both or neither, and a bad `T` if the annotation is good.
fn validate_movie_action_target(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_entry(x, d, dict_name, "T", OPTIONAL, Version::V10)?;
    let annotation = validate_entry(x, d, dict_name, "Annotation", OPTIONAL, Version::V10)?;
    let title_err = validate_string_entry(x, d, dict_name, "T", OPTIONAL, Version::V10, None).err();
    let annotation_err = if annotation.is_some() {
        validate_movie_action_annotation(x, d, dict_name).err()
    } else {
        None
    };
    match title_err {
        Some(e) if annotation.is_none() || annotation_err.is_some() => Err(e),
        _ => Ok(()),
    }
}

/// `validateMovieActionDict` (12.6.4.10).
fn validate_movie_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_movie_activation_dict(x, d)?;
    validate_movie_action_target(x, d, dict_name)?;
    validate_name_entry(
        x,
        d,
        dict_name,
        "Operation",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"Play" | b"Stop" | b"Pause" | b"Resume")),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Hide, named and form actions

/// `validateHideActionDictEntryT`: the target of a hide action.
fn validate_hide_action_dict_entry_t(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    match o {
        Object::String(_) | Object::Hex(_) => {
            // Ensure UTF16 correctness.
            text(o)?;
        }
        // An annotation dictionary, which needs a Subtype.
        Object::Dict(d) => {
            validate_name_entry(x, d, "annotDict", "Subtype", REQUIRED, Version::V10, None)?;
        }
        // A mixed array of annotation dictionary references and strings.
        Object::Array(a) => {
            for (i, el) in a.iter().enumerate() {
                let target = x
                    .dereference(el)
                    .map_err(|e| e.context(format!("Hide.T[{i}]: dereference")))?
                    .clone();
                match &target {
                    Object::Null => {}
                    Object::String(_) | Object::Hex(_) => {
                        text(&target).map_err(|e| e.context(format!("Hide.T[{i}]: string")))?;
                    }
                    Object::Dict(d) => {
                        validate_name_entry(
                            x,
                            d,
                            "annotDict",
                            "Subtype",
                            REQUIRED,
                            Version::V10,
                            None,
                        )
                        .map_err(|e| e.context(format!("Hide.T[{i}]: annotation dict")))?;
                    }
                    other => {
                        return Err(Error::Validation(format!(
                            "Hide.T[{i}]: expected string or annotation dict, got {}",
                            type_name(other)
                        )));
                    }
                }
            }
        }
        other => {
            return Err(Error::Validation(format!(
                "Hide.T: expected string, annotation dict or array, got {}",
                type_name(other)
            )));
        }
    }
    Ok(())
}

/// `validateHideActionDict` (12.6.4.10).
fn validate_hide_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // T, required, dict, text string or array
    let Some(t) = d.find(b"T").filter(|o| !o.is_null()) else {
        return Err(Error::Validation(
            "Hide action: missing required entry \"T\"".to_string(),
        ));
    };
    let target = x
        .dereference(t)
        .map_err(|e| e.context("Hide.T: dereference"))?
        .clone();
    if target.is_null() {
        return Err(Error::Validation("Hide.T: missing object".to_string()));
    }
    validate_hide_action_dict_entry_t(x, &target)?;

    // H, optional, boolean
    validate_boolean_entry(x, d, dict_name, "H", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateNamedActionDict` (12.6.4.11): a standard name, or one of the known nonstandard ones.
fn validate_named_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    validate_name_entry(
        x,
        d,
        dict_name,
        "N",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| {
            matches!(
                s,
                b"NextPage"
                    | b"PrevPage"
                    | b"FirstPage"
                    | b"LastPage"
                    | b"AcroSrch:Query"
                    | b"AcroSrch:Results"
                    | b"Find"
                    | b"FindAgain"
                    | b"FindAgainDoc"
                    | b"FindPrevious"
                    | b"FindPreviousDoc"
                    | b"FullScreen"
                    | b"GoBack"
                    | b"GoBackDoc"
                    | b"GoForward"
                    | b"GoToPage"
                    | b"Print"
                    | b"Quit"
                    | b"SaveAs"
                    | b"ShowHideBookmarks"
                    | b"FitPage"
                    | b"FitWidth"
                    | b"Close"
                    | b"CropPages"
                    | b"ZoomViewIn"
                    | b"ZoomViewOut"
            )
        }),
    )?;
    Ok(())
}

/// The `Fields` array of a submit-form or reset-form action: references to field dictionaries
/// or (since 1.3) text strings with the fully qualified field names.
fn validate_form_action_fields(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    action: &str,
) -> Result<(), Error> {
    let a = validate_array_entry(x, d, dict_name, "Fields", OPTIONAL, Version::V10, None)?;
    for (i, el) in a.iter().flatten().enumerate() {
        if !matches!(el, Object::Ref(_)) {
            if matches!(el, Object::String(_) | Object::Hex(_)) {
                continue;
            }
            return Err(Error::Validation(format!(
                "{action}.Fields[{i}]: expected string or indirect reference, got {}",
                type_name(el)
            )));
        }
        let o = x
            .dereference(el)
            .map_err(|e| e.context(format!("{action}.Fields[{i}]: dereference")))?;
        if !matches!(o, Object::String(_) | Object::Hex(_) | Object::Dict(_)) {
            return Err(Error::Validation(format!(
                "{action}.Fields[{i}]: expected string or field dictionary, got {}",
                type_name(o)
            )));
        }
    }
    Ok(())
}

/// `validateSubmitFormActionDict` (12.7.5.2).
fn validate_submit_form_action_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // F, required, URL specification
    validate_url_spec_entry(x, d, dict_name, "F", REQUIRED, Version::V10)?;
    validate_form_action_fields(x, d, dict_name, "SubmitForm")?;
    validate_integer_entry(x, d, dict_name, "Flags", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateResetFormActionDict` (12.7.5.3).
fn validate_reset_form_action_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_form_action_fields(x, d, dict_name, "ResetForm")?;
    validate_integer_entry(x, d, dict_name, "Flags", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateImportDataActionDict` (12.7.5.4).
fn validate_import_data_action_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_file_spec_entry(x, d, dict_name, "F", OPTIONAL, Version::V11)?;
    Ok(())
}

/// `validateJavaScript`: a text string or a stream. Relaxed validation accepts it since 1.2, and
/// a missing entry.
fn validate_java_script(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, OPTIONAL, Version::V12)? else {
        return Ok(());
    };
    match o {
        // Ensure UTF16 correctness.
        Object::String(s) => string_literal_to_string(&s).map(|_| ()),
        Object::Hex(h) => hex_literal_to_string(&h).map(|_| ()),
        Object::Stream(_) => Ok(()),
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected string, hex literal or stream dict, got {}",
            type_name(&other)
        ))),
    }
}

/// `validateJavaScriptActionDict` (12.6.4.16).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_java_script_action_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_java_script(x, d, dict_name, "JS")
}

// ---------------------------------------------------------------------------------------------
// Optional content, rendition, transition and 3D actions

/// `validateSetOCGStateGroup`: an optional content group.
fn validate_set_ocg_state_group(
    x: &mut XRefTable,
    o: &Object,
    index: usize,
    dict_name: &str,
) -> Result<(), Error> {
    let group = x
        .dereference(o)
        .map_err(|e| {
            e.context(format!(
                "{dict_name}.State[{index}]: dereference optional content group"
            ))
        })?
        .clone();
    let Object::Dict(group) = group else {
        return Err(Error::Validation(format!(
            "{dict_name}.State[{index}]: expected optional content group dictionary"
        )));
    };
    optional_content::validate_optional_content_group_dict(x, &group, Version::V15)
        .map_err(|e| e.context(format!("{dict_name}.State[{index}]")))
}

/// `validateSetOCGStateArray`: sequences of an operator (`ON`, `OFF` or `Toggle`) and its
/// optional content groups.
fn validate_set_ocg_state_array(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let entry_name = "State";
    let a = validate_array_entry(x, d, dict_name, entry_name, REQUIRED, Version::V10, None)?
        .unwrap_or_default();
    if a.is_empty() {
        return Err(array_cardinality_error(
            dict_name,
            entry_name,
            0,
            "one or more operator and OCG sequences",
        ));
    }
    let (mut operator_seen, mut operand_seen) = (false, false);
    for (i, raw) in a.iter().enumerate() {
        let operator = match x.dereference(raw) {
            Err(e) => {
                return Err(e.context(format!("{dict_name}.{entry_name}[{i}]: dereference")));
            }
            Ok(Object::Name(n)) => Some(n.clone()),
            Ok(_) => None,
        };
        if let Some(n) = operator {
            if !matches!(n.as_slice(), b"ON" | b"OFF" | b"Toggle") {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}[{i}]: invalid operator {}",
                    lossy(&n)
                )));
            }
            if operator_seen && !operand_seen {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}[{i}]: preceding operator has no OCG operands"
                )));
            }
            operator_seen = true;
            operand_seen = false;
            continue;
        }
        if !operator_seen {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: expected ON, OFF, or Toggle operator"
            )));
        }
        validate_set_ocg_state_group(x, raw, i, dict_name)?;
        operand_seen = true;
    }
    if !operand_seen {
        return Err(Error::Validation(format!(
            "{dict_name}.{entry_name}[{}]: operator has no OCG operands",
            a.len() - 1
        )));
    }
    Ok(())
}

/// `validateSetOCGStateActionDict` (12.6.4.12).
fn validate_set_ocg_state_action_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_set_ocg_state_array(x, d, dict_name)?;
    validate_boolean_entry(x, d, dict_name, "PreserveRB", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `renditionScreenPage`: the page dictionary of the page `page_ref`, which has to be in the
/// page tree.
fn rendition_screen_page(v: &mut Validator<'_>, page_ref: Ref) -> Result<Dict, Error> {
    let page_dict =
        v.x.dereference_dict_ref(page_ref)
            .map_err(|e| e.context("Rendition.AN.P: dereference page"))?
            .cloned();
    let Some(page_dict) = page_dict else {
        return Err(Error::Validation(
            "Rendition.AN.P: page is null".to_string(),
        ));
    };
    validate_name_entry(
        v.x,
        &page_dict,
        "pageDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Page"),
    )?;
    media::validate_rendition_page_membership(v, page_ref.num)?;
    Ok(page_dict)
}

/// `validateRenditionScreenPage`: the screen annotation is in the `Annots` of its page.
fn validate_rendition_screen_page(
    v: &mut Validator<'_>,
    annotation_ref: Ref,
    page_ref: Ref,
) -> Result<(), Error> {
    let page_dict = rendition_screen_page(v, page_ref)?;
    let annots = media::rendition_page_annotations(v, &page_dict, page_ref.num)?;
    if annots.contains(&annotation_ref) {
        return Ok(());
    }
    Err(Error::Validation(
        "Rendition.AN: annotation is not in the page Annots array".to_string(),
    ))
}

/// `validateRenditionScreenCompatibility`: relaxed validation accepts a screen annotation that
/// is a direct dictionary or has no `P` entry.
fn validate_rendition_screen_compatibility(
    v: &mut Validator<'_>,
    d: &Dict,
    page_ref: Option<Ref>,
) -> Result<(), Error> {
    validate_rectangle_entry(v.x, d, "annotDict", "Rect", REQUIRED, Version::V10, None)?;
    if let Some(page_ref) = page_ref {
        rendition_screen_page(v, page_ref)?;
    }
    v.check()
}

/// `validateRenditionScreenTarget` (12.6.4.14): `AN`, the screen annotation.
fn validate_rendition_screen_target(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    required: bool,
) -> Result<(), Error> {
    v.check()?;
    let raw = d.find(b"AN").cloned();
    let annotation_ref = match &raw {
        Some(Object::Ref(r)) => Some(*r),
        _ => None,
    };
    let Some(mut annotation_dict) =
        validate_dict_entry(v.x, d, dict_name, "AN", required, Version::V10, None)?
    else {
        return Ok(());
    };
    annotation::validate_rendition_screen_annotation_structure(v, &mut annotation_dict)
        .map_err(|e| e.context("Rendition.AN"))?;
    store_repaired_dict(v.x, raw.as_ref(), &annotation_dict)?;

    let page_ref = validate_ind_ref_entry(
        v.x,
        &annotation_dict,
        "annotDict",
        "P",
        OPTIONAL,
        Version::V10,
    )?;
    match (annotation_ref, page_ref) {
        (Some(annotation_ref), Some(page_ref)) => {
            validate_rendition_screen_page(v, annotation_ref, page_ref)
        }
        (_, page_ref) => validate_rendition_screen_compatibility(v, &annotation_dict, page_ref),
    }
}

/// `validateRenditionActionDict` (12.6.4.14).
fn validate_rendition_action_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // OP, integer
    let op = validate_integer_entry(
        v.x,
        d,
        dict_name,
        "OP",
        OPTIONAL,
        Version::V14,
        Some(&|i: i64| (0..=4).contains(&i)),
    )?;

    // JS, text string or stream
    validate_java_script(v.x, d, dict_name, "JS")?;

    // R, required for OP 0 and 4, rendition object dict
    let required = matches!(op, Some(0 | 4));
    let raw_r = d.find(b"R").cloned();
    let context = dict_entry_context(dict_name, "R", raw_r.as_ref());
    let d1 = validate_dict_entry(v.x, d, dict_name, "R", required, Version::V14, None)
        .map_err(|e| e.context(context.clone()))?;
    if let Some(d1) = d1 {
        media::validate_rendition_dict(v, &d1, ref_num(raw_r.as_ref()), Version::V14)
            .map_err(|e| e.context(context))?;
    }

    // AN, required for any OP 0..4, indRef of screen annotation dict
    validate_rendition_screen_target(v, d, dict_name, op.is_some())
}

/// `validateTransActionDict` (12.6.4.14).
fn validate_trans_action_dict(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let Some(mut d1) = validate_dict_entry(x, d, dict_name, "Trans", REQUIRED, Version::V10, None)?
    else {
        return Ok(());
    };
    page::validate_transition_dict(x, &mut d1)?;
    store_repaired_dict(x, d.find(b"Trans"), &d1)
}

/// `validateGoTo3DViewActionDict` (12.6.4.15). `owner_obj_nr` is the object number of the
/// action, which keys the reply chain of the target annotation if that is a direct dictionary.
fn validate_go_to_3d_view_action_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
    dict_name: &str,
) -> Result<(), Error> {
    // TA, required, target annotation
    let Some(mut d1) = validate_dict_entry(v.x, d, dict_name, "TA", REQUIRED, Version::V16, None)?
    else {
        return Ok(());
    };
    let raw = d.find(b"TA");
    let ta_obj_nr = match raw {
        Some(Object::Ref(r)) => r.num,
        _ => owner_obj_nr,
    };
    annotation::validate_annotation_dict(v, &mut d1, ta_obj_nr)?;
    store_repaired_dict(v.x, raw, &d1)?;
    validate_name_entry(
        v.x,
        &d1,
        dict_name,
        "Subtype",
        REQUIRED,
        Version::V16,
        Some(&|s: &[u8]| s == b"3D"),
    )?;

    // V, required, the view to use: 3DViewDict or integer or text string or name
    three_d::validate_3d_view_selector(
        v,
        d,
        dict_name,
        "V",
        REQUIRED,
        three_d::ThreeDViewSelectorKind::Action,
    )
}

// ---------------------------------------------------------------------------------------------
// Action dictionaries and chains

/// `validateActionDictCore`: the validator of the action type `s`. Returns whether the action
/// dictionary has to be repaired (see [`validate_go_to_e_action_dict`]).
fn validate_action_dict_core(
    v: &mut Validator<'_>,
    s: &[u8],
    d: &Dict,
    owner_obj_nr: i64,
) -> Result<bool, Error> {
    // The version a type is allowed since, in relaxed mode.
    let since = match s {
        b"GoTo" | b"GoToR" | b"Launch" | b"Thread" | b"URI" | b"SubmitForm" => Version::V10,
        b"GoToE" => Version::V11,
        b"Sound" | b"Movie" | b"Hide" | b"Named" | b"ResetForm" | b"ImportData" | b"JavaScript" => {
            Version::V12
        }
        b"Rendition" => Version::V14,
        b"SetOCGState" | b"Trans" => Version::V15,
        b"GoTo3DView" => Version::V16,
        _ => {
            return Err(Error::Validation(format!(
                "action {0}: unsupported action type \"{0}\"",
                lossy(s)
            )));
        }
    };
    let name = lossy(s);

    v.x.validate_version(&name, since)
        .map_err(|e| e.context(format!("action {name}: version")))?;

    let dn = name.as_str();
    let r = match s {
        b"GoTo" => validate_go_to_action_dict(v.x, d, dn).map(|()| false),
        b"GoToR" => validate_go_to_r_action_dict(v.x, d, dn).map(|()| false),
        b"GoToE" => validate_go_to_e_action_dict(v, d, dn),
        b"Launch" => validate_launch_action_dict(v.x, d, dn).map(|()| false),
        b"Thread" => validate_thread_action_dict(v.x, d, dn).map(|()| false),
        b"URI" => validate_uri_action_dict(v.x, d, dn).map(|()| false),
        b"Sound" => validate_sound_action_dict(v.x, d, dn).map(|()| false),
        b"Movie" => validate_movie_action_dict(v.x, d, dn).map(|()| false),
        b"Hide" => validate_hide_action_dict(v.x, d, dn).map(|()| false),
        b"Named" => validate_named_action_dict(v.x, d, dn).map(|()| false),
        b"SubmitForm" => validate_submit_form_action_dict(v.x, d, dn).map(|()| false),
        b"ResetForm" => validate_reset_form_action_dict(v.x, d, dn).map(|()| false),
        b"ImportData" => validate_import_data_action_dict(v.x, d, dn).map(|()| false),
        b"JavaScript" => validate_java_script_action_dict(v.x, d, dn).map(|()| false),
        b"SetOCGState" => validate_set_ocg_state_action_dict(v.x, d, dn).map(|()| false),
        b"Rendition" => validate_rendition_action_dict(v, d, dn).map(|()| false),
        b"Trans" => validate_trans_action_dict(v.x, d, dn).map(|()| false),
        _ => validate_go_to_3d_view_action_dict(v, d, owner_obj_nr, dn).map(|()| false),
    };
    r.map_err(|e| e.context(format!("action {name}")))
}

/// `model.ActionVisit` (recursion.go:273): the active ancestors and the completed subtrees of
/// one action chain traversal.
#[derive(Debug, Default)]
pub struct ActionVisit {
    ancestors: BTreeSet<i64>,
    validated: BTreeMap<i64, u64>,
}

impl ActionVisit {
    /// `NewActionVisit`.
    pub fn new() -> ActionVisit {
        ActionVisit::default()
    }

    /// `Enter`: rejects action chain cycles. Object number 0 (a direct object) is not tracked.
    ///
    /// # Errors
    /// [`Sentinel::ActionCycle`].
    pub fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::ActionCycle).context(format!("obj#{obj_nr}")));
        }
        Ok(())
    }

    /// `Leave`: leaves the current action chain node.
    pub fn leave(&mut self, obj_nr: i64) {
        if obj_nr != 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `AlreadyValidated`: whether a completed subtree covers the starting `depth`.
    pub fn already_validated(&self, obj_nr: i64, depth: u64) -> bool {
        obj_nr != 0 && self.validated.get(&obj_nr).is_some_and(|d| depth <= *d)
    }

    /// `MarkValidated`: records the successful validation of a subtree from `depth` on.
    pub fn mark_validated(&mut self, obj_nr: i64, depth: u64) {
        if obj_nr == 0 {
            return;
        }
        let d = self.validated.entry(obj_nr).or_insert(depth);
        if depth > *d {
            *d = depth;
        }
    }
}

/// `validateActionDictObjectWithOrigin` (action.go:1370) for the action dictionary `d`, which is
/// the object `raw` (a reference, or `d` itself). Go's `origin` only feeds the active content
/// notices, which are not ported, so it is not a parameter.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_action_dict_object(
    v: &mut Validator<'_>,
    d: &Dict,
    raw: Option<&Object>,
    context: &str,
) -> Result<(), Error> {
    validate_action_dict_object_depth(v, d, raw, context, 0, &mut ActionVisit::new())
}

/// `validateActionDictObjectDepth`: [`validate_action_dict_object`] for an action at `depth` of a
/// chain whose state is `visit` (the JavaScript name tree shares one visit between its values).
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::MaxRecursionDepth`] and [`Sentinel::ActionCycle`] for an
/// action chain that is too deep or circular, [`Error::Validation`] and the errors of the
/// lookups.
pub fn validate_action_dict_object_depth(
    v: &mut Validator<'_>,
    d: &Dict,
    o: Option<&Object>,
    context: &str,
    depth: u64,
    visit: &mut ActionVisit,
) -> Result<(), Error> {
    let obj_nr = ref_num(o);

    v.check()?;
    v.x.check_recursion_depth("action chain", depth)
        .map_err(|e| e.context(context.to_string()))?;
    visit
        .enter(obj_nr)
        .map_err(|e| e.context(object_context(context, o)))?;

    let r = validate_visited_action(v, d, o, obj_nr, context, depth, visit);
    visit.leave(obj_nr);
    r
}

/// The part of [`validate_action_dict_object_depth`] between entering and leaving the node.
fn validate_visited_action(
    v: &mut Validator<'_>,
    d: &Dict,
    o: Option<&Object>,
    obj_nr: i64,
    context: &str,
    depth: u64,
    visit: &mut ActionVisit,
) -> Result<(), Error> {
    // A deeper path needs revalidation to preserve descendant depth checks.
    if visit.already_validated(obj_nr, depth) {
        return Ok(());
    }

    let repaired = validate_action_dict(v, d, obj_nr, depth, visit)
        .map_err(|e| wrap_recursion_error(object_context(context, o), e))?;
    if repaired {
        let mut fixed = d.clone();
        let dest = fixed.0.remove(b"Dest".as_slice()).unwrap_or(Object::Null);
        fixed.0.insert(b"D".to_vec(), dest);
        store_repaired_dict(v.x, o, &fixed)?;
    }
    visit.mark_validated(obj_nr, depth);
    Ok(())
}

/// `validateNextAction`: the `Next` entry, an action dictionary or an array of them.
fn validate_next_action(
    v: &mut Validator<'_>,
    o: &Object,
    depth: u64,
    visit: &mut ActionVisit,
) -> Result<(), Error> {
    if let Ok(d) = v.x.dereference_dict(o).map(|d| d.cloned()) {
        let Some(d) = d else {
            return Ok(());
        };
        return validate_action_dict_object_depth(v, &d, Some(o), "action Next", depth + 1, visit);
    }

    let a =
        v.x.dereference_array(o)
            .map_err(|e| e.context("action Next: expected action dict or array"))?
            .cloned();
    let Some(a) = a else {
        return Ok(());
    };

    for (i, el) in a.iter().enumerate() {
        v.check()?;
        let d =
            v.x.dereference_dict(el)
                .map_err(|e| e.context(format!("action Next[{i}]: dereference dict")))?
                .cloned();
        let Some(d) = d else {
            continue;
        };
        validate_action_dict_object_depth(
            v,
            &d,
            Some(el),
            &format!("action Next[{i}]"),
            depth + 1,
            visit,
        )?;
    }
    Ok(())
}

/// `validateActionDict` (12.6.2, table 193). Returns whether the dictionary has to be repaired.
fn validate_action_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
    depth: u64,
    visit: &mut ActionVisit,
) -> Result<bool, Error> {
    let dict_name = "actionDict";

    // Type, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| matches!(s, b"A" | b"Action")),
    )
    .map_err(|e| e.context("action dictionary Type"))?;

    // S, optional in relaxed mode, name, action Type
    let s = validate_name_entry(v.x, d, dict_name, "S", OPTIONAL, Version::V10, None)
        .map_err(|e| e.context("action dictionary S"))?;

    let mut repaired = false;
    if let Some(s) = s {
        repaired = validate_action_dict_core(v, &s, d, owner_obj_nr)?;
    }

    if let Some(next) = d.find(b"Next") {
        validate_next_action(v, next, depth, visit)?;
    }
    Ok(repaired)
}

/// `validateRootAdditionalActions` (action.go:1495): the `AA` entry of the catalog.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_root_additional_actions(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_additional_actions(v, root, "rootDict", "AA", required, since, "root")
}

/// `validateAdditionalActions`: the additional-actions dictionary `entry_name` of `dict`. The
/// triggers that are allowed depend on `source`: `root`, `page` or `fieldOrAnnot`.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for a trigger that the source does not allow,
/// and the errors of the action validation.
pub fn validate_additional_actions(
    v: &mut Validator<'_>,
    dict: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    source: &str,
) -> Result<(), Error> {
    let Some(d) = validate_dict_entry(v.x, dict, dict_name, entry_name, required, since, None)?
    else {
        return Ok(());
    };

    for (k, val) in d.iter() {
        v.check()?;
        let trigger = lossy(k);

        let allowed = match source {
            "root" => matches!(k.as_slice(), b"WC" | b"WS" | b"DS" | b"WP" | b"DP"),
            "page" => matches!(k.as_slice(), b"O" | b"C"),
            // A terminal form field may be merged with a widget annotation.
            "fieldOrAnnot" => matches!(
                k.as_slice(),
                b"K" | b"F"
                    | b"V"
                    | b"C"
                    | b"E"
                    | b"X"
                    | b"D"
                    | b"U"
                    | b"Fo"
                    | b"Bl"
                    | b"PO"
                    | b"PC"
                    | b"PV"
                    | b"PI"
            ),
            _ => false,
        };
        if !allowed {
            return Err(Error::Validation(format!(
                "additional action {dict_name}.{entry_name}: action {trigger} not allowed for source {source}"
            )));
        }

        let action =
            v.x.dereference_dict(val)
                .map_err(|e| {
                    e.context(format!(
                        "additional action {dict_name}.{entry_name}.{trigger}: dereference dict"
                    ))
                })?
                .cloned();
        let Some(action) = action else {
            continue;
        };

        validate_action_dict_object(
            v,
            &action,
            Some(val),
            &format!("additional action {dict_name}.{entry_name}.{trigger}"),
        )?;
    }
    Ok(())
}

/// `validateAdditionalActionsWithOwner`: [`validate_additional_actions`]. Go's owner object
/// number only labels errors.
///
/// # Errors
/// As [`validate_additional_actions`].
pub fn validate_additional_actions_with_owner(
    v: &mut Validator<'_>,
    dict: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    source: &str,
) -> Result<(), Error> {
    validate_additional_actions(v, dict, dict_name, entry_name, required, since, source)
}

// ---------------------------------------------------------------------------------------------
// Form actions (form_actions.go)

/// `formActionPages`: the pages of the page tree and the page of each widget annotation,
/// built lazily for one AcroForm validation pass.
#[derive(Debug, Clone, Default)]
pub struct FormActionPages {
    /// The page number of each page; `None` until the page tree has been walked.
    pages: Option<BTreeMap<Ref, i64>>,
    /// The first page whose `Annots` array holds the object, by object number.
    widgets: BTreeMap<i64, i64>,
}

/// `withFormActionPages`: starts the form action page state of an AcroForm validation pass.
/// Returns the state it replaces; the caller restores it when the pass is done (Go's context
/// goes out of scope there).
pub fn with_form_action_pages(v: &mut Validator<'_>) -> Option<FormActionPages> {
    v.st.form_action_pages.replace(FormActionPages::default())
}

/// `formActionPages.collectPage`.
fn collect_form_action_page(
    v: &mut Validator<'_>,
    p: &mut FormActionPages,
    page_ref: Ref,
    d: &Dict,
) -> Result<(), Error> {
    let pages = p.pages.get_or_insert_with(BTreeMap::new);
    let page = i64::try_from(pages.len())
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    pages.insert(page_ref, page);

    let annots = d.find(b"Annots").cloned().unwrap_or(Object::Null);
    let annots =
        v.x.dereference_array(&annots)
            .map_err(|e| e.context(format!("form actions: page Annots obj#{}", page_ref.num)))?
            .cloned()
            .unwrap_or_default();
    for value in &annots {
        v.check()?;
        if let Object::Ref(r) = value {
            p.widgets.entry(r.num).or_insert(page);
        }
    }
    Ok(())
}

/// `formActionPages.walk`.
fn walk_form_action_pages(
    v: &mut Validator<'_>,
    p: &mut FormActionPages,
    page_ref: Ref,
    depth: u64,
    visit: &mut PageTreeVisit,
) -> Result<(), Error> {
    v.check()?;
    visit.enter(page_ref.num)?;
    let r = walk_form_action_page_node(v, p, page_ref, depth, visit);
    visit.leave(page_ref.num);
    r
}

fn walk_form_action_page_node(
    v: &mut Validator<'_>,
    p: &mut FormActionPages,
    page_ref: Ref,
    depth: u64,
    visit: &mut PageTreeVisit,
) -> Result<(), Error> {
    let num = page_ref.num;
    let d =
        v.x.dereference_dict_ref(page_ref)
            .map_err(|e| e.context(format!("form actions: page dictionary obj#{num}")))?
            .cloned();
    let Some(d) = d else {
        return Err(Error::Validation(format!(
            "form actions: missing page dictionary obj#{num}"
        )));
    };

    // The page validator treats Catalog.Pages as the root without requiring its Type entry.
    let name = if depth > 0 {
        let (t, _) =
            v.x.dereference_name_entry(&d, b"Type")
                .map_err(|e| e.context(format!("page tree: node obj#{num} Type")))?;
        t.ok_or_else(|| Error::Validation(format!("page tree: node obj#{num} missing Type")))?
    } else {
        b"Pages".to_vec()
    };
    match name.as_slice() {
        b"Page" => return collect_form_action_page(v, p, page_ref, &d),
        b"Pages" => {}
        _ => {
            return Err(Error::Validation(
                "form actions: invalid page tree node type".to_string(),
            ));
        }
    }

    // Match the page-tree validator: only Pages nodes contribute to the depth limit.
    v.x.check_recursion_depth("form action page tree", depth)?;
    let kids = d.find(b"Kids").cloned().unwrap_or(Object::Null);
    let kids = v.x.dereference_array(&kids)?.cloned().unwrap_or_default();
    for value in &kids {
        let Object::Ref(kid) = value else {
            return Err(Error::Validation(
                "form actions: page tree kid is not an indirect reference".to_string(),
            ));
        };
        walk_form_action_pages(v, p, *kid, depth + 1, visit)?;
    }
    Ok(())
}

/// `formActionPages.build`.
fn build_form_action_pages(v: &mut Validator<'_>, p: &mut FormActionPages) -> Result<(), Error> {
    if v.x.root.is_none() && v.x.root_dict.is_none() {
        return Ok(());
    }
    let root = v.x.pages()?;
    let mut next = FormActionPages {
        pages: Some(BTreeMap::new()),
        widgets: BTreeMap::new(),
    };
    walk_form_action_pages(v, &mut next, root, 0, &mut PageTreeVisit::new())?;
    *p = next;
    Ok(())
}

/// `formActionPage`: the page number of the widget `obj_nr` whose dictionary is `d`, 0 if there
/// is none.
fn form_action_page(v: &mut Validator<'_>, obj_nr: i64, d: &Dict) -> Result<i64, Error> {
    v.check()?;
    // Outside of an AcroForm pass the state is a temporary one.
    let in_pass = v.st.form_action_pages.is_some();
    let mut p = v.st.form_action_pages.take().unwrap_or_default();
    let r = form_action_page_in(v, &mut p, obj_nr, d);
    if in_pass {
        v.st.form_action_pages = Some(p);
    }
    r
}

fn form_action_page_in(
    v: &mut Validator<'_>,
    p: &mut FormActionPages,
    obj_nr: i64,
    d: &Dict,
) -> Result<i64, Error> {
    if p.pages.is_none() {
        build_form_action_pages(v, p)?;
    }
    if let Some(&page) = p.widgets.get(&obj_nr)
        && page != 0
    {
        return Ok(page);
    }
    if let Some(Object::Ref(r)) = d.find(b"P") {
        return Ok(p
            .pages
            .as_ref()
            .and_then(|pages| pages.get(r))
            .copied()
            .unwrap_or(0));
    }
    Ok(0)
}

/// `validateFormActionsOnPage`: runs `validate` for the form field or widget `obj_nr`, whose
/// dictionary is `d`. If a terminal node has actions, the current page is the page of the
/// widget while `validate` runs.
///
/// # Errors
/// The errors of `validate`, and of the walk of the page tree.
pub fn validate_form_actions_on_page(
    v: &mut Validator<'_>,
    obj_nr: i64,
    d: &Dict,
    terminal: bool,
    validate: impl FnOnce(&mut Validator<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    if terminal && !d.has_entry(b"A") && !d.has_entry(b"AA") {
        return validate(v);
    }
    let page = if terminal {
        form_action_page(v, obj_nr, d)?
    } else {
        0
    };
    let previous = v.x.cur_page;
    v.x.cur_page = page;
    let r = validate(v);
    v.x.cur_page = previous;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::xref::{Entry, EntryObject};
    use std::sync::atomic::AtomicBool;

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn reference(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn table(max_depth: u64, dicts: &[(i64, Dict)]) -> XRefTable {
        let mut conf = Config::default();
        conf.limits.max_recursion_depth = max_depth;
        let mut x = XRefTable::new(conf);
        x.header_version = Some(Version::V17);
        for (n, d) in dicts {
            x.table.insert(*n, Entry::gen0(Object::Dict(d.clone())));
        }
        x
    }

    fn named_action(next: Option<Object>) -> Dict {
        let mut d = dict(&[("S", name("Named")), ("N", name("FirstPage"))]);
        if let Some(next) = next {
            d.set(b"Next", next);
        }
        d
    }

    fn embedded_target(next: Option<Object>) -> Dict {
        let mut d = dict(&[("R", name("P"))]);
        if let Some(next) = next {
            d.set(b"T", next);
        }
        d
    }

    fn embedded_go_to_action(target: Object) -> Dict {
        dict(&[
            ("S", name("GoToE")),
            ("D", Object::string("destination")),
            ("T", target),
        ])
    }

    fn run(x: &mut XRefTable, d: &Dict, num: i64) -> Result<(), Error> {
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(x, &cancel);
        validate_action_dict_object(&mut v, d, Some(&reference(num)), "test action")
    }

    /// TestValidateActionDictRejectsRecursionDepth
    #[test]
    fn action_dict_rejects_recursion_depth() {
        let dicts = [
            (1, named_action(Some(reference(2)))),
            (2, named_action(Some(reference(3)))),
            (3, named_action(None)),
        ];
        let mut x = table(1, &dicts);
        let err = run(&mut x, &dicts[0].1, 1).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");
    }

    /// TestValidateActionDictRejectsCycles
    #[test]
    fn action_dict_rejects_cycles() {
        let cases = [
            vec![(1, named_action(Some(reference(1))))],
            vec![
                (1, named_action(Some(reference(2)))),
                (2, named_action(Some(reference(1)))),
            ],
            vec![
                (1, named_action(Some(Object::Array(vec![reference(2)])))),
                (2, named_action(Some(reference(1)))),
            ],
        ];
        for dicts in cases {
            let mut x = table(100, &dicts);
            let err = run(&mut x, &dicts[0].1, 1).unwrap_err();
            assert!(err.is(Sentinel::ActionCycle), "{err}");
        }
    }

    /// TestValidateActionDictAllowsSharedSuccessor
    #[test]
    fn action_dict_allows_shared_successor() {
        let dicts = [
            (
                1,
                named_action(Some(Object::Array(vec![reference(2), reference(3)]))),
            ),
            (2, named_action(Some(reference(4)))),
            (3, named_action(Some(reference(4)))),
            (4, named_action(None)),
        ];
        let mut x = table(100, &dicts);
        assert_eq!(run(&mut x, &dicts[0].1, 1), Ok(()));
    }

    fn run_embedded(x: &mut XRefTable, action: &Dict) -> Result<bool, Error> {
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(x, &cancel);
        validate_go_to_e_action_dict(&mut v, action, "GoToE")
    }

    /// TestValidateEmbeddedTargetRejectsRecursionDepth
    #[test]
    fn embedded_target_rejects_recursion_depth() {
        let dicts = [
            (5, embedded_target(Some(reference(6)))),
            (6, embedded_target(Some(reference(7)))),
            (7, embedded_target(None)),
        ];
        let mut x = table(1, &dicts);
        let err = run_embedded(&mut x, &embedded_go_to_action(reference(5))).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");
    }

    /// TestValidateEmbeddedTargetRejectsCycles
    #[test]
    fn embedded_target_rejects_cycles() {
        let cases = [
            vec![(5, embedded_target(Some(reference(5))))],
            vec![
                (5, embedded_target(Some(reference(6)))),
                (6, embedded_target(Some(reference(5)))),
            ],
        ];
        for dicts in cases {
            let mut x = table(100, &dicts);
            let err = run_embedded(&mut x, &embedded_go_to_action(reference(5))).unwrap_err();
            assert!(err.is(Sentinel::TargetCycle), "{err}");
        }
    }

    /// TestValidateEmbeddedTargetAllowsReuse
    #[test]
    fn embedded_target_allows_reuse() {
        let mut x = table(100, &[(5, embedded_target(None))]);
        let action = embedded_go_to_action(reference(5));
        for _ in 0..2 {
            assert_eq!(run_embedded(&mut x, &action), Ok(false));
        }
    }

    /// TestValidateActionNextArrayCancellation, TestValidateEmbeddedTargetCancellation: an
    /// action chain stops at the first cancellation check.
    #[test]
    fn cancellation_stops_the_traversal() {
        let next = Object::Array((2..=5).map(reference).collect());
        let mut dicts = vec![(1, named_action(Some(next)))];
        dicts.extend((2..=5).map(|n| (n, named_action(None))));
        let mut x = table(100, &dicts);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_action_dict_object(&mut v, &dicts[0].1, Some(&reference(1)), "a");
        assert_eq!(err, Err(Error::Cancelled));

        let dicts = [
            (5, embedded_target(Some(reference(6)))),
            (6, embedded_target(None)),
        ];
        let mut x = table(100, &dicts);
        let mut v = Validator::new(&mut x, &cancel);
        let action = embedded_go_to_action(reference(5));
        assert_eq!(
            validate_go_to_e_action_dict(&mut v, &action, "GoToE"),
            Err(Error::Cancelled)
        );
    }

    /// TestSharedActionChecksDeeperPath
    #[test]
    fn shared_action_checks_deeper_path() {
        let mut dicts = vec![
            (
                1,
                named_action(Some(Object::Array(vec![reference(2), reference(4)]))),
            ),
            (2, named_action(Some(reference(3)))),
            (3, named_action(None)),
            (4, named_action(Some(reference(5)))),
            (5, named_action(Some(reference(2)))),
        ];
        let mut x = table(3, &dicts);
        let err = run(&mut x, &dicts[0].1, 1).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");

        dicts[0]
            .1
            .set(b"Next", Object::Array(vec![reference(4), reference(2)]));
        let mut x = table(4, &dicts);
        assert_eq!(run(&mut x, &dicts[0].1, 1), Ok(()));
    }

    /// TestSharedActionPreservesRepair: the first visit repairs a shared action.
    #[test]
    fn shared_action_preserves_repair() {
        let goto_e = dict(&[
            ("S", name("GoToE")),
            ("F", Object::string("other.pdf")),
            ("Dest", Object::string("target")),
        ]);
        let first = named_action(Some(Object::Array(vec![reference(2), reference(2)])));
        let mut x = table(100, &[(1, first.clone()), (2, goto_e)]);
        assert_eq!(run(&mut x, &first, 1), Ok(()));
        let stored = match x.table.get(&2).and_then(|e| e.object.as_ref()) {
            Some(EntryObject::Obj(Object::Dict(d))) => d.clone(),
            other => panic!("{other:?}"),
        };
        assert!(matches!(stored.find(b"D"), Some(Object::String(s)) if s == b"target"));
        assert!(stored.find(b"Dest").is_none());
    }

    /// TestActionValidationStartsFresh
    #[test]
    fn action_validation_starts_fresh() {
        let mut d = named_action(None);
        let mut x = table(100, &[(1, d.clone())]);
        assert_eq!(run(&mut x, &d, 1), Ok(()));
        d.set(b"S", name("InvalidAction"));
        assert!(run(&mut x, &d, 1).is_err());
    }

    #[test]
    fn action_types_and_versions() {
        let mut x = table(100, &[]);
        let uri = dict(&[("S", name("URI")), ("URI", Object::string("https://a.b/"))]);
        assert_eq!(run(&mut x, &uri, 0), Ok(()));
        let missing_uri = dict(&[("S", name("URI"))]);
        assert!(run(&mut x, &missing_uri, 0).is_err());
        // Relaxed validation accepts a missing S.
        assert_eq!(run(&mut x, &dict(&[("Type", name("Action"))]), 0), Ok(()));
        // 3D view actions need 1.6, so a 1.5 document fails on the version.
        x.header_version = Some(Version::V15);
        let rendition_free = dict(&[("S", name("GoTo3DView"))]);
        assert!(run(&mut x, &rendition_free, 0).is_err());
    }

    #[test]
    fn named_actions() {
        let mut x = table(100, &[]);
        for n in ["NextPage", "Print", "ZoomViewOut"] {
            let d = dict(&[("S", name("Named")), ("N", name(n))]);
            assert_eq!(run(&mut x, &d, 0), Ok(()), "{n}");
        }
        let bad = dict(&[("S", name("Named")), ("N", name("Bogus"))]);
        assert!(run(&mut x, &bad, 0).is_err());
    }

    #[test]
    fn hide_action() {
        let mut x = table(100, &[]);
        let ok = dict(&[("S", name("Hide")), ("T", Object::string("field"))]);
        assert_eq!(run(&mut x, &ok, 0), Ok(()));
        let none = dict(&[("S", name("Hide"))]);
        assert!(run(&mut x, &none, 0).is_err());
        let bad = dict(&[("S", name("Hide")), ("T", Object::Integer(1))]);
        assert!(run(&mut x, &bad, 0).is_err());
        let array = dict(&[
            ("S", name("Hide")),
            (
                "T",
                Object::Array(vec![Object::string("a"), Object::Integer(2)]),
            ),
        ]);
        assert!(run(&mut x, &array, 0).is_err());
    }

    #[test]
    fn set_ocg_state_sequences() {
        let mut x = table(100, &[]);
        let action = |state: Vec<Object>| {
            dict(&[("S", name("SetOCGState")), ("State", Object::Array(state))])
        };
        assert!(run(&mut x, &action(vec![]), 0).is_err());
        // An operand before any operator, an unknown operator and a trailing operator.
        let group = Object::Dict(dict(&[
            ("Type", name("OCG")),
            ("Name", Object::string("layer")),
        ]));
        assert!(run(&mut x, &action(vec![group.clone()]), 0).is_err());
        assert!(run(&mut x, &action(vec![name("Flip"), group.clone()]), 0).is_err());
        assert!(run(&mut x, &action(vec![name("ON")]), 0).is_err());
        assert_eq!(
            run(&mut x, &action(vec![name("ON"), group.clone()]), 0),
            Ok(())
        );
        assert!(run(&mut x, &action(vec![name("ON"), name("OFF"), group]), 0).is_err());
    }

    #[test]
    fn movie_time_values() {
        let mut x = table(100, &[]);
        assert_eq!(
            validate_movie_time_value(&mut x, &Object::Integer(3), "t"),
            Ok(())
        );
        assert!(validate_movie_time_value(&mut x, &Object::Integer(-3), "t").is_err());
        assert_eq!(
            validate_movie_time_value(&mut x, &Object::string("12345678"), "t"),
            Ok(())
        );
        assert!(validate_movie_time_value(&mut x, &Object::string("1234567"), "t").is_err());
        assert!(validate_movie_time_value(&mut x, &name("x"), "t").is_err());

        let activation = dict(&[
            (
                "Start",
                Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
            ),
            (
                "FWScale",
                Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
            ),
            (
                "FWPosition",
                Object::Array(vec![Object::Float(0.5), Object::Integer(1)]),
            ),
        ]);
        assert_eq!(validate_movie_activation_dict(&mut x, &activation), Ok(()));
        let out_of_range = dict(&[(
            "FWPosition",
            Object::Array(vec![Object::Float(0.5), Object::Integer(2)]),
        )]);
        assert!(validate_movie_activation_dict(&mut x, &out_of_range).is_err());
    }

    #[test]
    fn additional_actions_by_source() {
        let aa = dict(&[("O", Object::Dict(named_action(None)))]);
        let page = dict(&[("AA", Object::Dict(aa))]);
        let mut x = table(100, &[]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let r = validate_additional_actions(
            &mut v,
            &page,
            "pageDict",
            "AA",
            OPTIONAL,
            Version::V12,
            "page",
        );
        assert_eq!(r, Ok(()));
        let r = validate_additional_actions(
            &mut v,
            &page,
            "rootDict",
            "AA",
            OPTIONAL,
            Version::V12,
            "root",
        );
        assert!(r.is_err());
    }

    #[test]
    fn form_actions_set_the_current_page_for_the_closure() {
        let mut x = table(100, &[]);
        x.cur_page = 5;
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);

        // A non-terminal node runs on page 0.
        let d = dict(&[("A", Object::Dict(named_action(None)))]);
        let r = validate_form_actions_on_page(&mut v, 1, &d, false, |v| {
            assert_eq!(v.x.cur_page, 0);
            Ok(())
        });
        assert_eq!(r, Ok(()));
        assert_eq!(v.x.cur_page, 5);

        // A terminal node without actions leaves the page alone.
        let plain = Dict::default();
        let r = validate_form_actions_on_page(&mut v, 1, &plain, true, |v| {
            assert_eq!(v.x.cur_page, 5);
            Err(Error::Validation("x".to_string()))
        });
        assert_eq!(r, Err(Error::Validation("x".to_string())));
        assert_eq!(v.x.cur_page, 5);

        // The previous state is returned and can be restored.
        assert!(with_form_action_pages(&mut v).is_none());
        assert!(with_form_action_pages(&mut v).is_some());
    }
}
