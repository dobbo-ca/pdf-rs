//! The viewer preferences of the catalog (Go: validate/viewerPreferences.go, 12.2).
//!
//! Go stores the values in `model.ViewerPreferences` on the document. Nothing the port reaches
//! reads them, so only what a later check depends on is kept: whether `PrintScaling` is
//! `AppDefault`, which `Enforce` must not contradict.

use super::Validator;
use super::object::{
    OPTIONAL, validate_array_entry, validate_dict_entry, validate_flex_boolean_entry,
    validate_integer_array_entry, validate_integer_entry, validate_name_array_entry,
    validate_name_entry, validate_name_entry_keep, validate_string_entry,
};
use crate::error::Error;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object};

/// `validateDirection`: relaxed validation also accepts a string.
fn validate_direction(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let r = validate_name_entry(
        x,
        d,
        dict_name,
        "Direction",
        OPTIONAL,
        Version::V13,
        Some(&|s: &[u8]| s == b"L2R" || s == b"R2L"),
    );
    if r.is_ok() {
        return Ok(());
    }
    validate_string_entry(
        x,
        d,
        dict_name,
        "Direction",
        OPTIONAL,
        Version::V13,
        Some(&|s: &str| s == "L2R" || s == "R2L"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Direction")))?;
    Ok(())
}

/// `validatePageBoundaries`.
fn validate_page_boundaries(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    for entry in ["ViewArea", "PrintArea", "ViewClip", "PrintClip"] {
        validate_name_entry(
            x,
            d,
            dict_name,
            entry,
            OPTIONAL,
            Version::V14,
            Some(&|s: &[u8]| {
                matches!(
                    s,
                    b"MediaBox" | b"CropBox" | b"BleedBox" | b"TrimBox" | b"ArtBox"
                )
            }),
        )
        .map_err(|e| e.context(format!("{dict_name}.{entry}")))?;
    }
    Ok(())
}

/// `validatePrintPageRange`: pairs of page numbers, each pair ascending.
fn validate_print_page_range(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let arr = validate_integer_array_entry(
        x,
        d,
        dict_name,
        "PrintPageRange",
        OPTIONAL,
        Version::V17,
        Some(&|a: &[Object]| a.len().is_multiple_of(2)),
    )
    .map_err(|e| e.context(format!("{dict_name}.PrintPageRange")))?;
    let Some(arr) = arr else {
        return Ok(());
    };

    let mut page_range: Vec<i64> = Vec::with_capacity(arr.len());
    for (j, o) in arr.iter().enumerate() {
        match x.dereference_integer(o) {
            Ok(Some(i)) => page_range.push(i),
            Ok(None) => {
                return Err(Error::Validation(format!(
                    "{dict_name}.PrintPageRange[{j}]: missing integer at index {j}"
                )));
            }
            Err(e) => return Err(e.context(format!("{dict_name}.PrintPageRange[{j}]"))),
        }
    }

    for j in (0..page_range.len()).step_by(2) {
        if let (Some(first), Some(last)) = (page_range.get(j), page_range.get(j + 1))
            && first >= last
        {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry=PrintPageRange invalid dict entry"
            ))
            .context(format!("{dict_name}.PrintPageRange")));
        }
    }
    Ok(())
}

/// `validateEnforcePrintScaling`: `Enforce` names `PrintScaling`, which cannot be `AppDefault`.
fn validate_enforce_print_scaling(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    print_scaling_app_default: bool,
) -> Result<(), Error> {
    let arr = validate_name_array_entry(
        x,
        d,
        dict_name,
        "Enforce",
        OPTIONAL,
        Version::V20,
        Some(&|a: &[Object]| a.len() == 1),
    )
    .map_err(|e| e.context(format!("{dict_name}.Enforce")))?;
    let Some(first) = arr.as_ref().and_then(|a| a.first()) else {
        return Ok(());
    };

    x.dereference_name(first, Version::V20, Some(&|s: &[u8]| s == b"PrintScaling"))
        .map_err(|e| e.context(format!("{dict_name}.Enforce[0]")))?;
    if print_scaling_app_default {
        return Err(Error::Validation(
            "ViewerPreferences.Enforce: PrintScaling requires PrintScaling != \"AppDefault\""
                .to_string(),
        ));
    }
    Ok(())
}

/// `validatePrinterPreferences`.
fn validate_printer_preferences(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // PrintScaling: relaxed validation ignores an invalid value, but the name is still used.
    let n = match validate_name_entry_keep(
        x,
        d,
        dict_name,
        "PrintScaling",
        OPTIONAL,
        Version::V13,
        Some(&|s: &[u8]| s == b"None" || s == b"AppDefault"),
    ) {
        Ok(n) => n,
        Err(e) => e.name,
    };
    // `PrintScalingFor`: the name in any case.
    let app_default = n.is_some_and(|n| n.eq_ignore_ascii_case(b"appdefault"));

    validate_name_entry(
        x,
        d,
        dict_name,
        "Duplex",
        OPTIONAL,
        Version::V14,
        Some(&|s: &[u8]| {
            matches!(
                s,
                b"Simplex" | b"DuplexFlipShortEdge" | b"DuplexFlipLongEdge"
            )
        }),
    )
    .map_err(|e| e.context(format!("{dict_name}.Duplex")))?;

    validate_flex_boolean_entry(x, d, dict_name, "PickTrayByPDFSize", OPTIONAL, Version::V15)
        .map_err(|e| e.context(format!("{dict_name}.PickTrayByPDFSize")))?;

    validate_integer_entry(
        x,
        d,
        dict_name,
        "NumCopies",
        OPTIONAL,
        Version::V15,
        Some(&|i: i64| i >= 1),
    )
    .map_err(|e| e.context(format!("{dict_name}.NumCopies")))?;

    validate_print_page_range(x, d, dict_name)?;

    validate_enforce_print_scaling(x, d, dict_name, app_default)
}

/// `validateViewerPreferencesFlags`.
fn validate_viewer_preferences_flags(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    for entry in [
        "HideToolbar",
        "HideMenubar",
        "HideWindowUI",
        "FitWindow",
        "CenterWindow",
        "DisplayDocTitle",
    ] {
        validate_flex_boolean_entry(x, d, dict_name, entry, OPTIONAL, Version::V10)
            .map_err(|e| e.context(format!("{dict_name}.{entry}")))?;
    }
    Ok(())
}

/// `digestViewerPreferencesArray`: an out-of-spec array of names instead of the dictionary.
fn digest_viewer_preferences_array(x: &mut XRefTable, arr: &[Object]) -> Result<(), Error> {
    for (i, v) in arr.iter().enumerate() {
        let o = x
            .dereference(v)
            .map_err(|e| e.context(format!("rootDict.ViewerPreferences[{i}]")))?;
        if !matches!(o, Object::Name(_)) {
            return Err(Error::Validation(format!(
                "rootDict.ViewerPreferences[{i}]: expected name, got {}",
                crate::model::deref::type_name(o)
            )));
        }
    }
    Ok(())
}

/// `validateViewerPreferences`: the `ViewerPreferences` entry of the catalog.
///
/// # Errors
/// [`Error::Validation`] for an entry that is not valid, and the errors of the dereference.
pub fn validate_viewer_preferences(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let dict_name = "rootDict";
    let d = match validate_dict_entry(
        x,
        root,
        dict_name,
        "ViewerPreferences",
        required,
        since,
        None,
    ) {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(_) => {
            // Relaxed: an array of names.
            let arr = validate_array_entry(
                x,
                root,
                dict_name,
                "ViewerPreferences",
                required,
                since,
                None,
            )
            .map_err(|e| e.context("rootDict.ViewerPreferences"))?;
            return match arr {
                Some(arr) if !arr.is_empty() => digest_viewer_preferences_array(x, &arr),
                _ => Ok(()),
            };
        }
    };

    let dict_name = "ViewerPreferences";

    validate_viewer_preferences_flags(x, &d, dict_name)?;

    validate_name_entry(
        x,
        &d,
        dict_name,
        "NonFullScreenPageMode",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| {
            matches!(
                s,
                b"UseNone" | b"UseOutlines" | b"UseThumbs" | b"UseOC" | b"PageOnly"
            )
        }),
    )
    .map_err(|e| e.context(format!("{dict_name}.NonFullScreenPageMode")))?;

    validate_direction(x, &d, dict_name)?;

    validate_page_boundaries(x, &d, dict_name)?;

    validate_printer_preferences(x, &d, dict_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn run(version: Version, prefs: Object) -> Result<(), Error> {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(version);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let mut root = dict(&[("ViewerPreferences", prefs)]);
        validate_viewer_preferences(&mut v, &mut root, false, Version::V12)
    }

    #[test]
    fn valid_preferences() {
        let prefs = Object::Dict(dict(&[
            ("HideToolbar", Object::Bool(true)),
            ("FitWindow", Object::name("true")),
            ("NonFullScreenPageMode", Object::name("PageOnly")),
            ("Direction", Object::string("R2L")),
            ("ViewArea", Object::name("CropBox")),
            ("PrintScaling", Object::name("None")),
            ("Duplex", Object::name("Simplex")),
            ("NumCopies", Object::Integer(2)),
            (
                "PrintPageRange",
                Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(3),
                    Object::Integer(5),
                    Object::Integer(6),
                ]),
            ),
        ]));
        assert_eq!(run(Version::V17, prefs), Ok(()));
    }

    #[test]
    fn invalid_entries() {
        for (key, value) in [
            ("HideToolbar", Object::Integer(1)),
            ("NonFullScreenPageMode", Object::name("Other")),
            ("ViewClip", Object::name("Other")),
            ("Duplex", Object::name("Other")),
            ("NumCopies", Object::Integer(0)),
            (
                "PrintPageRange",
                Object::Array(vec![Object::Integer(3), Object::Integer(3)]),
            ),
            ("PrintPageRange", Object::Array(vec![Object::Integer(3)])),
            ("Direction", Object::name("Up")),
        ] {
            let prefs = Object::Dict(dict(&[(key, value)]));
            let r = run(Version::V17, prefs);
            assert!(
                matches!(&r, Err(e) if matches!(e.root(), Error::Validation(_))),
                "{key}: {r:?}"
            );
        }
    }

    #[test]
    fn enforce_print_scaling() {
        let enforce = Object::Array(vec![Object::name("PrintScaling")]);
        let ok = dict(&[
            ("Enforce", enforce.clone()),
            ("PrintScaling", Object::name("None")),
        ]);
        assert_eq!(run(Version::V20, Object::Dict(ok)), Ok(()));
        let conflict = dict(&[
            ("Enforce", enforce),
            ("PrintScaling", Object::name("AppDefault")),
        ]);
        assert!(run(Version::V20, Object::Dict(conflict)).is_err());
        // Enforce needs PDF 2.0.
        let old = dict(&[("Enforce", Object::Array(vec![Object::name("PrintScaling")]))]);
        assert!(run(Version::V17, Object::Dict(old)).is_err());
    }

    #[test]
    fn an_array_of_names_is_digested() {
        let names = Object::Array(vec![Object::name("HideToolbar")]);
        assert_eq!(run(Version::V17, names), Ok(()));
        let bad = Object::Array(vec![Object::Integer(1)]);
        assert!(run(Version::V17, bad).is_err());
        assert_eq!(run(Version::V17, Object::Array(Vec::new())), Ok(()));
    }
}
