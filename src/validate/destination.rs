//! Destinations (Go: validate/destination.go, destination_remote.go).
//!
//! Relaxed validation only. Not ported: the merge registration of a named destination in
//! `validateActionDestinationEntry` (only `Cmd` MERGE reaches it), the validation notices, and
//! `validationObjectNumber` with the owner object numbers. `object_context` and
//! `dict_entry_context` (destination.go:34-48) are in `object.rs`.

use super::object::{REQUIRED, dict_entry_context, validate_array_entry, validate_entry};
use crate::error::Error;
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::text::{hex_literal_to_string, string_literal_to_string};
use crate::object::{Dict, Object};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `validateDestinationArrayFirstElement` in relaxed mode: whether the first element is a page
/// or pages dictionary. Anything else (null, no dictionary, a missing or other `Type`) skips the
/// rest of the destination.
fn first_element_is_page(x: &mut XRefTable, first: &Object) -> Result<bool, Error> {
    let type_value = match x.dereference(first) {
        Err(e) => return Err(e.context("destination array[0]: dereference page")),
        Ok(Object::Dict(d)) => d.find(b"Type").cloned(),
        Ok(_) => return Ok(false),
    };
    Ok(matches!(
        x.name_from_value(b"Type", type_value.as_ref()),
        Ok((Some(t), _)) if t == b"Page" || t == b"Pages"
    ))
}

/// `validateDestinationArrayLength`.
fn valid_array_length(a: &[Object]) -> bool {
    (2..=6).contains(&a.len())
}

/// `validateDestType`.
fn validate_dest_type(a: &[Object], dest_type: &[u8]) -> Result<(), Error> {
    let too_long = match dest_type {
        b"Fit" | b"FitH" | b"FitV" | b"FitBH" => false,
        b"FitB" => a.len() > 2,
        b"FitBV" => a.len() > 3,
        b"XYZ" => a.len() > 5,
        b"FitR" => a.len() > 6,
        _ => {
            return Err(Error::Validation(format!(
                "destination array mode: invalid mode \"{}\"",
                lossy(dest_type)
            )));
        }
    };
    if too_long {
        return Err(Error::Validation(format!(
            "destination array mode {}: invalid length {}",
            lossy(dest_type),
            a.len()
        )));
    }
    Ok(())
}

/// `validateDestinationOperands` (12.3.2.2, table 149).
fn validate_destination_operands(
    x: &mut XRefTable,
    a: &[Object],
    name: &[u8],
) -> Result<(), Error> {
    let length = match name {
        b"FitH" | b"FitV" | b"FitBH" | b"FitBV" => 3,
        b"XYZ" => 5,
        b"FitR" => 6,
        _ => 2,
    };
    if a.len() != length {
        return Err(Error::Validation(format!(
            "destination array mode {}: invalid length {}, expected {length}",
            lossy(name),
            a.len()
        )));
    }
    for (i, operand) in a.iter().enumerate().skip(2) {
        let present = x
            .dereference(operand)
            .map(|o| !o.is_null() || name == b"FitR");
        let r = match present {
            Ok(true) => x.dereference_number(operand).map(|_| ()),
            Ok(false) => Ok(()),
            Err(e) => Err(e),
        };
        r.map_err(|e| {
            e.context(format!(
                "destination array mode {} operand[{i}]",
                lossy(name)
            ))
        })?;
    }
    Ok(())
}

/// `validateDestinationArrayMode` (12.3.2.2, table 149). Relaxed validation digests an error of
/// the operands.
fn validate_destination_array_mode(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    let Some(second) = a.get(1) else {
        return Err(Error::Panic("destination array[1]"));
    };
    let name = match x.dereference(second) {
        Err(e) => return Err(e.context("destination array[1]")),
        Ok(Object::Name(n)) => n.clone(),
        Ok(o) => {
            return Err(Error::Validation(format!(
                "destination array[1]: expected name, got {}",
                type_name(o)
            )));
        }
    };
    validate_dest_type(a, &name)?;
    let _ = validate_destination_operands(x, a, &name);
    Ok(())
}

/// `validateDestinationArray` (destination.go:136). Relaxed validation accepts an array of the
/// wrong length.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_destination_array(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    if !valid_array_length(a) {
        return Ok(());
    }
    let Some(first) = a.first() else {
        return Ok(());
    };
    if !first_element_is_page(x, first)? {
        return Ok(());
    }
    validate_destination_array_mode(x, a)
}

/// `validateDestinationDict`: a dictionary with a required array `D`.
fn validate_destination_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let context = dict_entry_context("destination dictionary", "D", d.find(b"D"));
    let a = validate_array_entry(x, d, "DestinationDict", "D", REQUIRED, Version::V10, None)
        .map_err(|e| e.context(context.clone()))?;
    let Some(a) = a else {
        return Ok(());
    };
    validate_destination_array(x, &a).map_err(|e| e.context(context))
}

/// `validateDestination` (destination.go:234): the name of the destination `o` is or refers to
/// (empty for an array or no destination), after validating it. `for_action` forbids a
/// dictionary.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_destination(
    x: &mut XRefTable,
    o: &Object,
    for_action: bool,
) -> Result<String, Error> {
    let target = x
        .dereference(o)
        .map_err(|e| e.context("destination: dereference"))?
        .clone();
    match target {
        Object::Null => Ok(String::new()),
        Object::Name(n) => Ok(lossy(&n)),
        Object::String(s) => string_literal_to_string(&s),
        Object::Hex(h) => hex_literal_to_string(&h),
        Object::Dict(d) => {
            if for_action {
                return Err(Error::Validation(
                    "destination: action destination cannot be dict".to_string(),
                ));
            }
            validate_destination_dict(x, &d)?;
            Ok(String::new())
        }
        Object::Array(a) => {
            validate_destination_array(x, &a)?;
            Ok(String::new())
        }
        other => Err(Error::Validation(format!(
            "destination: unsupported object type {}",
            type_name(&other)
        ))),
    }
}

/// `validateActionDestinationEntry` (12.3.2): the destination entry `entry_name` of an action.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups, with the entry as context.
pub fn validate_action_destination_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let context = dict_entry_context(dict_name, entry_name, raw.as_ref());
    let o = validate_entry(x, d, dict_name, entry_name, required, since)
        .map_err(|e| e.context(context.clone()))?;
    if o.is_none() {
        return Ok(());
    }
    let raw = raw.unwrap_or(Object::Null);
    validate_destination(x, &raw, true)
        .map(|_| ())
        .map_err(|e| e.context(context))
}

/// `validateRemoteDestinationArray`: a destination of a remote or embedded go-to action. Its
/// first element is a page number, which relaxed validation does not check; a wrong array length
/// is accepted.
fn validate_remote_destination_array(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    if !valid_array_length(a) {
        return Ok(());
    }
    let Some(first) = a.first() else {
        return Ok(());
    };
    x.dereference(first)?;
    validate_destination_array_mode(x, a)
}

/// `validateRemoteActionDestinationEntry`: the required destination entry `entry_name` of a
/// remote or embedded go-to action.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups, with the entry as context.
pub fn validate_remote_action_destination_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let context = dict_entry_context(dict_name, entry_name, raw.as_ref());
    let r = match validate_entry(x, d, dict_name, entry_name, REQUIRED, Version::V10) {
        Ok(Some(Object::Array(a))) => validate_remote_destination_array(x, &a),
        Ok(Some(_)) => validate_destination(x, &raw.unwrap_or(Object::Null), true).map(|_| ()),
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    };
    r.map_err(|e| e.context(context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::xref::Entry;

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    #[test]
    fn names_and_strings_are_returned() {
        let mut x = table();
        let n = validate_destination(&mut x, &Object::name("chapter"), false);
        assert_eq!(n, Ok("chapter".to_string()));
        let s = validate_destination(&mut x, &Object::string("target"), true);
        assert_eq!(s, Ok("target".to_string()));
        let null = validate_destination(&mut x, &Object::Null, true);
        assert_eq!(null, Ok(String::new()));
    }

    #[test]
    fn action_destination_cannot_be_a_dict() {
        let mut x = table();
        let o = Object::Dict(Dict::default());
        assert!(validate_destination(&mut x, &o, true).is_err());
        // A dictionary without the required D entry fails for any other destination.
        assert!(validate_destination(&mut x, &o, false).is_err());
    }

    #[test]
    fn array_mode_and_length() {
        let mut x = table();
        let mut page = Dict::default();
        page.set(b"Type", Object::name("Page"));
        x.table.insert(1, Entry::gen0(Object::Dict(page)));
        let page_ref = Object::Ref(crate::object::Ref {
            num: 1,
            generation: 0,
        });

        let ok = [page_ref.clone(), Object::name("Fit")];
        assert_eq!(validate_destination_array(&mut x, &ok), Ok(()));

        let bad_mode = [page_ref.clone(), Object::name("Bogus")];
        assert!(validate_destination_array(&mut x, &bad_mode).is_err());

        let fit_b_too_long = [page_ref.clone(), Object::name("FitB"), Object::Integer(0)];
        assert!(validate_destination_array(&mut x, &fit_b_too_long).is_err());

        // Relaxed: a wrong length, and operands of the wrong count, are digested.
        let one = [page_ref.clone()];
        assert_eq!(validate_destination_array(&mut x, &one), Ok(()));
        let xyz = [page_ref, Object::name("XYZ"), Object::Integer(1)];
        assert_eq!(validate_destination_array(&mut x, &xyz), Ok(()));
    }

    #[test]
    fn first_element_that_is_no_page_skips_the_destination() {
        let mut x = table();
        let a = [Object::Integer(0), Object::name("Bogus")];
        assert_eq!(validate_destination_array(&mut x, &a), Ok(()));
    }

    #[test]
    fn remote_destination_uses_a_page_number() {
        let mut x = table();
        let mut d = Dict::default();
        d.set(
            b"D",
            Object::Array(vec![Object::Integer(0), Object::name("Fit")]),
        );
        assert_eq!(
            validate_remote_action_destination_entry(&mut x, &d, "GoToR", "D"),
            Ok(())
        );
        d.set(
            b"D",
            Object::Array(vec![Object::Integer(0), Object::name("Bogus")]),
        );
        assert!(validate_remote_action_destination_entry(&mut x, &d, "GoToR", "D").is_err());
        let missing = Dict::default();
        assert!(validate_remote_action_destination_entry(&mut x, &missing, "GoToR", "D").is_err());
    }
}
