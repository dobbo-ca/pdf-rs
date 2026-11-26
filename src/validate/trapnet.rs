//! Trap network states (Go: validate/trapnet_states.go).
//!
//! `validatePageAnnotationDict` (trapnet_states.go:52) calls `validateAnnotationDict` first and
//! checks the state count only if that reports a trap network annotation. That function belongs
//! to annotation.go (WP 16), so the call is not made here: [`validate_trap_net_state_count`] is
//! the part that follows it, which WP 16's page annotation walk calls for a trap network
//! annotation, passing the number of the page annotations before it.

use super::Validator;
use super::object::{OPTIONAL, array_cardinality_error, validate_array_entry};
use crate::error::Error;
use crate::model::Version;
use crate::model::deref::type_name;
use crate::object::{Dict, Object};

/// `validateTrapNetStateValues`: the `AnnotStates` array holds names or nulls.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an element that is neither a name nor null,
/// and the errors of the lookups.
pub fn validate_trap_net_state_values(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(
        v.x,
        d,
        dict_name,
        "AnnotStates",
        OPTIONAL,
        Version::V10,
        None,
    )?
    else {
        return Ok(());
    };
    for (i, raw) in a.iter().enumerate() {
        v.check()?;
        let o =
            v.x.dereference(raw)
                .map_err(|e| e.context(format!("{dict_name}.AnnotStates[{i}]")))?;
        if o.is_null() {
            continue;
        }
        if !matches!(o, Object::Name(_)) {
            return Err(Error::Validation(format!(
                "{dict_name}.AnnotStates[{i}]: expected name or null, got {}",
                type_name(o)
            )));
        }
    }
    Ok(())
}

/// The state count check of `validatePageAnnotationDict` for a trap network annotation `d`: the
/// `AnnotStates` array (if there is one) has one state per page annotation before it,
/// `preceding_count` of them.
///
/// # Errors
/// [`Error::Validation`] for another count, and the errors of
/// `XRefTable::dereference_array`.
pub fn validate_trap_net_state_count(
    v: &mut Validator<'_>,
    d: &Dict,
    preceding_count: usize,
) -> Result<(), Error> {
    let null = Object::Null;
    let o = d.find(b"AnnotStates").unwrap_or(&null);
    let Some(a) = v.x.dereference_array(o)? else {
        return Ok(());
    };
    if a.len() == preceding_count {
        return Ok(());
    }
    Err(array_cardinality_error(
        "annotDict",
        "AnnotStates",
        a.len(),
        &format!("{preceding_count}, one state per page annotation excluding TrapNet"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::XRefTable;
    use std::sync::atomic::AtomicBool;

    fn states(a: Vec<Object>) -> Dict {
        let mut d = Dict::default();
        d.set(b"AnnotStates", Object::Array(a));
        d
    }

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    #[test]
    fn states_are_names_or_null() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let ok = states(vec![Object::name("A"), Object::Null]);
        assert_eq!(
            validate_trap_net_state_values(&mut v, &ok, "annotDict"),
            Ok(())
        );
        let bad = states(vec![Object::Integer(1)]);
        assert!(matches!(
            validate_trap_net_state_values(&mut v, &bad, "annotDict"),
            Err(Error::Validation(_))
        ));
        assert_eq!(
            validate_trap_net_state_values(&mut v, &Dict::default(), "annotDict"),
            Ok(())
        );
    }

    #[test]
    fn the_state_count_must_match() {
        let mut x = table();
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let d = states(vec![Object::name("A"), Object::name("B")]);
        assert_eq!(validate_trap_net_state_count(&mut v, &d, 2), Ok(()));
        let e = validate_trap_net_state_count(&mut v, &d, 3).unwrap_err();
        assert_eq!(
            e.to_string(),
            "pdf: validation: annotDict.AnnotStates: invalid array length 2, expected 3, one state per page annotation excluding TrapNet"
        );
        assert_eq!(
            validate_trap_net_state_count(&mut v, &Dict::default(), 3),
            Ok(())
        );
        let cancelled = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancelled);
        assert_eq!(
            validate_trap_net_state_values(&mut v, &d, "annotDict"),
            Err(Error::Cancelled)
        );
    }
}
