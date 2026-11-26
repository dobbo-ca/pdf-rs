//! Media clips, media players, play parameters and renditions (Go: validate/media.go,
//! mediaValidation.go, rendition.go, and `RenditionVisit` of model/recursion.go:327).
//!
//! What other packages call: [`validate_rendition_dict`] (actions, name trees),
//! [`validate_media_object`] (3D streams, rich media), and the rendition page index functions
//! [`record_rendition_page`] (page tree walk), [`validate_rendition_page_membership`],
//! [`rendition_page_annotations`] and [`clear_rendition_page_annotations`] (rendition actions,
//! annotations).
//!
//! Differences from Go:
//!
//! * Validation is relaxed, always (see `object.rs`). The `ValidationError` object numbers are
//!   dropped, and so are `mediaEntryError` (which only sets one) and `validate*Context` pairs
//!   without the context (`validateMediaCriteriaDict`, `validateMediaPlayersDict`, ...: Go
//!   calls them from tests only).
//! * The link collection (`collectLinkTarget`, `collectFileSpecificationTarget`) is dead
//!   (`ValidateLinks` is false), and so is its `isEmbeddedFileSpecification` predicate.
//! * The rendition page index always exists in a validation pass, so the branch of
//!   `validateRenditionPageMembership` for "no enclosing page walk" (`PageNumber`) is not ported.
//! * `validateFileSpecificationOrFormObject` is WP 17's [`super::file_spec`] function.

use super::Validator;
use super::file_spec::validate_file_specification_or_form_object;
use super::object::{
    OPTIONAL, REQUIRED, dict_entry_context, validate_array_entry, validate_boolean_entry,
    validate_dict_entry, validate_entry, validate_integer_array_entry, validate_integer_entry,
    validate_multi_language_text_entry, validate_name_array_entry, validate_name_entry,
    validate_number_array_entry, validate_number_entry, validate_string_array_entry,
    validate_string_entry,
};
use crate::error::{Error, Sentinel};
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, Ref};
use std::collections::{BTreeMap, BTreeSet};

/// A check that a name is `want`.
fn is_name(want: &'static [u8]) -> impl Fn(&[u8]) -> bool {
    move |s: &[u8]| s == want
}

/// `d[key]`: the entry stored under exactly `key`.
fn raw_entry<'a>(d: &'a Dict, key: &str) -> Option<&'a Object> {
    d.0.get(key.as_bytes())
}

/// `validationObjectNumber(0, o)`: the object number of a reference, else 0.
fn ref_num(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

/// An error for an entry that the check `validate` of Go's entry validators refused.
fn invalid_entry(dict_name: &str, entry_name: &str) -> Error {
    Error::Validation(format!(
        "dict={dict_name} entry={entry_name} invalid dict entry"
    ))
}

/// `model.WrapRecursionError`: the context is not added to a recursion limit error.
pub(super) fn wrap_recursion_error(context: String, e: Error) -> Error {
    if e.is(Sentinel::MaxRecursionDepth) {
        e
    } else {
        e.context(context)
    }
}

/// `validateNonNegativeIntegerArray`.
fn non_negative_integer_array(x: &mut XRefTable, a: &[Object], length: usize) -> bool {
    a.len() == length
        && a.iter()
            .all(|raw| matches!(x.dereference(raw), Ok(Object::Integer(i)) if *i >= 0))
}

/// `mediaNumberInRange`.
fn media_number_in_range(o: &Object, min: f64, max: f64) -> bool {
    match o {
        Object::Integer(i) => (min..=max).contains(&(*i as f64)),
        Object::Float(f) => (min..=max).contains(f),
        _ => false,
    }
}

/// `validateNumberArrayRange`.
fn number_array_range(x: &mut XRefTable, a: &[Object], length: usize, min: f64, max: f64) -> bool {
    a.len() == length
        && a.iter().all(|raw| {
            x.dereference(raw)
                .is_ok_and(|o| media_number_in_range(o, min, max))
        })
}

/// `validateIntegerArrayEntry` with the check `validateNonNegativeIntegerArray(.., length)`.
fn validate_non_negative_integer_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    length: usize,
) -> Result<(), Error> {
    if let Some(a) =
        validate_integer_array_entry(x, d, dict_name, entry_name, required, since, None)?
        && !non_negative_integer_array(x, &a, length)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(())
}

/// `validateMinimumBitDepthDict` (table 269).
fn validate_minimum_bit_depth_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "minBitDepthDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MinBitDepth")),
    )?;

    // V, required, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "V",
        REQUIRED,
        since,
        Some(&|i: i64| i >= 0),
    )?;

    // M, optional, integer
    validate_integer_entry(x, d, dict_name, "M", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMinimumScreenSizeDict` (table 270).
fn validate_minimum_screen_size_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "minScreenSizeDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MinScreenSize")),
    )?;

    // V, required, integer array, length 2
    validate_non_negative_integer_array_entry(x, d, dict_name, "V", REQUIRED, since, 2)?;

    // M, optional, integer
    validate_integer_entry(x, d, dict_name, "M", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateSoftwareIdentifierDict` (table 292).
fn validate_software_identifier_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "swIdDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"SoftwareIdentifier")),
    )?;

    // U, required, ASCII string
    validate_string_entry(x, d, dict_name, "U", REQUIRED, since, None)?;

    // L, optional, array
    validate_array_entry(x, d, dict_name, "L", OPTIONAL, since, None)?;

    // LI, optional, boolean
    validate_boolean_entry(x, d, dict_name, "LI", OPTIONAL, since, None)?;

    // H, optional, array
    validate_array_entry(x, d, dict_name, "H", OPTIONAL, since, None)?;

    // HI, optional, boolean
    validate_boolean_entry(x, d, dict_name, "HI", OPTIONAL, since, None)?;

    // OS, optional, array
    validate_string_array_entry(x, d, dict_name, "OS", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMediaCriteriaDictEntryD`: the minimum bit depth dictionary.
fn validate_media_criteria_dict_entry_d(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let context = || dict_entry_context(dict_name, "D", raw_entry(d, "D"));
    let d1 = validate_dict_entry(x, d, dict_name, "D", required, since, None)
        .map_err(|e| e.context(context()))?;
    if let Some(d1) = d1 {
        validate_minimum_bit_depth_dict(x, &d1, since).map_err(|e| e.context(context()))?;
    }
    Ok(())
}

/// `validateMediaCriteriaDictEntryZ`: the minimum screen size dictionary.
fn validate_media_criteria_dict_entry_z(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let context = || dict_entry_context(dict_name, "Z", raw_entry(d, "Z"));
    let d1 = validate_dict_entry(x, d, dict_name, "Z", required, since, None)
        .map_err(|e| e.context(context()))?;
    if let Some(d1) = d1 {
        validate_minimum_screen_size_dict(x, &d1, since).map_err(|e| e.context(context()))?;
    }
    Ok(())
}

/// `validateMediaCriteriaDictEntryVContext`: the array of software identifier dictionaries.
fn validate_media_criteria_dict_entry_v(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let a = validate_array_entry(v.x, d, dict_name, "V", required, since, None)
        .map_err(|e| e.context(format!("{dict_name}.V")))?;
    let Some(a) = a else {
        return Ok(());
    };

    for (i, raw) in a.iter().enumerate() {
        v.check()
            .map_err(|e| e.context(format!("{dict_name}.V[{i}]")))?;

        if raw.is_null() {
            continue;
        }

        let d1 =
            v.x.dereference_dict(raw)
                .map_err(|e| {
                    e.context(format!(
                        "{dict_name}.V[{i}]: dereference software identifier dict"
                    ))
                })?
                .cloned();

        if let Some(d1) = d1 {
            validate_software_identifier_dict(v.x, &d1, since)
                .map_err(|e| e.context(format!("{dict_name}.V[{i}]")))?;
        }
    }

    Ok(())
}

/// `validateMediaCriteriaDictContext` (table 268).
fn validate_media_criteria_dict_context(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaCritDict";

    // Type, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaCriteria")),
    )?;

    // A, C, O, S, optional, boolean
    for name in ["A", "C", "O", "S"] {
        validate_boolean_entry(v.x, d, dict_name, name, OPTIONAL, since, None)?;
    }

    // R, optional, integer
    validate_integer_entry(v.x, d, dict_name, "R", OPTIONAL, since, None)?;

    // D, optional, dict
    validate_media_criteria_dict_entry_d(v.x, d, dict_name, OPTIONAL, since)?;

    // Z, optional, dict
    validate_media_criteria_dict_entry_z(v.x, d, dict_name, OPTIONAL, since)?;

    // V, optional, array
    validate_media_criteria_dict_entry_v(v, d, dict_name, OPTIONAL, since)?;

    // P, optional, array
    validate_name_array_entry(
        v.x,
        d,
        dict_name,
        "P",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 1 || a.len() == 2),
    )?;

    // L, optional, array
    validate_string_array_entry(v.x, d, dict_name, "L", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMediaPermissionsDict` (table 275): the entry `P` of `d`.
fn validate_media_permissions_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(x, d, dict_name, "P", OPTIONAL, since, None)? else {
        return Ok(());
    };

    let dict_name = "mediaPermissionDict";

    // Type, optional, name
    validate_name_entry(
        x,
        &d1,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaPermissions")),
    )?;

    // TF, optional, ASCII string
    validate_string_entry(
        x,
        &d1,
        dict_name,
        "TF",
        OPTIONAL,
        since,
        Some(&|s: &str| matches!(s, "TEMPNEVER" | "TEMPEXTRACT" | "TEMPACCESS" | "TEMPALWAYS")),
    )?;
    Ok(())
}

/// `validateMediaPlayerInfoDict` (table 291).
fn validate_media_player_info_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaPlayerInfoDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaPlayerInfo")),
    )?;

    // PID, required, software identifier dict
    if let Some(d1) = validate_dict_entry(x, d, dict_name, "PID", REQUIRED, since, None)? {
        validate_software_identifier_dict(x, &d1, since)?;
    }

    // MH, optional, dict
    validate_dict_entry(x, d, dict_name, "MH", OPTIONAL, since, None)?;

    // BE, optional, dict
    validate_dict_entry(x, d, dict_name, "BE", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMediaPlayerInfoArray`: the number of media player info dictionaries in `entry_name`.
fn validate_media_player_info_array(
    v: &mut Validator<'_>,
    d: &Dict,
    entry_name: &str,
    since: Version,
) -> Result<usize, Error> {
    let dict_name = "mediaPlayersDict";

    let a = validate_array_entry(v.x, d, dict_name, entry_name, OPTIONAL, since, None)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}")))?;
    let Some(a) = a else {
        return Ok(0);
    };

    for (i, raw) in a.iter().enumerate() {
        v.check()
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?;
        let missing = || {
            Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: missing media player info dict"
            ))
        };
        if raw.is_null() {
            return Err(missing());
        }

        let d1 =
            v.x.dereference_dict(raw)
                .map_err(|e| {
                    e.context(format!(
                        "{dict_name}.{entry_name}[{i}]: dereference media player info dict"
                    ))
                })?
                .cloned();
        let Some(d1) = d1 else {
            return Err(missing());
        };

        validate_media_player_info_dict(v.x, &d1, since)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?;
    }

    Ok(a.len())
}

/// `validateMediaPlayersDictContext` (13.2.7.2): whether the dictionary has a player that can
/// play (`MU` or `A`).
fn validate_media_players_dict_context(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<bool, Error> {
    let dict_name = "mediaPlayersDict";

    // Type, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaPlayers")),
    )?;

    // MU, optional, array of media player info dicts
    let mu_count = validate_media_player_info_array(v, d, "MU", since)?;

    // A, optional, array of media player info dicts
    let a_count = validate_media_player_info_array(v, d, "A", since)?;

    // NU, optional, array of media player info dicts
    validate_media_player_info_array(v, d, "NU", since)?;

    Ok(mu_count > 0 || a_count > 0)
}

/// `validateFileSpecOrFormXObjectEntry`: whether the entry is a file specification (else it is
/// a form XObject).
fn validate_file_spec_or_form_x_object_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<bool, Error> {
    let raw = raw_entry(d, entry_name).cloned();
    let context = || dict_entry_context(dict_name, entry_name, raw.as_ref());
    let o = validate_entry(v.x, d, dict_name, entry_name, required, since)
        .map_err(|e| e.context(context()))?;
    let (Some(_), Some(raw)) = (o, raw.as_ref()) else {
        return Ok(false);
    };

    validate_file_specification_or_form_object(v, raw).map_err(|e| e.context(context()))
}

/// `validateMediaClipPlayers`: `PL`.
fn validate_media_clip_players(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    let d1 = validate_dict_entry(v.x, d, dict_name, "PL", OPTIONAL, since, None)
        .map_err(|e| e.context(format!("{dict_name}.PL")))?;
    let Some(d1) = d1 else {
        return Ok(());
    };
    validate_media_players_dict_context(v, &d1, since)
        .map_err(|e| e.context(format!("{dict_name}.PL")))?;
    Ok(())
}

/// `validateMediaClipContentType`: `CT`, which a form XObject does not have.
fn validate_media_clip_content_type(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    file_spec: bool,
    since: Version,
) -> Result<(), Error> {
    if !file_spec {
        let raw = raw_entry(d, "CT").cloned().unwrap_or(Object::Null);
        if !x.dereference(&raw)?.is_null() {
            return Err(Error::Validation(format!(
                "{}: not allowed for a Form XObject",
                dict_entry_context(dict_name, "CT", Some(&raw))
            )));
        }
        return Ok(());
    }
    validate_string_entry(x, d, dict_name, "CT", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMediaClipBaseURL`: the base URL of `MH` or `BE`.
fn validate_media_clip_base_url(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, OPTIONAL, since, None)? else {
        return Ok(());
    };
    // BU, optional, ASCII string
    validate_string_entry(x, &d1, "", "BU", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateMediaClipDataDict` (13.2.4.2).
fn validate_media_clip_data_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaClipDataDict";

    // D, required, file specification or stream
    let file_spec =
        validate_file_spec_or_form_x_object_entry(v, d, dict_name, "D", REQUIRED, since)?;

    // CT, optional, ASCII string
    validate_media_clip_content_type(v.x, d, dict_name, file_spec, since)?;

    // P, optional, media permissions dict
    validate_media_permissions_dict(v.x, d, dict_name, since)?;

    // Alt, optional, string array
    validate_multi_language_text_entry(v.x, d, dict_name, "Alt", since)?;

    // PL, optional, media players dict
    validate_media_clip_players(v, d, dict_name, since)?;

    // MH, optional, dict
    validate_media_clip_base_url(v.x, d, dict_name, "MH", since)?;

    // BE, optional, dict
    validate_media_clip_base_url(v.x, d, dict_name, "BE", since)
}

/// `validateTimespanDict` (13.2.6.3, table 289).
fn validate_timespan_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "timespanDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"Timespan")),
    )?;

    // S, required, name
    validate_name_entry(x, d, dict_name, "S", REQUIRED, since, Some(&is_name(b"S")))?;

    // V, required, number
    validate_number_entry(
        x,
        d,
        dict_name,
        "V",
        REQUIRED,
        since,
        Some(&|f: f64| f >= 0.0),
    )?;
    Ok(())
}

/// `validateMediaOffsetDict` (13.2.6.2).
fn validate_media_offset_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "mediaOffsetDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaOffset")),
    )?;

    // S, required, name
    let sub_type = validate_name_entry(
        x,
        d,
        dict_name,
        "S",
        REQUIRED,
        since,
        Some(&|s: &[u8]| matches!(s, b"T" | b"F" | b"M")),
    )?
    .unwrap_or_default();

    match sub_type.as_slice() {
        b"T" => {
            let d1 = validate_dict_entry(x, d, dict_name, "T", REQUIRED, since, None)?;
            if let Some(d1) = d1 {
                validate_timespan_dict(x, &d1, since)?;
            }
        }
        b"F" => {
            validate_integer_entry(
                x,
                d,
                dict_name,
                "F",
                REQUIRED,
                since,
                Some(&|i: i64| i >= 0),
            )?;
        }
        b"M" => {
            validate_string_entry(x, d, dict_name, "M", REQUIRED, since, None)?;
        }
        _ => {}
    }

    Ok(())
}

/// `validateMediaClipSectionDictMHBE`: the media offsets `B` and `E`.
fn validate_media_clip_section_dict_mhbe(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaClipSectionMHBE";

    for entry_name in ["B", "E"] {
        let d1 = validate_dict_entry(x, d, dict_name, entry_name, OPTIONAL, since, None)?;
        if let Some(d1) = d1 {
            validate_media_offset_dict(x, &d1, since)?;
        }
    }

    Ok(())
}

/// `mediaClipTraversal`: the media clip objects that are being validated.
#[derive(Debug, Default)]
struct MediaClipTraversal {
    ancestors: BTreeSet<i64>,
}

impl MediaClipTraversal {
    /// `enter`: object numbers up to 0 (direct objects) are not tracked.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr <= 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::MediaClipCycle));
        }
        Ok(())
    }

    fn leave(&mut self, obj_nr: i64) {
        if obj_nr > 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `validateEntry`: the media clip dictionary in the entry `entry_name` of `d`.
    fn validate_entry(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        required: bool,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        let raw = d.find(entry_name.as_bytes()).cloned();
        let context = || dict_entry_context(dict_name, entry_name, raw.as_ref());
        v.check().map_err(|e| e.context(context()))?;

        let mut clip_obj_nr = 0;
        if let Some(raw) = raw.as_ref().filter(|o| !o.is_null()) {
            v.x.check_recursion_depth("MediaClip chain", depth)
                .map_err(|e| e.context(context()))?;
            clip_obj_nr = ref_num(raw);
            self.enter(clip_obj_nr).map_err(|e| e.context(context()))?;
        }

        let r = self
            .validate_clip(v, d, dict_name, entry_name, required, since, depth)
            .map_err(|e| e.context(context()));
        self.leave(clip_obj_nr);
        r
    }

    fn validate_clip(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        required: bool,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, required, since, None)?;
        if let Some(d1) = d1 {
            self.validate_dict(v, &d1, since, depth)?;
        }
        Ok(())
    }

    /// `validateSectionDict` (13.2.4.3).
    fn validate_section_dict(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        let dict_name = "mediaClipSectionDict";

        // D, required, media clip dict
        self.validate_entry(v, d, dict_name, "D", REQUIRED, since, depth + 1)?;

        // Alt, optional, string array
        validate_multi_language_text_entry(v.x, d, dict_name, "Alt", since)?;

        // MH, BE, optional, dict
        for entry_name in ["MH", "BE"] {
            let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, OPTIONAL, since, None)?;
            if let Some(d1) = d1 {
                validate_media_clip_section_dict_mhbe(v.x, &d1, since)?;
            }
        }

        Ok(())
    }

    /// `validateDict` (13.2.4).
    fn validate_dict(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        let dict_name = "mediaClipDict";

        // Type, optional, name
        validate_name_entry(
            v.x,
            d,
            dict_name,
            "Type",
            OPTIONAL,
            since,
            Some(&is_name(b"MediaClip")),
        )?;

        // S, required, name
        let sub_type = validate_name_entry(
            v.x,
            d,
            dict_name,
            "S",
            REQUIRED,
            since,
            Some(&|s: &[u8]| s == b"MCD" || s == b"MCS"),
        )?
        .unwrap_or_default();

        // N, optional, text string
        validate_string_entry(v.x, d, dict_name, "N", OPTIONAL, since, None)?;

        if sub_type == b"MCD" {
            validate_media_clip_data_dict(v, d, since)
                .map_err(|e| e.context(format!("{dict_name} data")))?;
        }

        if sub_type == b"MCS" {
            self.validate_section_dict(v, d, since, depth)
                .map_err(|e| e.context(format!("{dict_name} section")))?;
        }

        Ok(())
    }
}

/// `validateMediaDurationDict`.
fn validate_media_duration_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "mediaDurationDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaDuration")),
    )?;

    // S, required, name
    let s = validate_name_entry(
        x,
        d,
        dict_name,
        "S",
        REQUIRED,
        since,
        Some(&|s: &[u8]| matches!(s, b"I" | b"F" | b"T")),
    )?
    .unwrap_or_default();

    // T is ignored for intrinsic and infinite durations (see table 281).
    if s != b"T" {
        return Ok(());
    }

    // T, required, timespan dict
    if let Some(d1) = validate_dict_entry(x, d, dict_name, "T", REQUIRED, since, None)? {
        validate_timespan_dict(x, &d1, since)?;
    }
    Ok(())
}

/// `validateMediaPlayParamsMHBEDict`.
fn validate_media_play_params_mhbe_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaPlayParamsMHBEDict";

    // V, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "V",
        OPTIONAL,
        since,
        Some(&|i: i64| i >= 0),
    )?;

    // C, optional, boolean
    validate_boolean_entry(x, d, dict_name, "C", OPTIONAL, since, None)?;

    // F, optional, integer. Unrecognized values follow the MH/BE viability rules in table 280.
    validate_integer_entry(x, d, dict_name, "F", OPTIONAL, since, None)?;

    // D, optional, media duration dict
    if let Some(d1) = validate_dict_entry(x, d, dict_name, "D", OPTIONAL, since, None)? {
        validate_media_duration_dict(x, &d1, since)?;
    }

    // A, optional, boolean
    validate_boolean_entry(x, d, dict_name, "A", OPTIONAL, since, None)?;

    // RC, optional, number
    validate_number_entry(
        x,
        d,
        dict_name,
        "RC",
        OPTIONAL,
        since,
        Some(&|f: f64| f >= 0.0),
    )?;
    Ok(())
}

/// `validateMediaPlayParamsDict` (13.2.5): whether the players dictionary has a player that can
/// play.
fn validate_media_play_params_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<bool, Error> {
    let dict_name = "mediaPlayParamsDict";

    // Type, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaPlayParams")),
    )?;

    // PL, optional, media players dict
    let mut has_input_player = false;
    if let Some(d1) = validate_dict_entry(v.x, d, dict_name, "PL", OPTIONAL, since, None)? {
        has_input_player = validate_media_players_dict_context(v, &d1, since)?;
    }

    // MH, BE, optional, dict
    for entry_name in ["MH", "BE"] {
        let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, OPTIONAL, since, None)?;
        if let Some(d1) = d1 {
            validate_media_play_params_mhbe_dict(v.x, &d1, since)?;
        }
    }

    Ok(has_input_player)
}

/// `validateFloatingWindowsParameterDict` (table 284).
fn validate_floating_windows_parameter_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "floatWinParamsDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"FWParams")),
    )?;

    // D, required, array of integers
    validate_non_negative_integer_array_entry(x, d, dict_name, "D", REQUIRED, since, 2)?;

    // RT, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "RT",
        OPTIONAL,
        since,
        Some(&|i: i64| (0..=3).contains(&i)),
    )?;

    // P, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "P",
        OPTIONAL,
        since,
        Some(&|i: i64| (0..=8).contains(&i)),
    )?;

    // O, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "O",
        OPTIONAL,
        since,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // T, optional, boolean
    validate_boolean_entry(x, d, dict_name, "T", OPTIONAL, since, None)?;

    // UC, optional, boolean
    validate_boolean_entry(x, d, dict_name, "UC", OPTIONAL, since, None)?;

    // R, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "R",
        OPTIONAL,
        since,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // TT, optional, string array
    validate_multi_language_text_entry(x, d, dict_name, "TT", since)
}

/// `validateScreenParametersMHBEDict`.
fn validate_screen_parameters_mhbe_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "screenParmsMHBEDict";

    // W, optional, integer
    let w = validate_integer_entry(
        x,
        d,
        dict_name,
        "W",
        OPTIONAL,
        since,
        Some(&|i: i64| (0..=3).contains(&i)),
    )?
    .unwrap_or(3);

    // B, optional, array of 3 numbers
    if let Some(a) = validate_number_array_entry(x, d, dict_name, "B", OPTIONAL, since, None)?
        && !number_array_range(x, &a, 3, 0.0, 1.0)
    {
        return Err(invalid_entry(dict_name, "B"));
    }

    // O, optional, number
    validate_number_entry(
        x,
        d,
        dict_name,
        "O",
        OPTIONAL,
        since,
        Some(&|f: f64| (0.0..=1.0).contains(&f)),
    )?;

    // M, optional, integer
    validate_integer_entry(x, d, dict_name, "M", OPTIONAL, since, None)?;

    // F, required if W == 0, floating windows parameter dict
    if let Some(d1) = validate_dict_entry(x, d, dict_name, "F", w == 0, since, None)? {
        validate_floating_windows_parameter_dict(x, &d1, since)?;
    }
    Ok(())
}

/// `validateScreenParametersDict`.
fn validate_screen_parameters_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "screenParmsDict";

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"MediaScreenParams")),
    )?;

    // MH, BE, optional, dict
    for entry_name in ["MH", "BE"] {
        let d1 = validate_dict_entry(x, d, dict_name, entry_name, OPTIONAL, since, None)?;
        if let Some(d1) = d1 {
            validate_screen_parameters_mhbe_dict(x, &d1, since)?;
        }
    }

    Ok(())
}

/// `validateMediaRenditionDict` (table 271).
fn validate_media_rendition_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "mediaRendDict";

    // C, optional, dict
    MediaClipTraversal::default().validate_entry(v, d, dict_name, "C", OPTIONAL, since, 0)?;
    let raw_clip = d.find(b"C").cloned().filter(|o| !o.is_null());
    let mut has_clip = raw_clip.is_some();
    if let Some(raw) = &raw_clip {
        let clip_is_null =
            v.x.dereference(raw)
                .map_err(|e| e.context(dict_entry_context(dict_name, "C", Some(raw))))?
                .is_null();
        has_clip = !clip_is_null;
    }

    // P, required if C not present, dict
    let raw = raw_entry(d, "P").cloned();
    let context = || dict_entry_context(dict_name, "P", raw.as_ref());
    let d1 = validate_dict_entry(v.x, d, dict_name, "P", !has_clip, since, None)
        .map_err(|e| e.context(context()))?;
    let mut has_input_player = false;
    if let Some(d1) = d1 {
        has_input_player =
            validate_media_play_params_dict(v, &d1, since).map_err(|e| e.context(context()))?;
    }
    if !has_clip && !has_input_player {
        return Err(Error::Validation(format!(
            "{dict_name}.P.PL: missing non-empty MU or A player array"
        )));
    }

    // SP, optional, dict
    let raw = raw_entry(d, "SP").cloned();
    let context = || dict_entry_context(dict_name, "SP", raw.as_ref());
    let d1 = validate_dict_entry(v.x, d, dict_name, "SP", OPTIONAL, since, None)
        .map_err(|e| e.context(context()))?;
    if let Some(d1) = d1 {
        validate_screen_parameters_dict(v.x, &d1, since).map_err(|e| e.context(context()))?;
    }

    Ok(())
}

/// `RenditionVisit`: the active ancestors and the completed subtrees of one selector rendition
/// traversal.
#[derive(Debug, Default)]
struct RenditionVisit {
    ancestors: BTreeSet<i64>,
    /// The depth at which each object number completed.
    validated: BTreeMap<i64, u64>,
}

impl RenditionVisit {
    /// `Enter`: object number 0 (a direct dictionary) is not tracked.
    ///
    /// # Errors
    /// [`Sentinel::RenditionCycle`] with the object number as context.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::RenditionCycle).context(format!("obj#{obj_nr}")));
        }
        Ok(())
    }

    fn leave(&mut self, obj_nr: i64) {
        if obj_nr != 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `AlreadyValidated`: a completed subtree covers the requested starting depth.
    fn already_validated(&self, obj_nr: i64, depth: u64) -> bool {
        obj_nr != 0 && self.validated.get(&obj_nr).is_some_and(|d| depth <= *d)
    }

    /// `MarkValidated`.
    fn mark_validated(&mut self, obj_nr: i64, depth: u64) {
        if obj_nr == 0 {
            return;
        }
        let previous = self.validated.entry(obj_nr).or_insert(depth);
        if depth > *previous {
            *previous = depth;
        }
    }
}

/// `validateSelectorRenditionDict` (table 272).
fn validate_selector_rendition_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
    depth: u64,
    visit: &mut RenditionVisit,
) -> Result<(), Error> {
    let dict_name = "selectorRendDict";

    let a = validate_array_entry(v.x, d, dict_name, "R", REQUIRED, since, None)
        .map_err(|e| e.context(format!("{dict_name}.R")))?;
    let Some(a) = a else {
        return Ok(());
    };

    for (i, raw) in a.iter().enumerate() {
        v.check()?;

        if raw.is_null() {
            continue;
        }

        let context = || super::object::object_context(&format!("{dict_name}.R[{i}]"), Some(raw));

        let d1 =
            v.x.dereference_dict(raw)
                .map_err(|e| e.context(format!("{}: dereference rendition dict", context())))?
                .cloned();
        let Some(d1) = d1 else {
            continue;
        };

        validate_rendition_dict_depth(v, &d1, ref_num(raw), since, depth + 1, visit)
            .map_err(|e| wrap_recursion_error(context(), e))?;
    }

    Ok(())
}

/// `validateRenditionDictEntryMH` and `validateRenditionDictEntryBE`: the media criteria `C` of
/// the dictionary `entry_name` (`MH` or `BE`).
fn validate_rendition_dict_entry_mhbe(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    let d1 = validate_dict_entry(v.x, d, dict_name, entry_name, OPTIONAL, since, None)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}")))?;

    if let Some(d1) = d1 {
        let sub_name = format!("{entry_name}Dict");
        let d2 = validate_dict_entry(v.x, &d1, &sub_name, "C", OPTIONAL, since, None)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}.C")))?;

        if let Some(d2) = d2 {
            validate_media_criteria_dict_context(v, &d2, since)
                .map_err(|e| e.context(format!("{dict_name}.{entry_name}.C")))?;
        }
    }

    Ok(())
}

/// `validateRenditionDict` (13.2.3): the rendition dictionary `d`, which is the object
/// `owner_obj_nr` (0 for a direct dictionary).
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::MaxRecursionDepth`] and [`Sentinel::RenditionCycle`] for a
/// selector rendition graph that is too deep or circular, [`Error::Validation`] and the errors
/// of the lookups.
pub fn validate_rendition_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
    since: Version,
) -> Result<(), Error> {
    validate_rendition_dict_depth(v, d, owner_obj_nr, since, 0, &mut RenditionVisit::default())
}

fn validate_rendition_dict_depth(
    v: &mut Validator<'_>,
    d: &Dict,
    owner_obj_nr: i64,
    since: Version,
    depth: u64,
    visit: &mut RenditionVisit,
) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth("rendition graph", depth)?;
    visit.enter(owner_obj_nr)?;

    let r = if visit.already_validated(owner_obj_nr, depth) {
        Ok(())
    } else {
        validate_rendition_dict_body(v, d, since, depth, visit)
            .map(|()| visit.mark_validated(owner_obj_nr, depth))
    };

    visit.leave(owner_obj_nr);
    r
}

fn validate_rendition_dict_body(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
    depth: u64,
    visit: &mut RenditionVisit,
) -> Result<(), Error> {
    let dict_name = "renditionDict";

    // Type, optional, name
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&is_name(b"Rendition")),
    )
    .map_err(|e| e.context(format!("{dict_name}.Type")))?;

    // S, required, name
    let rendition_type = validate_name_entry(
        v.x,
        d,
        dict_name,
        "S",
        REQUIRED,
        since,
        Some(&|s: &[u8]| s == b"MR" || s == b"SR"),
    )
    .map_err(|e| e.context(format!("{dict_name}.S")))?
    .unwrap_or_default();

    // N, optional, text string
    validate_string_entry(v.x, d, dict_name, "N", OPTIONAL, since, None)
        .map_err(|e| e.context(format!("{dict_name}.N")))?;

    // MH, BE, optional, dict
    validate_rendition_dict_entry_mhbe(v, d, dict_name, "MH", since)?;
    validate_rendition_dict_entry_mhbe(v, d, dict_name, "BE", since)?;

    if rendition_type == b"MR" {
        validate_media_rendition_dict(v, d, since)
            .map_err(|e| e.context(format!("{dict_name} media rendition")))?;
    }

    if rendition_type == b"SR" {
        validate_selector_rendition_dict(v, d, since, depth, visit)
            .map_err(|e| e.context(format!("{dict_name} selector rendition")))?;
    }

    Ok(())
}

/// The error of a rendition whose page is not in the page tree (`renditionPageNotInTree`).
fn rendition_page_not_in_tree() -> Error {
    Error::Validation("Rendition.AN.P: page is not in the page tree".to_string())
}

/// `recordRenditionPage`: the page tree walk found the page object `obj_nr`.
pub fn record_rendition_page(v: &mut Validator<'_>, obj_nr: i64) {
    v.st.rendition_pages.pages.insert(obj_nr);
}

/// `validateRenditionPageMembership` (12.6.4.14): the page `obj_nr` of a rendition action has to
/// be in the page tree. A page that the walk has not reached yet is checked when it is done
/// (`RenditionPageIndex::finish`).
///
/// # Errors
/// [`Error::Cancelled`] and [`Error::Validation`] for a page that is not in the page tree.
pub fn validate_rendition_page_membership(v: &mut Validator<'_>, obj_nr: i64) -> Result<(), Error> {
    v.check()?;
    let index = &mut v.st.rendition_pages;
    if index.pages.contains(&obj_nr) {
        return Ok(());
    }
    if !index.complete {
        index.pending.push(obj_nr);
        return Ok(());
    }
    Err(rendition_page_not_in_tree())
}

/// `renditionPageAnnotations`: the annotation references of the page `obj_nr`, whose
/// dictionary is `page_dict`.
///
/// # Errors
/// [`Error::Cancelled`], and the errors of the `Annots` entry.
pub fn rendition_page_annotations(
    v: &mut Validator<'_>,
    page_dict: &Dict,
    obj_nr: i64,
) -> Result<BTreeSet<Ref>, Error> {
    v.check()?;
    if let Some(refs) = v.st.rendition_pages.annotations.get(&obj_nr) {
        return Ok(refs.clone());
    }

    let annots = validate_array_entry(
        v.x,
        page_dict,
        "pageDict",
        "Annots",
        REQUIRED,
        Version::V10,
        None,
    )?;
    // The cache keeps the set for the pass, in B-tree nodes about half full.
    let n = annots.as_ref().map_or(0, Vec::len) as u64;
    v.held.grow_by(n * 2 * size_of::<Ref>() as u64)?;
    let mut refs = BTreeSet::new();
    for o in annots.iter().flatten() {
        v.check()?;
        if let Object::Ref(r) = o {
            refs.insert(*r);
        }
    }

    v.st.rendition_pages
        .annotations
        .insert(obj_nr, refs.clone());
    Ok(refs)
}

/// `clearRenditionPageAnnotations`.
pub fn clear_rendition_page_annotations(v: &mut Validator<'_>, obj_nr: i64) {
    v.st.rendition_pages.annotations.remove(&obj_nr);
}

/// `mediaValidationKind`: the role of a media object whose structural validation is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaValidationKind {
    ThreeDStream,
    RichMediaContent,
    RichMediaAssets,
}

/// `validateMediaObject`: runs `validate` unless the indirect object `raw` already validated in
/// this role in this pass; the result says whether it was cached, so that a caller can still do
/// its owner-specific work. A direct object has no identity and is always validated.
///
/// # Errors
/// [`Error::Cancelled`] and the errors of `validate`.
pub fn validate_media_object(
    v: &mut Validator<'_>,
    raw: Option<&Object>,
    kind: MediaValidationKind,
    validate: impl FnOnce(&mut Validator<'_>) -> Result<(), Error>,
) -> Result<bool, Error> {
    v.check()?;
    let key = match raw {
        Some(Object::Ref(r)) => Some((kind, *r)),
        _ => None,
    };
    if let Some(k) = &key
        && v.st.media_validated.contains(k)
    {
        return Ok(true);
    }

    validate(v)?;
    v.check()?;

    if let Some(k) = key {
        v.st.media_validated.insert(k);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn r(n: i64) -> Object {
        Object::Ref(Ref {
            num: n,
            generation: 0,
        })
    }

    /// `selectorRendition`.
    fn selector_rendition(refs: Object) -> Dict {
        let mut d = Dict::default();
        d.set(b"Type", Object::name("Rendition"));
        d.set(b"S", Object::name("SR"));
        d.set(b"R", refs);
        d
    }

    /// `renditionXRefTable`.
    fn rendition_table(max_depth: u64, objects: Vec<(i64, Object)>) -> XRefTable {
        let mut conf = Config::default();
        conf.limits.max_recursion_depth = max_depth;
        let mut x = XRefTable::new(conf);
        x.header_version = Some(Version::V17);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn validate(x: &mut XRefTable, d: &Dict, cancel: &AtomicBool) -> Result<(), Error> {
        let mut v = Validator::new(x, cancel);
        validate_rendition_dict(&mut v, d, 1, Version::V15)
    }

    /// `TestValidateSelectorRenditionCancellation`: the cancel flag stops the traversal.
    #[test]
    fn selector_rendition_cancellation() {
        let d1 = selector_rendition(Object::Array(vec![r(2), Object::Integer(1), r(3)]));
        let objects = vec![
            (1, Object::Dict(d1.clone())),
            (2, Object::Dict(selector_rendition(Object::Array(vec![])))),
            (3, Object::Dict(selector_rendition(Object::Array(vec![])))),
        ];
        let mut x = rendition_table(100, objects);
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let err = validate(&mut x, &d1, &cancel).unwrap_err();
        assert!(matches!(err.root(), Error::Cancelled), "got {err}");
    }

    /// `TestValidateSelectorRenditionRejectsCycleAndDepth`.
    #[test]
    fn selector_rendition_rejects_cycle_and_depth() {
        let cancel = AtomicBool::new(false);

        let cycle = selector_rendition(Object::Array(vec![r(1)]));
        let mut x = rendition_table(100, vec![(1, Object::Dict(cycle.clone()))]);
        let err = validate(&mut x, &cycle, &cancel).unwrap_err();
        assert!(err.is(Sentinel::RenditionCycle), "got {err}");

        let chain = vec![
            (1, selector_rendition(Object::Array(vec![r(2)]))),
            (2, selector_rendition(Object::Array(vec![r(3)]))),
            (3, selector_rendition(Object::Array(vec![]))),
        ];
        let first = chain[0].1.clone();
        let objects = chain
            .into_iter()
            .map(|(n, d)| (n, Object::Dict(d)))
            .collect();
        let mut x = rendition_table(1, objects);
        let err = validate(&mut x, &first, &cancel).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "got {err}");
    }

    /// `TestValidateSelectorRenditionCachesSharedSubtrees`: shared indirect successors are
    /// validated once, so the 2^12 paths of the chain are cheap.
    #[test]
    fn selector_rendition_caches_shared_subtrees() {
        let mut dicts = vec![(13, selector_rendition(Object::Array(vec![])))];
        for n in (1..=12).rev() {
            dicts.push((
                n,
                selector_rendition(Object::Array(vec![r(n + 1), r(n + 1)])),
            ));
        }
        let first = dicts.last().map(|(_, d)| d.clone()).unwrap();
        let objects = dicts
            .into_iter()
            .map(|(n, d)| (n, Object::Dict(d)))
            .collect();
        let mut x = rendition_table(100, objects);
        let cancel = AtomicBool::new(false);
        validate(&mut x, &first, &cancel).unwrap();
    }

    /// `TestValidateSelectorRenditionChecksSharedSubtreeAtDeeperDepth`: a cached shallower path
    /// cannot hide a depth error.
    #[test]
    fn selector_rendition_checks_shared_subtree_at_deeper_depth() {
        let cancel = AtomicBool::new(false);
        let build = |max_depth: u64, root: Vec<Object>| {
            let objects = vec![
                (1, Object::Dict(selector_rendition(Object::Array(root)))),
                (
                    2,
                    Object::Dict(selector_rendition(Object::Array(vec![r(3)]))),
                ),
                (3, Object::Dict(selector_rendition(Object::Array(vec![])))),
                (
                    4,
                    Object::Dict(selector_rendition(Object::Array(vec![r(5)]))),
                ),
                (
                    5,
                    Object::Dict(selector_rendition(Object::Array(vec![r(2)]))),
                ),
            ];
            rendition_table(max_depth, objects)
        };

        let mut x = build(3, vec![r(2), r(4)]);
        let first = selector_rendition(Object::Array(vec![r(2), r(4)]));
        let err = validate(&mut x, &first, &cancel).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth), "got {err}");

        let mut x = build(4, vec![r(4), r(2)]);
        let first = selector_rendition(Object::Array(vec![r(4), r(2)]));
        validate(&mut x, &first, &cancel).unwrap();
    }

    /// `TestValidateSelectorRenditionKeepsDirectDictionariesDistinct`.
    #[test]
    fn selector_rendition_keeps_direct_dictionaries_distinct() {
        let mut second = Dict::default();
        second.set(b"Type", Object::name("Rendition"));
        second.set(b"R", Object::Array(vec![]));
        let first = selector_rendition(r(2));
        let objects = vec![
            (1, Object::Dict(first.clone())),
            (
                2,
                Object::Array(vec![
                    Object::Dict(selector_rendition(Object::Array(vec![]))),
                    Object::Dict(second),
                ]),
            ),
        ];
        let mut x = rendition_table(100, objects);
        let cancel = AtomicBool::new(false);
        assert!(validate(&mut x, &first, &cancel).is_err());
    }

    /// The media object cache: an indirect object validates once, a direct one always.
    #[test]
    fn media_objects_are_cached_by_reference() {
        let mut x = rendition_table(100, vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let raw = r(5);
        let kind = MediaValidationKind::ThreeDStream;
        assert!(!validate_media_object(&mut v, Some(&raw), kind, |_| Ok(())).unwrap());
        assert!(validate_media_object(&mut v, Some(&raw), kind, |_| Ok(())).unwrap());
        // Another role is validated again, and a failure is not cached.
        let other = MediaValidationKind::RichMediaAssets;
        assert!(!validate_media_object(&mut v, Some(&raw), other, |_| Ok(())).unwrap());
        let bad = r(6);
        let fail = |_: &mut Validator<'_>| Err(Error::Validation("bad".to_string()));
        assert!(validate_media_object(&mut v, Some(&bad), kind, fail).is_err());
        assert!(!validate_media_object(&mut v, Some(&bad), kind, |_| Ok(())).unwrap());
        assert!(!validate_media_object(&mut v, None, kind, |_| Ok(())).unwrap());
        assert!(!validate_media_object(&mut v, None, kind, |_| Ok(())).unwrap());
    }

    /// The page index: a page that the walk has not reached is pending until `finish`.
    #[test]
    fn rendition_pages_are_checked_when_the_walk_is_done() {
        let mut x = rendition_table(100, vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        record_rendition_page(&mut v, 3);
        validate_rendition_page_membership(&mut v, 3).unwrap();
        validate_rendition_page_membership(&mut v, 9).unwrap();
        assert!(v.st.rendition_pages.finish(&cancel).is_err());
        v.st.rendition_pages.pending.clear();
        v.st.rendition_pages.complete = true;
        assert!(validate_rendition_page_membership(&mut v, 3).is_ok());
        assert!(validate_rendition_page_membership(&mut v, 9).is_err());
    }
}
