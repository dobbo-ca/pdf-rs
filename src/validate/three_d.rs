//! 3D annotations' data (Go: validate/threeD.go).
//!
//! What other packages call: [`validate_annotation_dict_3d`] (annotations, WP 16) and
//! [`validate_3d_view_selector`] with [`ThreeDViewSelectorKind::Action`] (3D actions, WP 17).
//!
//! Differences from Go: validation is relaxed, always. A 3D annotation without an appearance, a
//! stream with another subtype and a legacy view selector name are skipped, where Go only logs
//! them, and the version 1.7 entries are accepted since 1.6. The active content notice of
//! `OnInstantiate` is not ported (the notices are out of scope), but the dereference that
//! precedes it is. `ValidationError` object numbers are dropped (see `object.rs`).
//!
//! `validate_rectangle_entry` repairs a null coordinate of `3DB` in a copy of the annotation's
//! array; a caller that holds a direct array writes the repair back itself.

use super::Validator;
use super::media::{MediaValidationKind, validate_media_object};
use super::name_tree::validate_name_tree;
use super::object::{
    OPTIONAL, REQUIRED, validate_array_entry, validate_boolean_entry, validate_dict_entry,
    validate_entry, validate_integer_entry, validate_name_entry, validate_number_array_entry,
    validate_number_entry, validate_rectangle_entry, validate_stream_dict_entry,
    validate_string_entry,
};
use crate::error::Error;
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, StreamDict};

/// `threeDViewSelectorKind`: where a view selector is, which decides the names it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeDViewSelectorKind {
    /// The `DV` entry of a 3D stream.
    Stream,
    /// The `3DV` entry of a 3D annotation.
    Annotation,
    /// The `V` entry of a 3D action.
    Action,
}

/// `threeDV17SinceVersion` (relaxed): the entries added in 1.7 are accepted since 1.6.
const THREE_D_V17_SINCE: Version = Version::V16;

/// `missingRequiredEntryError`.
fn missing_required_entry_error(dict_name: &str, entry_name: &str, hint: &str) -> Error {
    let mut msg = format!("dict={dict_name} required entry={entry_name} missing");
    if !hint.is_empty() {
        msg.push_str("; repair: ");
        msg.push_str(hint);
    }
    Error::Validation(msg)
}

/// The type of an optional object for a message.
fn type_of(o: Option<&Object>) -> &'static str {
    o.map_or("<nil>", type_name)
}

/// `validate3DAnnotationAppearance`: the appearance is validated; a missing normal appearance
/// is skipped in relaxed validation.
fn validate_3d_annotation_appearance(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let appearance = validate_dict_entry(x, d, dict_name, "AP", OPTIONAL, Version::V16, None)?;
    if let Some(appearance) = appearance {
        validate_entry(
            x,
            &appearance,
            "appearanceDict",
            "N",
            OPTIONAL,
            Version::V16,
        )?;
    }
    Ok(())
}

/// The names an entry of a 3D activation dictionary may have.
fn activation_value(entry_name: &str, s: &[u8]) -> bool {
    match entry_name {
        "A" => matches!(s, b"PO" | b"PV" | b"XA"),
        "AIS" => matches!(s, b"I" | b"L"),
        "D" => matches!(s, b"PC" | b"PI" | b"XD"),
        _ => matches!(s, b"U" | b"I" | b"L"),
    }
}

/// `validate3DActivationDict` (13.6.2, table 299).
fn validate_3d_activation_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "3DActivationDict";
    for name in ["A", "AIS", "D", "DIS"] {
        validate_name_entry(
            x,
            d,
            dict_name,
            name,
            OPTIONAL,
            Version::V16,
            Some(&|s: &[u8]| activation_value(name, s)),
        )?;
    }
    for entry_name in ["TB", "NP"] {
        validate_boolean_entry(
            x,
            d,
            dict_name,
            entry_name,
            OPTIONAL,
            THREE_D_V17_SINCE,
            None,
        )?;
    }
    Ok(())
}

/// `validate3DPathMember`: an element of `U3DPath` is a text string.
fn validate_3d_path_member(v: &mut Validator<'_>, raw: &Object) -> Result<(), Error> {
    v.check()?;
    let o = v.x.dereference(raw)?.clone();
    if o.is_null() {
        return Err(Error::Validation("expected text string".to_string()));
    }
    v.x.dereference_string_or_hex_literal(&o, Version::V16, None)?;
    Ok(())
}

/// `validate3DViewPath`: `U3DPath` is a text string or an array of them.
fn validate_3d_view_path(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "3DViewDict";
    let Some(raw) = d.find(b"U3DPath").cloned() else {
        return Err(missing_required_entry_error(
            dict_name,
            "U3DPath",
            "required when MS is U3D",
        ));
    };
    let o = validate_entry(v.x, d, dict_name, "U3DPath", REQUIRED, Version::V16)?;
    match o {
        Some(Object::String(_) | Object::Hex(_)) => {
            v.x.dereference_string_or_hex_literal(&raw, Version::V16, None)?;
            Ok(())
        }
        Some(Object::Array(a)) => {
            if a.is_empty() {
                return Err(Error::Validation(format!(
                    "dict={dict_name} entry=U3DPath: expected at least one string"
                )));
            }
            for (i, value) in a.iter().enumerate() {
                validate_3d_path_member(v, value)
                    .map_err(|e| e.context(format!("dict={dict_name} entry=U3DPath[{i}]")))?;
            }
            Ok(())
        }
        other => Err(Error::Validation(format!(
            "dict={dict_name} entry=U3DPath: invalid type {}",
            type_of(other.as_ref())
        ))),
    }
}

/// `validate3DDictArrayEntry`: an optional array of dictionaries, each checked with `validate`.
fn validate_3d_dict_array_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
    validate: Option<fn(&mut Validator<'_>, &Dict) -> Result<(), Error>>,
) -> Result<(), Error> {
    let a = validate_array_entry(v.x, d, dict_name, entry_name, OPTIONAL, since, None)?;
    let Some(a) = a else {
        return Ok(());
    };
    for (i, raw) in a.iter().enumerate() {
        v.check()?;
        let entry = v.x.dereference_dict(raw)?.cloned();
        let Some(entry) = entry else {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name}[{i}]: expected dictionary"
            )));
        };
        if let Some(validate) = validate {
            validate(v, &entry)?;
        }
    }
    Ok(())
}

/// `validate3DViewNestedEntries`.
fn validate_3d_view_nested_entries(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "3DViewDict";
    for entry_name in ["P", "BG"] {
        validate_dict_entry(v.x, d, dict_name, entry_name, OPTIONAL, Version::V16, None)?;
    }
    for entry_name in ["RM", "LS"] {
        validate_dict_entry(
            v.x,
            d,
            dict_name,
            entry_name,
            OPTIONAL,
            THREE_D_V17_SINCE,
            None,
        )?;
    }
    validate_stream_dict_entry(v.x, d, dict_name, "O", OPTIONAL, Version::V16, None)?;
    for entry_name in ["SA", "NA"] {
        validate_3d_dict_array_entry(v, d, dict_name, entry_name, THREE_D_V17_SINCE, None)?;
    }
    validate_boolean_entry(v.x, d, dict_name, "NR", OPTIONAL, THREE_D_V17_SINCE, None)?;
    Ok(())
}

/// `validate3DViewDict` (13.6.4, table 304).
fn validate_3d_view_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    v.check()?;
    let dict_name = "3DViewDict";
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V16,
        Some(&|s: &[u8]| s == b"3DView"),
    )?;
    validate_string_entry(v.x, d, dict_name, "XN", REQUIRED, Version::V16, None)?;
    validate_string_entry(v.x, d, dict_name, "IN", OPTIONAL, Version::V16, None)?;
    let ms = validate_name_entry(
        v.x,
        d,
        dict_name,
        "MS",
        OPTIONAL,
        Version::V16,
        Some(&|s: &[u8]| s == b"M" || s == b"U3D"),
    )?;
    if ms.as_deref() == Some(b"M".as_slice()) {
        validate_number_array_entry(
            v.x,
            d,
            dict_name,
            "C2W",
            REQUIRED,
            Version::V16,
            Some(&|a: &[Object]| a.len() == 12),
        )?;
    }
    if ms.as_deref() == Some(b"U3D".as_slice()) {
        validate_3d_view_path(v, d)?;
    }
    if ms.is_some() {
        validate_number_entry(
            v.x,
            d,
            dict_name,
            "CO",
            OPTIONAL,
            Version::V16,
            Some(&|f: f64| f >= 0.0),
        )?;
    }
    validate_3d_view_nested_entries(v, d)
}

/// `valid3DViewSelectorName`.
fn valid_3d_view_selector_name(name: &[u8], kind: ThreeDViewSelectorKind) -> bool {
    match kind {
        ThreeDViewSelectorKind::Stream => matches!(name, b"F" | b"L"),
        ThreeDViewSelectorKind::Annotation => matches!(name, b"F" | b"L" | b"D"),
        ThreeDViewSelectorKind::Action => matches!(name, b"F" | b"L" | b"D" | b"N" | b"P"),
    }
}

/// `validate3DViewSelector`: the 3D view in `entry_name` of `d` is a view dictionary, an index,
/// a name or a name of a view.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for a bad selector, and the errors of the lookups.
pub fn validate_3d_view_selector(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    kind: ThreeDViewSelectorKind,
) -> Result<(), Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, Version::V16)? else {
        return Ok(());
    };
    match o {
        Object::Dict(d1) => validate_3d_view_dict(v, &d1),
        Object::Integer(i) => {
            if i >= 0 {
                return Ok(());
            }
            Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name}: negative 3D view index {i}"
            )))
        }
        Object::String(_) | Object::Hex(_) => {
            let raw = raw.unwrap_or(Object::Null);
            v.x.dereference_string_or_hex_literal(&raw, Version::V16, None)?;
            Ok(())
        }
        Object::Name(n) => {
            // Relaxed validation treats the legacy name DEFAULT of a stream as absent.
            if kind == ThreeDViewSelectorKind::Stream && n == b"DEFAULT" {
                return Ok(());
            }
            if valid_3d_view_selector_name(&n, kind) {
                return Ok(());
            }
            Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name}: invalid 3D view selector name {:?}",
                String::from_utf8_lossy(&n)
            )))
        }
        other => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name}: invalid 3D view selector type {}",
            type_name(&other)
        ))),
    }
}

/// `validate3DAnimationStyleDict` (table 301).
fn validate_3d_animation_style_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "3DAnimationStyleDict";
    let since = THREE_D_V17_SINCE;
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"3DAnimationStyle"),
    )?;
    let subtype = validate_name_entry(x, d, dict_name, "Subtype", OPTIONAL, since, None)?;
    // Play count and time multiplier are ignored for None, and other subtypes are treated as
    // None.
    if !matches!(subtype.as_deref(), Some(b"Linear" | b"Oscillating")) {
        return Ok(());
    }
    validate_integer_entry(x, d, dict_name, "PC", OPTIONAL, since, None)?;
    validate_number_entry(
        x,
        d,
        dict_name,
        "TM",
        OPTIONAL,
        since,
        Some(&|f: f64| f > 0.0),
    )?;
    Ok(())
}

/// `validate3DResources`: the name tree of named resources.
fn validate_3d_resources(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let Some(raw) = d.find(b"Resources").cloned() else {
        return Ok(());
    };
    let resources = validate_dict_entry(
        v.x,
        d,
        "3DStreamDict",
        "Resources",
        OPTIONAL,
        Version::V16,
        None,
    )?;
    let Some(resources) = resources else {
        return Ok(());
    };
    validate_name_tree(v, "3DResources", &resources, true, Some(&raw))?;
    Ok(())
}

/// `validate3DStreamSubtype`: only `U3D` (and `PRC` since 2.0) are known; relaxed validation
/// skips another format.
fn validate_3d_stream_subtype(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    validate_name_entry(
        x,
        d,
        "3DStreamDict",
        "Subtype",
        REQUIRED,
        Version::V16,
        None,
    )?;
    Ok(())
}

/// `validate3DOnInstantiate` (13.6.3.1): the script of the stream. A shared script is
/// dereferenced again for each annotation that uses it.
fn validate_3d_on_instantiate(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let js = validate_stream_dict_entry(
        x,
        d,
        "3DStreamDict",
        "OnInstantiate",
        OPTIONAL,
        Version::V16,
        None,
    )?;
    if js.is_none()
        && let Some(raw) = d.0.get(b"OnInstantiate".as_slice())
        && !raw.is_null()
    {
        x.dereference_stream_dict(raw)?;
    }
    Ok(())
}

/// `validateShared3DStream` (13.6.3.1): validates the 3D stream once per pass, and the script
/// of a cached stream again.
fn validate_shared_3d_stream(
    v: &mut Validator<'_>,
    raw: Option<&Object>,
    sd: &StreamDict,
) -> Result<(), Error> {
    let cached = validate_media_object(v, raw, MediaValidationKind::ThreeDStream, |v| {
        validate_3d_stream_dict(v, sd)
    })?;
    if !cached {
        return Ok(());
    }
    validate_3d_on_instantiate(v.x, &sd.dict)
}

/// `validate3DStreamDict` (13.6.3.1).
fn validate_3d_stream_dict(v: &mut Validator<'_>, sd: &StreamDict) -> Result<(), Error> {
    v.check()?;
    let d = &sd.dict;
    let dict_name = "3DStreamDict";
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V16,
        Some(&|s: &[u8]| s == b"3D"),
    )?;
    validate_3d_stream_subtype(v.x, d)?;
    validate_3d_dict_array_entry(
        v,
        d,
        dict_name,
        "VA",
        Version::V16,
        Some(validate_3d_view_dict),
    )?;
    validate_3d_view_selector(
        v,
        d,
        dict_name,
        "DV",
        OPTIONAL,
        ThreeDViewSelectorKind::Stream,
    )?;
    validate_3d_resources(v, d)?;
    validate_3d_on_instantiate(v.x, d)?;
    let animation =
        validate_dict_entry(v.x, d, dict_name, "AN", OPTIONAL, THREE_D_V17_SINCE, None)?;
    if let Some(animation) = animation {
        validate_3d_animation_style_dict(v.x, &animation)?;
    }
    Ok(())
}

/// `validate3DReferenceDict` (13.6.3.3).
fn validate_3d_reference_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "3DReferenceDict";
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V16,
        Some(&|s: &[u8]| s == b"3DRef"),
    )?;
    // 3DD is required in strict validation only; relaxed validation skips a missing one.
    let Some(o) = validate_entry(v.x, d, dict_name, "3DD", OPTIONAL, Version::V16)? else {
        return Ok(());
    };
    match o {
        Object::Stream(sd) => validate_shared_3d_stream(v, d.0.get(b"3DD".as_slice()), &sd),
        other => Err(Error::Validation(format!(
            "dict={dict_name} entry=3DD: expected 3D stream, got {}",
            type_name(&other)
        ))),
    }
}

/// `validate3DDataEntry` (13.6.2): `3DD` is a 3D stream or a reference dictionary.
fn validate_3d_data_entry(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let raw = d.find(b"3DD").cloned();
    let o = validate_entry(v.x, d, "3D", "3DD", REQUIRED, Version::V16)?;
    match o {
        Some(Object::Stream(sd)) => validate_shared_3d_stream(v, raw.as_ref(), &sd),
        Some(Object::Dict(d1)) => validate_3d_reference_dict(v, &d1),
        other => Err(Error::Validation(format!(
            "dict=3D entry=3DD: expected 3D stream or reference dictionary, got {}",
            type_of(other.as_ref())
        ))),
    }
}

/// `validateAnnotationDict3D` (13.6.2): the entries of a 3D annotation dictionary `d`.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] and the errors of the lookups.
pub fn validate_annotation_dict_3d(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    v.check()?;
    validate_3d_annotation_appearance(v.x, d, dict_name)?;
    validate_3d_data_entry(v, d)?;
    validate_3d_view_selector(
        v,
        d,
        dict_name,
        "3DV",
        OPTIONAL,
        ThreeDViewSelectorKind::Annotation,
    )?;
    let activation = validate_dict_entry(v.x, d, dict_name, "3DA", OPTIONAL, Version::V16, None)?;
    if let Some(activation) = activation {
        validate_3d_activation_dict(v.x, &activation)?;
    }
    validate_boolean_entry(v.x, d, dict_name, "3DI", OPTIONAL, Version::V16, None)?;
    validate_rectangle_entry(v.x, d, dict_name, "3DB", OPTIONAL, Version::V16, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::Ref;
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

    fn selector(x: &mut XRefTable, o: Object, kind: ThreeDViewSelectorKind) -> Result<(), Error> {
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(x, &cancel);
        let d = dict(&[("V", o)]);
        validate_3d_view_selector(&mut v, &d, "3DADict", "V", REQUIRED, kind)
    }

    #[test]
    fn view_selector_values() {
        use ThreeDViewSelectorKind::{Action, Annotation, Stream};
        let mut x = table();
        assert!(selector(&mut x, Object::Integer(0), Action).is_ok());
        assert!(selector(&mut x, Object::Integer(-1), Action).is_err());
        assert!(selector(&mut x, Object::string("view"), Annotation).is_ok());
        // Names by place.
        assert!(selector(&mut x, Object::name("N"), Action).is_ok());
        assert!(selector(&mut x, Object::name("N"), Annotation).is_err());
        assert!(selector(&mut x, Object::name("D"), Annotation).is_ok());
        assert!(selector(&mut x, Object::name("D"), Stream).is_err());
        // The legacy name of a stream is skipped in relaxed validation.
        assert!(selector(&mut x, Object::name("DEFAULT"), Stream).is_ok());
        assert!(selector(&mut x, Object::name("DEFAULT"), Action).is_err());
        assert!(selector(&mut x, Object::Bool(true), Action).is_err());
    }

    #[test]
    fn view_dictionary_and_path() {
        let mut x = table();
        let ok = dict(&[
            ("XN", Object::string("front")),
            ("MS", Object::name("U3D")),
            ("U3DPath", Object::Array(vec![Object::string("a")])),
        ]);
        let r = selector(&mut x, Object::Dict(ok), ThreeDViewSelectorKind::Action);
        assert!(r.is_ok(), "{r:?}");
        let no_path = dict(&[("XN", Object::string("front")), ("MS", Object::name("U3D"))]);
        let r = selector(
            &mut x,
            Object::Dict(no_path),
            ThreeDViewSelectorKind::Action,
        );
        assert!(r.is_err());
        let empty = dict(&[
            ("XN", Object::string("front")),
            ("MS", Object::name("U3D")),
            ("U3DPath", Object::Array(vec![])),
        ]);
        let r = selector(&mut x, Object::Dict(empty), ThreeDViewSelectorKind::Action);
        assert!(r.is_err());
        let matrix = dict(&[("XN", Object::string("front")), ("MS", Object::name("M"))]);
        let r = selector(&mut x, Object::Dict(matrix), ThreeDViewSelectorKind::Action);
        assert!(r.is_err());
    }

    /// A shared 3D stream validates once, and its owner-specific work runs for each user.
    #[test]
    fn a_shared_stream_validates_once() {
        let mut x = table();
        let stream_dict = dict(&[("Subtype", Object::name("U3D"))]);
        let sd = StreamDict {
            dict: stream_dict,
            ..StreamDict::default()
        };
        x.table
            .insert(5, Entry::gen0(Object::Stream(Box::new(sd.clone()))));
        let raw = Object::Ref(Ref {
            num: 5,
            generation: 0,
        });
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_shared_3d_stream(&mut v, Some(&raw), &sd).unwrap();
        assert_eq!(v.st.media_validated.len(), 1);
        validate_shared_3d_stream(&mut v, Some(&raw), &sd).unwrap();
        assert_eq!(v.st.media_validated.len(), 1);
    }

    #[test]
    fn activation_names() {
        let mut x = table();
        let ok = dict(&[("A", Object::name("PO")), ("DIS", Object::name("U"))]);
        assert!(validate_3d_activation_dict(&mut x, &ok).is_ok());
        let bad = dict(&[("A", Object::name("PX"))]);
        assert!(validate_3d_activation_dict(&mut x, &bad).is_err());
    }
}
