//! Property lists (Go: validate/property.go): the `Properties` resource of a page, form or
//! pattern, and the property lists of marked content.
//!
//! The traversal record that Go keeps in the `context.Context` is [`PassState::props`]: the
//! outermost call of a pass creates it and removes it when it returns.
//!
//! [`PassState::props`]: super::state::PassState

use super::Validator;
use super::metadata::validate_metadata;
use super::object::{
    GraphDepths, OPTIONAL, ValidatedGraph, current_graph_depths, dict_entry_context,
    object_context, validate_string_or_stream_entry,
};
use super::page::validate_resource_dict;
use super::xobject::persist_stream_entries;
use crate::error::Error;
use crate::model::Version;
use crate::object::{Dict, Object};
use std::collections::BTreeSet;

/// `propertiesTraversal`: the property lists on the path being validated, and those that were
/// validated already.
#[derive(Debug, Clone, Default)]
pub struct PropertiesTraversal {
    ancestors: BTreeSet<i64>,
    validated: ValidatedGraph,
}

/// `propertiesObjectIdentity`: the object number of a reference, 0 for a direct object.
fn properties_object_identity(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `propertiesTraversalFromContext`: runs `f` with the traversal of the pass, creating it if
/// this is the outermost call.
fn with_traversal<T>(v: &mut Validator<'_>, f: impl FnOnce(&mut Validator<'_>) -> T) -> T {
    let created = v.st.props.is_none();
    if created {
        v.st.props = Some(PropertiesTraversal::default());
    }
    let r = f(v);
    if created {
        v.st.props = None;
    }
    r
}

/// `propertiesTraversal.enter`: false if `o` is on the path already (a cycle).
fn enter(v: &mut Validator<'_>, o: &Object) -> (i64, bool) {
    let obj_nr = properties_object_identity(o);
    if obj_nr <= 0 {
        return (0, true);
    }
    let Some(t) = v.st.props.as_mut() else {
        return (obj_nr, true);
    };
    (obj_nr, t.ancestors.insert(obj_nr))
}

/// `validatePropertiesDict`: a property list, see 14.6.2. It is a dictionary with private
/// information, and with optional `Metadata`, `Contents` and `Resources` entries. Relaxed
/// validation accepts a stream's dictionary as well.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an entry that is not valid, and the errors of
/// the dereference.
pub fn validate_properties_dict(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    with_traversal(v, |v| validate_traversal(v, o))
}

/// `propertiesTraversal.validate`.
fn validate_traversal(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    v.check()?;

    let (property_obj_nr, entered) = enter(v, o);
    if !entered {
        return Ok(());
    }
    let r = validate_entered(v, o, property_obj_nr);
    if property_obj_nr > 0
        && let Some(t) = v.st.props.as_mut()
    {
        t.ancestors.remove(&property_obj_nr);
    }
    r
}

fn validate_entered(v: &mut Validator<'_>, o: &Object, property_obj_nr: i64) -> Result<(), Error> {
    let depths: GraphDepths = current_graph_depths(&v.st);
    if v.st
        .props
        .as_ref()
        .is_some_and(|t| t.validated.already_validated(property_obj_nr, depths))
    {
        return Ok(());
    }

    let Some(mut d) = dereference_properties_dict(v, o)
        .map_err(|e| e.context(object_context("propertiesDict: dereference", Some(o))))?
    else {
        return Ok(());
    };

    validate_metadata(v.x, &mut d, OPTIONAL, Version::V14).map_err(|e| {
        e.context(dict_entry_context(
            "propertiesDict",
            "Metadata",
            d.find(b"Metadata"),
        ))
    })?;
    // Go deletes a `Metadata` entry that refers to nothing through the alias of the stored map.
    if let Object::Ref(ir) = o
        && d.find(b"Metadata").is_none()
    {
        if let Ok(Some(stored)) = v.x.dereference_dict_mut(*ir) {
            stored.delete(b"Metadata");
        }
        persist_stream_entries(v.x, *ir, &d);
    }
    validate_properties_dict_entries(v, &d)?;
    if let Some(t) = v.st.props.as_mut() {
        t.validated.mark_validated(property_obj_nr, depths);
    }
    Ok(())
}

/// `dereferencePropertiesDict`: the dictionary of the property list, `None` for null. Relaxed
/// validation takes the dictionary of a stream.
fn dereference_properties_dict(v: &mut Validator<'_>, o: &Object) -> Result<Option<Dict>, Error> {
    match v.x.dereference(o)?.clone() {
        Object::Null => Ok(None),
        Object::Dict(d) => Ok(Some(d)),
        Object::Stream(sd) => Ok(Some(sd.dict)),
        _ => {
            // The error of a value that is no dictionary.
            v.x.dereference_dict(o)?;
            Ok(None)
        }
    }
}

/// `validatePropertiesDictEntries`: the entries in key order.
fn validate_properties_dict_entries(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    for (key, val) in d.iter() {
        match key.as_slice() {
            b"Metadata" => {}
            b"Contents" => {
                validate_string_or_stream_entry(
                    v.x,
                    d,
                    "propertiesDict",
                    "Contents",
                    OPTIONAL,
                    Version::V10,
                )
                .map_err(|e| {
                    e.context(dict_entry_context("propertiesDict", "Contents", Some(val)))
                })?;
            }
            b"Resources" => {
                validate_resource_dict(v, val).map_err(|e| {
                    e.context(dict_entry_context("propertiesDict", "Resources", Some(val)))
                })?;
            }
            b"OCG" | b"OCMD" => {
                return Err(Error::Validation(format!(
                    "{}: unsupported key",
                    dict_entry_context("propertiesDict", &lossy(key), Some(val))
                )));
            }
            // MCID, Alt, ActualText, E and Lang, and any other key.
            _ => {
                v.x.dereference(val).map_err(|e| {
                    e.context(format!(
                        "{}: dereference",
                        dict_entry_context("propertiesDict", &lossy(key), Some(val))
                    ))
                })?;
            }
        }
    }
    Ok(())
}

/// `validatePropertiesResourceDict`: the `Properties` resource dictionary, a dictionary of
/// property lists. The names are visited in order.
///
/// # Errors
/// As [`validate_properties_dict`], and [`Error::Validation`] if the version is lower than
/// `since`.
pub fn validate_properties_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("PropertiesResourceDict", since)
        .map_err(|e| e.context("propertiesResourceDict"))?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| {
                e.context(object_context(
                    "propertiesResourceDict: dereference",
                    Some(o),
                ))
            })?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };

    with_traversal(v, |v| {
        for (name, o) in d.iter() {
            v.check()?;
            validate_properties_dict(v, o).map_err(|e| {
                e.context(object_context(
                    &format!("propertiesResourceDict.{}", lossy(name)),
                    Some(o),
                ))
            })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Entry, XRefTable};
    use crate::object::Ref;
    use std::sync::atomic::AtomicBool;

    fn r(n: i64) -> Object {
        Object::Ref(Ref {
            num: n,
            generation: 0,
        })
    }

    fn table(objects: Vec<(i64, Object)>) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn dict(entries: &[(&str, Object)]) -> Object {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        Object::Dict(d)
    }

    #[test]
    fn a_property_list_with_known_and_unknown_entries() {
        let mut x = table(vec![(
            1,
            dict(&[
                ("MCID", Object::Integer(3)),
                ("Contents", Object::string("text")),
                ("Lang", Object::string("en")),
            ]),
        )]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_properties_dict(&mut v, &r(1)).is_ok());
        // The traversal of the outermost call is gone.
        assert!(v.st.props.is_none());
    }

    #[test]
    fn an_ocg_key_is_unsupported() {
        let mut x = table(vec![(1, dict(&[("OCG", Object::Integer(1))]))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(matches!(
            validate_properties_dict(&mut v, &r(1)),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn a_self_reference_and_a_null_list_are_fine() {
        // An unknown key is only dereferenced, and a null list has nothing to validate.
        let mut x = table(vec![(1, dict(&[("Other", r(1))])), (2, Object::Null)]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_properties_dict(&mut v, &r(1)).is_ok());
        assert!(validate_properties_dict(&mut v, &r(2)).is_ok());
    }

    #[test]
    fn a_properties_resource_dict_checks_the_version_and_every_name() {
        let mut x = table(vec![(
            1,
            dict(&[("MC0", dict(&[("MCID", Object::Integer(0))]))]),
        )]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_properties_resource_dict(&mut v, &r(1), Version::V15).is_ok());
        assert!(validate_properties_resource_dict(&mut v, &r(1), Version::V20).is_err());
        // Direct property lists with an OCG entry fail.
        let bad = dict(&[("MC0", dict(&[("OCMD", Object::Null)]))]);
        assert!(validate_properties_resource_dict(&mut v, &bad, Version::V15).is_err());
    }

    #[test]
    fn a_metadata_entry_that_refers_to_nothing_is_deleted_from_the_stored_list() {
        // Go deletes it through the map of the stored object (RA_CI.d5.pdf, obj 9).
        let mut x = table(vec![(1, dict(&[("Metadata", r(7))]))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_properties_dict(&mut v, &r(1)).is_ok());
        let o = r(1);
        let d = x.dereference_dict(&o).unwrap().unwrap();
        assert!(d.find(b"Metadata").is_none());
    }

    #[test]
    fn a_cancelled_pass_stops() {
        let mut x = table(vec![(1, dict(&[]))]);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        assert_eq!(
            validate_properties_dict(&mut v, &r(1)),
            Err(Error::Cancelled)
        );
    }
}
