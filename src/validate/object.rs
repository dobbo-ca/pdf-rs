//! The entry validators and the graph depth records (Go: validate/object.go, graph_depth.go).
//!
//! WP 13 seeds this file with the part that the root table, the info dictionary, the XMP
//! metadata, the properties and the viewer preferences call, and graph_depth.go (the properties
//! need it). WP 14 appends the rest of object.go, array_cardinality.go and tree_traversal.go
//! below it, in this file.
//!
//! Differences from Go, which hold for every validator of this package:
//!
//! * `WithValidationErrorObject` is dropped (as in `model/deref.rs`), and with it every
//!   `ownerObjNr` parameter, `validationObjectNumber`, `validationEntryObjectNumber` and
//!   `validationRootObjectNumber`. Errors keep their cause and get a context message.
//! * Validation is relaxed, always: there is no mode parameter and the strict branches are not
//!   ported (design section 6). The notices and the log (`ShowSkipped`, `ShowRepaired`,
//!   `ShowDigestedSpecViolation`) are not ported either (design section 1, row 13).
//! * A validator that returns a dictionary, an array or a stream returns an owned copy
//!   (streams share their buffers). Go returns the stored map; a caller that relied on a write
//!   through it writes the copy back itself.
//! * `REQUIRED` and `OPTIONAL` are the `bool` constants of Go.

use super::state::PassState;
use crate::env::GoTime;
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Entry, EntryObject, Version, XRefTable};
use crate::object::date::date_time;
use crate::object::text::{hex_literal_to_string, string_literal_to_string, string_or_hex_literal};
use crate::object::{Array, Dict, Object, Ref, StreamDict};
use std::collections::{BTreeMap, BTreeSet};

/// `REQUIRED`: for required dictionary entries.
pub const REQUIRED: bool = true;

/// `OPTIONAL`: for optional dictionary entries.
pub const OPTIONAL: bool = false;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `objectContext` (destination.go:42): `context`, and the object number if `o` is a reference.
pub fn object_context(context: &str, o: Option<&Object>) -> String {
    match o {
        Some(Object::Ref(r)) => format!("{context} obj#{}", r.num),
        _ => context.to_string(),
    }
}

/// `dictEntryContext` (destination.go:34): `dict.entry`, and the object number if `o` is a
/// reference.
pub fn dict_entry_context(dict_name: &str, entry_name: &str, o: Option<&Object>) -> String {
    object_context(&format!("{dict_name}.{entry_name}"), o)
}

fn invalid_type(dict_name: &str, entry_name: &str, o: &Object) -> Error {
    Error::Validation(format!(
        "dict={dict_name} entry={entry_name} invalid type {}",
        type_name(o)
    ))
}

fn invalid_entry(dict_name: &str, entry_name: &str) -> Error {
    Error::Validation(format!(
        "dict={dict_name} entry={entry_name} invalid dict entry"
    ))
}

/// The version check of the entry validators: `ValidateVersion("dict=D entry=E", since)`. The
/// message is built only for the error.
fn check_version(
    x: &XRefTable,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    if x.version()? >= since {
        return Ok(());
    }
    x.validate_version(&format!("dict={dict_name} entry={entry_name}"), since)
}

/// What the entry validators have in common: `d.Entry`, the dereference, the check for a null
/// target and the version check. `None` is an absent, null or (if optional) null-target entry.
fn resolve_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<Object>, Error> {
    let (o, _) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    let Some(o) = o else {
        return Ok(None);
    };
    let o = x.dereference(o)?.clone();
    if o.is_null() {
        if required {
            return Err(Error::Validation(format!(
                "dict={dict_name} required entry={entry_name} is nil"
            )));
        }
        return Ok(None);
    }
    check_version(x, dict_name, entry_name, since)?;
    Ok(Some(o))
}

/// `validateEntry`: the dereferenced entry, `None` if it is absent or null.
///
/// # Errors
/// [`Error::Validation`] for a missing required entry or a version that is too low, and the
/// errors of `XRefTable::dereference`.
pub fn validate_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<Object>, Error> {
    resolve_entry(x, d, dict_name, entry_name, required, since)
}

/// `validateArrayEntry`.
///
/// # Errors
/// As [`validate_entry`], and [`Error::Validation`] for another type or a failed check.
pub fn validate_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(None);
    };
    let a = match o {
        Object::Array(a) => a,
        other => return Err(invalid_type(dict_name, entry_name, &other)),
    };
    if let Some(f) = validate
        && !f(&a)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(a))
}

/// `validateBooleanEntry`.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_boolean_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(bool) -> bool>,
) -> Result<Option<bool>, Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(None);
    };
    let b = match o {
        Object::Bool(b) => b,
        other => return Err(invalid_type(dict_name, entry_name, &other)),
    };
    if let Some(f) = validate
        && !f(b)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(b))
}

/// `validateFlexBooleanEntry`: a boolean, or the name `true` or `false` in any case.
///
/// # Errors
/// The errors of [`validate_name_entry`] if the entry is no boolean.
pub fn validate_flex_boolean_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<bool>, Error> {
    if let Ok(flag) = validate_boolean_entry(x, d, dict_name, entry_name, required, since, None) {
        return Ok(flag);
    }
    let n = validate_name_entry(
        x,
        d,
        dict_name,
        entry_name,
        required,
        since,
        Some(&|s: &[u8]| s.eq_ignore_ascii_case(b"false") || s.eq_ignore_ascii_case(b"true")),
    )?;
    Ok(n.map(|n| n.eq_ignore_ascii_case(b"true")))
}

/// `timeOfDateObject`: the time of the date string `o` is or refers to, `None` for an empty one.
///
/// # Errors
/// [`Error::Validation`] for a string that is no date, and the errors of
/// `XRefTable::dereference_string_or_hex_literal`.
pub fn time_of_date_object(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<Option<GoTime>, Error> {
    let s = x.dereference_string_or_hex_literal(o, since, None)?;
    if s.is_empty() {
        return Ok(None);
    }
    match date_time(s.as_bytes(), true) {
        Some(t) => Ok(Some(t)),
        None => Err(Error::Validation(format!(
            "date object: <{s}> invalid date"
        ))),
    }
}

/// `validateDateObject`: the `DateString` of the date `o` is or refers to, empty for an empty
/// string.
///
/// # Errors
/// As [`time_of_date_object`].
pub fn validate_date_object(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<String, Error> {
    Ok(time_of_date_object(x, o, since)?
        .map(|t| t.date_string())
        .unwrap_or_default())
}

/// `validateDateEntry`.
///
/// # Errors
/// [`Error::Validation`] for a missing required entry, a string that is no date or a version
/// that is too low, and the errors of `XRefTable::dereference_string_or_hex_literal`.
pub fn validate_date_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<GoTime>, Error> {
    let (o, _) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    let Some(o) = o else {
        return Ok(None);
    };
    let s = x.dereference_string_or_hex_literal(o, Version::V10, None)?;
    check_version(x, dict_name, entry_name, since)?;
    if s.is_empty() {
        if required {
            return Err(Error::Validation(format!(
                "dict={dict_name} required entry={entry_name} is nil"
            )));
        }
        return Ok(None);
    }
    match date_time(s.as_bytes(), true) {
        Some(t) => Ok(Some(t)),
        None => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid date <{s}>"
        ))),
    }
}

/// `validateDictEntry`. `validate` is called for a dictionary that is not empty.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&Dict) -> bool>,
) -> Result<Option<Dict>, Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(None);
    };
    let dict = match o {
        Object::Dict(dict) => dict,
        _ => {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid type"
            )));
        }
    };
    if let Some(f) = validate
        && !dict.is_empty()
        && !f(&dict)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(dict))
}

/// `validateIndRefArrayEntry`: an array of references (null elements are accepted).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no reference.
pub fn validate_ind_ref_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, validate)?
    else {
        return Ok(None);
    };
    for (i, o) in a.iter().enumerate() {
        if !matches!(o, Object::Null | Object::Ref(_)) {
            return Err(Error::Validation(format!(
                "indirect reference array: invalid type at index {i}"
            )));
        }
    }
    Ok(Some(a))
}

/// `validateIntegerEntry`.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_integer_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(i64) -> bool>,
) -> Result<Option<i64>, Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(None);
    };
    let i = match o {
        Object::Integer(i) => i,
        _ => {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid type"
            )));
        }
    };
    if let Some(f) = validate
        && !f(i)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(i))
}

/// The elements of `a` that are not null must satisfy `ok` once dereferenced.
fn validate_elements(
    x: &mut XRefTable,
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    ok: fn(&Object) -> bool,
) -> Result<(), Error> {
    for (i, raw) in a.iter().enumerate() {
        let o = x.dereference(raw)?;
        if o.is_null() {
            continue;
        }
        if !ok(o) {
            return Err(Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid type at index {i}"
            )));
        }
    }
    Ok(())
}

/// `validateIntegerArrayEntry`: an array of integers (null elements are accepted).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no integer.
pub fn validate_integer_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(None);
    };
    validate_elements(x, &a, dict_name, entry_name, |o| {
        matches!(o, Object::Integer(_))
    })?;
    if let Some(f) = validate
        && !f(&a)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(a))
}

/// What `validateNameEntry` returns on a failed check: the name, and the error.
#[derive(Debug)]
pub struct NameEntryError {
    /// The name that failed the check; `None` for any other error.
    pub name: Option<Vec<u8>>,
    pub err: Error,
}

/// `validateNameEntry` where the caller needs the name that failed the check (Go returns both).
///
/// # Errors
/// As [`validate_name_entry`].
pub fn validate_name_entry_keep(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[u8]) -> bool>,
) -> Result<Option<Vec<u8>>, NameEntryError> {
    let wrap = |err: Error| NameEntryError { name: None, err };
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since).map_err(wrap)? else {
        return Ok(None);
    };
    let name = match o {
        Object::Name(n) => n,
        other => return Err(wrap(invalid_type(dict_name, entry_name, &other))),
    };
    if let Some(f) = validate
        && (required || !name.is_empty())
        && !f(&name)
    {
        return Err(NameEntryError {
            err: Error::Validation(format!(
                "dict={dict_name} entry={entry_name} invalid dict entry: {}",
                lossy(&name)
            )),
            name: Some(name),
        });
    }
    Ok(Some(name))
}

/// `validateNameEntry`. `validate` is called if the entry is required or the name is not empty.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_name_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[u8]) -> bool>,
) -> Result<Option<Vec<u8>>, Error> {
    validate_name_entry_keep(x, d, dict_name, entry_name, required, since, validate)
        .map_err(|e| e.err)
}

/// `validateNameArrayEntry`: an array of names (null elements are accepted).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no name.
pub fn validate_name_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, validate)?
    else {
        return Ok(None);
    };
    validate_elements(x, &a, dict_name, entry_name, |o| {
        matches!(o, Object::Name(_))
    })?;
    Ok(Some(a))
}

/// `validateNumberEntryToFloat`: the number entry as a float, 0 if there is none. `validate` is
/// called with the value.
///
/// # Errors
/// [`Error::Validation`] for a missing required entry, another type, a null target, a failed
/// check or a version that is too low, and the errors of `XRefTable::dereference`.
pub fn validate_number_entry_to_float(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(f64) -> bool>,
) -> Result<f64, Error> {
    let (o, _) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    let Some(o) = o else {
        return Ok(0.0);
    };
    check_version(x, dict_name, entry_name, since)?;
    let f = match x.dereference(o)? {
        Object::Integer(i) => *i as f64,
        Object::Float(f) => *f,
        Object::Null => {
            return Err(Error::Validation(
                "number object: missing object".to_string(),
            ));
        }
        other => {
            return Err(Error::Validation(format!(
                "number object: invalid type {}",
                type_name(other)
            )));
        }
    };
    if let Some(v) = validate
        && !v(f)
    {
        return Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid dict entry: {f}"
        )));
    }
    Ok(f)
}

/// `validateStreamDictEntry`: the stream dictionary of the entry, `None` if the entry is absent,
/// refers to nothing or refers to a stream that was validated already.
///
/// # Errors
/// [`Error::Validation`] for a missing required entry or a failed check, [`Error::Corrupt`] for
/// a null entry or another type, and the errors of `XRefTable::dereference_stream_dict`.
pub fn validate_stream_dict_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&StreamDict) -> bool>,
) -> Result<Option<StreamDict>, Error> {
    let (o, found) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    if o.is_none() && !found {
        return Ok(None);
    }
    // A null entry: relaxed validation deletes it from `d` (which the caller owns a copy of)
    // and `DereferenceStreamDict` then fails on the null object.
    let (sd, valid) = x.dereference_stream_dict(o.unwrap_or(&Object::Null))?;
    if valid {
        return Ok(None);
    }
    let Some(sd) = sd else {
        if required {
            return Err(Error::Validation(format!(
                "dict={dict_name} required entry={entry_name} is nil"
            )));
        }
        return Ok(None);
    };
    check_version(x, dict_name, entry_name, since)?;
    if let Some(f) = validate
        && !f(&sd)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(sd))
}

/// `decodeString`: the text of a string or hex literal.
fn decode_string(o: &Object, dict_name: &str, entry_name: &str) -> Result<String, Error> {
    match o {
        Object::String(s) => string_literal_to_string(s),
        Object::Hex(h) => hex_literal_to_string(h),
        other => Err(invalid_type(dict_name, entry_name, other)),
    }
}

/// `validateStringEntry`. `validate` is called if the entry is required or the text is not
/// empty.
///
/// # Errors
/// As [`validate_array_entry`], and the UTF-16 errors of the text conversion.
pub fn validate_string_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&str) -> bool>,
) -> Result<Option<String>, Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(None);
    };
    let s = decode_string(&o, dict_name, entry_name)?;
    if let Some(f) = validate
        && (required || !s.is_empty())
        && !f(&s)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(s))
}

/// `validateStringOrStreamEntry`.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_string_or_stream_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::String(_) | Object::Hex(_) | Object::Stream(_) => Ok(()),
        _ => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid type"
        ))),
    }
}

/// `validateNameOrArrayOfNameEntry`.
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no name.
pub fn validate_name_or_array_of_name_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::Name(_) => Ok(()),
        Object::Array(a) => validate_elements(x, &a, dict_name, entry_name, |o| {
            matches!(o, Object::Name(_))
        }),
        _ => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid type"
        ))),
    }
}

/// `validateBooleanOrArrayOfBooleanEntry`.
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no boolean.
pub fn validate_boolean_or_array_of_boolean_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::Bool(_) => Ok(()),
        Object::Array(a) => validate_elements(x, &a, dict_name, entry_name, |o| {
            matches!(o, Object::Bool(_))
        }),
        _ => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid type"
        ))),
    }
}

/// `graphDepths` (graph_depth.go:23-26): the resource and pattern graph depths of a validation
/// step. The two graphs have their own depth limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphDepths {
    pub resource: i64,
    pub pattern: i64,
}

/// `currentGraphDepths`.
pub fn current_graph_depths(st: &PassState) -> GraphDepths {
    GraphDepths {
        resource: st.resource_depth,
        pattern: st.pattern_depth,
    }
}

/// `validatedGraph` (graph_depth.go:42): the objects whose validation succeeded within one
/// traversal. A shared object is validated once per path depth instead of once per path.
#[derive(Debug, Clone, Default)]
pub struct ValidatedGraph(BTreeMap<i64, GraphDepths>);

impl ValidatedGraph {
    /// `alreadyValidated`: `obj_nr` succeeded at depths at least as deep as `d`. A deeper path
    /// needs revalidation to keep the descendant depth checks.
    pub fn already_validated(&self, obj_nr: i64, d: GraphDepths) -> bool {
        if obj_nr <= 0 {
            return false;
        }
        self.0
            .get(&obj_nr)
            .is_some_and(|done| d.resource <= done.resource && d.pattern <= done.pattern)
    }

    /// `markValidated`: records a success of `obj_nr` at depths `d`. The limits are independent,
    /// so successes at different depths combine per graph.
    pub fn mark_validated(&mut self, obj_nr: i64, mut d: GraphDepths) {
        if obj_nr <= 0 {
            return;
        }
        if let Some(done) = self.0.get(&obj_nr) {
            d.resource = d.resource.max(done.resource);
            d.pattern = d.pattern.max(done.pattern);
        }
        self.0.insert(obj_nr, d);
    }
}

// ---------------------------------------------------------------------------------------------
// The rest of object.go (WP 14)
//
// Not ported, because nothing calls them: `validateBooleanOrStreamEntry`,
// `validateStreamDictOrDictEntry` and `validateFunctionOrArrayOfFunctionsEntry` (object.go:1491,
// :1539, :470; the last one needs the function traversal of function.go, WP 20).

/// The shared body of the `X or Y` entry validators: the dereferenced entry must satisfy `ok`.
fn validate_entry_type(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    ok: fn(&Object) -> bool,
) -> Result<(), Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    if ok(&o) {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "dict={dict_name} entry={entry_name} invalid type"
    )))
}

/// The elements of `a` that are not null must satisfy `ok` once dereferenced; `what` names the
/// array in the message.
fn check_elements(
    x: &mut XRefTable,
    a: &[Object],
    what: &str,
    ok: fn(&Object) -> bool,
) -> Result<(), Error> {
    for (i, raw) in a.iter().enumerate() {
        let o = x.dereference(raw)?;
        if o.is_null() {
            continue;
        }
        if !ok(o) {
            return Err(Error::Validation(format!(
                "{what}: invalid type at index {i}"
            )));
        }
    }
    Ok(())
}

fn is_number(o: &Object) -> bool {
    matches!(o, Object::Integer(_) | Object::Float(_))
}

fn number_value(o: &Object) -> f64 {
    match o {
        Object::Integer(i) => *i as f64,
        Object::Float(f) => *f,
        _ => 0.0,
    }
}

/// `validateBooleanArrayEntry`: an array of booleans (null elements are accepted).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no boolean.
pub fn validate_boolean_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(None);
    };
    validate_elements(x, &a, dict_name, entry_name, |o| {
        matches!(o, Object::Bool(_))
    })?;
    if let Some(f) = validate
        && !f(&a)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(a))
}

/// `validateFloatForObject`: the float `o` is or refers to (an integer is not accepted).
///
/// # Errors
/// [`Error::Validation`] for null, another type or a failed check, and the errors of
/// `XRefTable::dereference`.
pub fn validate_float_for_object(
    x: &mut XRefTable,
    o: &Object,
    validate: Option<&dyn Fn(f64) -> bool>,
) -> Result<f64, Error> {
    let f = match x.dereference(o)? {
        Object::Float(f) => *f,
        Object::Null => {
            return Err(Error::Validation(
                "float object: missing object".to_string(),
            ));
        }
        other => {
            return Err(Error::Validation(format!(
                "float object: invalid type {}",
                type_name(other)
            )));
        }
    };
    if let Some(v) = validate
        && !v(f)
    {
        return Err(Error::Validation(format!("invalid float: {f}")));
    }
    Ok(f)
}

/// `validateIndRefEntry`: the entry must be a reference (it is not dereferenced).
///
/// # Errors
/// [`Error::Validation`] for a missing required entry, another type or a version that is too
/// low.
pub fn validate_ind_ref_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<Ref>, Error> {
    let (o, _) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    let Some(o) = o else {
        return Ok(None);
    };
    let Object::Ref(r) = o else {
        return Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid type"
        )));
    };
    check_version(x, dict_name, entry_name, since)?;
    Ok(Some(*r))
}

/// `validateIntegerForObject`: the integer `o` is or refers to.
///
/// # Errors
/// [`Error::Validation`] for null, another type or a failed check, and the errors of
/// `XRefTable::dereference`.
pub fn validate_integer_for_object(
    x: &mut XRefTable,
    o: &Object,
    validate: Option<&dyn Fn(i64) -> bool>,
) -> Result<i64, Error> {
    let i = match x.dereference(o)? {
        Object::Integer(i) => *i,
        Object::Null => {
            return Err(Error::Validation(
                "integer object: missing object".to_string(),
            ));
        }
        other => {
            return Err(Error::Validation(format!(
                "integer object: invalid type {}",
                type_name(other)
            )));
        }
    };
    if let Some(v) = validate
        && !v(i)
    {
        return Err(Error::Validation(format!("invalid integer: {i}")));
    }
    Ok(i)
}

/// `validateNameForObject`: the name `o` is or refers to.
///
/// # Errors
/// [`Error::Validation`] for null, another type or a failed check, and the errors of
/// `XRefTable::dereference`.
pub fn validate_name_for_object(
    x: &mut XRefTable,
    o: &Object,
    validate: Option<&dyn Fn(&[u8]) -> bool>,
) -> Result<Vec<u8>, Error> {
    let name = match x.dereference(o)? {
        Object::Name(n) => n.clone(),
        Object::Null => {
            return Err(Error::Validation("name object: missing object".to_string()));
        }
        other => {
            return Err(Error::Validation(format!(
                "name object: invalid type {}",
                type_name(other)
            )));
        }
    };
    if let Some(v) = validate
        && !v(&name)
    {
        return Err(Error::Validation(format!("invalid name: {}", lossy(&name))));
    }
    Ok(name)
}

/// `validateNameArray`: the array `o` is or refers to, `None` for null. Its elements must be
/// names (null elements are accepted).
///
/// # Errors
/// [`Error::Validation`] for an element that is no name, and the errors of
/// `XRefTable::dereference_array`.
pub fn validate_name_array(x: &mut XRefTable, o: &Object) -> Result<Option<Array>, Error> {
    let Some(a) = x.dereference_array(o)? else {
        return Ok(None);
    };
    let a = a.clone();
    check_elements(x, &a, "name array", |o| matches!(o, Object::Name(_)))?;
    Ok(Some(a))
}

/// `validateNumberForObject`: the number `o` is or refers to, as an integer or float object.
///
/// # Errors
/// [`Error::Validation`] for null or another type, and the errors of `XRefTable::dereference`.
pub fn validate_number_for_object(x: &mut XRefTable, o: &Object) -> Result<Object, Error> {
    match x.dereference(o)? {
        Object::Integer(i) => Ok(Object::Integer(*i)),
        Object::Float(f) => Ok(Object::Float(*f)),
        Object::Null => Err(Error::Validation(
            "number object: missing object".to_string(),
        )),
        other => Err(Error::Validation(format!(
            "number object: invalid type {}",
            type_name(other)
        ))),
    }
}

/// `validateNumberEntry`: the number entry as an integer or float object. `validate` is called
/// with the value. The version is checked before the entry is dereferenced.
///
/// # Errors
/// As [`validate_number_entry_to_float`].
pub fn validate_number_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(f64) -> bool>,
) -> Result<Option<Object>, Error> {
    let (o, _) = d.entry(dict_name, entry_name.as_bytes(), required)?;
    let Some(o) = o else {
        return Ok(None);
    };
    check_version(x, dict_name, entry_name, since)?;
    let n = validate_number_for_object(x, o)?;
    let f = number_value(&n);
    if let Some(v) = validate
        && !v(f)
    {
        return Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid dict entry: {f}"
        )));
    }
    Ok(Some(n))
}

/// `validateNumberArray`: the array `o` is or refers to, `None` for null. Its elements must be
/// numbers (null elements are accepted).
///
/// # Errors
/// [`Error::Validation`] for an element that is no number, and the errors of
/// `XRefTable::dereference_array`.
pub fn validate_number_array(x: &mut XRefTable, o: &Object) -> Result<Option<Array>, Error> {
    let Some(a) = x.dereference_array(o)? else {
        return Ok(None);
    };
    let a = a.clone();
    check_elements(x, &a, "number array", is_number)?;
    Ok(Some(a))
}

/// `validateNumberArrayEntry`: an array of numbers (null elements are accepted).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no number.
pub fn validate_number_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, None)? else {
        return Ok(None);
    };
    validate_elements(x, &a, dict_name, entry_name, is_number)?;
    if let Some(f) = validate
        && !f(&a)
    {
        return Err(invalid_entry(dict_name, entry_name));
    }
    Ok(Some(a))
}

/// `validateRectangleEntry` (7.9.5): four numbers. A coordinate that is null once dereferenced
/// is replaced by 0 (`validateRectangleCoordinates`, relaxed). Go repairs the stored array:
/// here the repair is written back to the object if the entry is a reference, and the returned
/// array is the repaired copy, which a caller with a direct array writes back itself.
///
/// # Errors
/// As [`validate_number_array_entry`], and [`Error::Validation`] if `validate` fails.
pub fn validate_rectangle_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(mut a) = validate_number_array_entry(
        x,
        d,
        dict_name,
        entry_name,
        required,
        since,
        Some(&|a: &[Object]| a.len() == 4),
    )?
    else {
        return Ok(None);
    };
    let mut repaired = false;
    for slot in a.iter_mut() {
        let is_null = x.dereference(slot)?.is_null();
        if is_null {
            *slot = Object::Integer(0);
            repaired = true;
        }
    }
    if repaired
        && let Some(Object::Ref(r)) = d.find(entry_name.as_bytes())
        && let Some(Entry {
            object: Some(EntryObject::Obj(stored @ Object::Array(_))),
            ..
        }) = x.table.get_mut(&r.num)
    {
        *stored = Object::Array(a.clone());
    }
    if let Some(f) = validate
        && !f(&a)
    {
        return Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid rectangle entry"
        )));
    }
    Ok(Some(a))
}

/// `validateStreamDictForObject`: the stream dictionary `o` is or refers to (a clone that shares
/// its buffers).
///
/// # Errors
/// [`Error::Validation`] for null or another type, and the errors of `XRefTable::dereference`.
pub fn validate_stream_dict_for_object(x: &mut XRefTable, o: &Object) -> Result<StreamDict, Error> {
    match x.dereference(o)? {
        Object::Stream(sd) => Ok((**sd).clone()),
        Object::Null => Err(Error::Validation("stream dict: missing object".to_string())),
        other => Err(Error::Validation(format!(
            "stream dict: invalid type {}",
            type_name(other)
        ))),
    }
}

/// `validateStringArrayEntry`: an array of strings or hex strings (null elements are accepted).
/// Unlike the other array validators `validate` is called before the elements are checked.
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no string.
pub fn validate_string_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, validate)?
    else {
        return Ok(None);
    };
    for (i, raw) in a.iter().enumerate() {
        let context = || {
            object_context(
                &format!("dict={dict_name} entry={entry_name}[{i}]"),
                Some(raw),
            )
        };
        let o = x
            .dereference(raw)
            .map_err(|e| e.context(format!("{}: dereference", context())))?;
        match o {
            Object::Null | Object::String(_) | Object::Hex(_) => {}
            other => {
                return Err(Error::Validation(format!(
                    "{}: invalid type {}, expected types.StringLiteral or types.HexLiteral",
                    context(),
                    type_name(other)
                )));
            }
        }
    }
    Ok(Some(a))
}

/// `validateArrayArrayEntry`: an array of arrays (null elements are accepted). `validate` is
/// called before the elements are checked.
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no array.
pub fn validate_array_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    validate: Option<&dyn Fn(&[Object]) -> bool>,
) -> Result<Option<Array>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, validate)?
    else {
        return Ok(None);
    };
    check_elements(x, &a, "array array", |o| matches!(o, Object::Array(_)))?;
    Ok(Some(a))
}

/// `validateNameOrStringEntry`.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_name_or_string_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_entry_type(x, d, dict_name, entry_name, required, since, |o| {
        matches!(o, Object::String(_) | Object::Hex(_) | Object::Name(_))
    })
}

/// `validateIntOrStringEntry`.
///
/// # Errors
/// As [`validate_array_entry`].
pub fn validate_int_or_string_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_entry_type(x, d, dict_name, entry_name, required, since, |o| {
        matches!(o, Object::String(_) | Object::Hex(_) | Object::Integer(_))
    })
}

/// `validateIntegerOrArrayOfIntegerEntry`.
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for an element that is no integer.
pub fn validate_integer_or_array_of_integer_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = resolve_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match o {
        Object::Integer(_) => Ok(()),
        Object::Array(a) => validate_elements(x, &a, dict_name, entry_name, |o| {
            matches!(o, Object::Integer(_))
        }),
        _ => Err(Error::Validation(format!(
            "dict={dict_name} entry={entry_name} invalid type"
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// array_cardinality.go

/// `arrayCardinalityError`: `dict.entry` has `actual` elements where `expected` was required.
pub fn array_cardinality_error(
    dict_name: &str,
    entry_name: &str,
    actual: usize,
    expected: &str,
) -> Error {
    Error::Validation(format!(
        "{dict_name}.{entry_name}: invalid array length {actual}, expected {expected}"
    ))
}

/// `validateArrayExactLength`.
///
/// # Errors
/// [`Error::Validation`] if `a` has another length.
pub fn validate_array_exact_length(
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    length: usize,
) -> Result<(), Error> {
    if a.len() == length {
        return Ok(());
    }
    Err(array_cardinality_error(
        dict_name,
        entry_name,
        a.len(),
        &length.to_string(),
    ))
}

/// `validateArrayAllowedLengths`.
///
/// # Errors
/// [`Error::Validation`] if the length of `a` is not among `lengths`.
pub fn validate_array_allowed_lengths(
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    lengths: &[usize],
) -> Result<(), Error> {
    if lengths.contains(&a.len()) {
        return Ok(());
    }
    let list = lengths
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    Err(array_cardinality_error(
        dict_name,
        entry_name,
        a.len(),
        &format!("one of [{list}]"),
    ))
}

/// `validateArrayPairs`: complete pairs and at least `min_pairs` of them, without looking at the
/// element types.
///
/// # Errors
/// [`Error::Validation`] for an odd length or too few pairs.
pub fn validate_array_pairs(
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    min_pairs: usize,
) -> Result<(), Error> {
    if a.len().is_multiple_of(2) && a.len() / 2 >= min_pairs {
        return Ok(());
    }
    Err(array_cardinality_error(
        dict_name,
        entry_name,
        a.len(),
        &format!("complete pairs, minimum pair count {min_pairs}"),
    ))
}

/// `validateMultiLanguageTextEntry`: an array of (language identifier, text) pairs without
/// duplicate language identifiers (compared in lower case).
///
/// # Errors
/// As [`validate_string_array_entry`], and [`Error::Validation`] for a bad pair count, a
/// language identifier that is no string or a duplicate one.
pub fn validate_multi_language_text_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_string_array_entry(x, d, dict_name, entry_name, OPTIONAL, since, None)?
    else {
        return Ok(());
    };
    validate_array_pairs(&a, dict_name, entry_name, 1)?;
    let mut seen = BTreeSet::new();
    for (i, raw) in a.iter().enumerate().step_by(2) {
        let o = x.dereference(raw).map_err(|e| {
            e.context(format!(
                "{dict_name}.{entry_name}[{i}]: dereference language identifier"
            ))
        })?;
        let language = string_or_hex_literal(o).map_err(|_| {
            Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: expected language identifier string"
            ))
        })?;
        if !seen.insert(language.to_lowercase()) {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: duplicate language identifier {language:?}"
            )));
        }
    }
    Ok(())
}

/// `validateUnitIntervalArrayEntry`: an optional array of numbers whose length is one of
/// `lengths`. A value outside 0 to 1 is accepted (the strict failure of Go only yields a notice).
///
/// # Errors
/// As [`validate_array_entry`], and [`Error::Validation`] for a bad length or an element that
/// is no number.
pub fn validate_unit_interval_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
    lengths: &[usize],
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, OPTIONAL, since, None)? else {
        return Ok(());
    };
    match lengths {
        [length] => validate_array_exact_length(&a, dict_name, entry_name, *length)?,
        _ => validate_array_allowed_lengths(&a, dict_name, entry_name, lengths)?,
    }
    for (i, o) in a.iter().enumerate() {
        x.dereference_number(o)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]: expected number")))?;
    }
    Ok(())
}

/// `validateColorArrayEntry`: 0, 1, 3 or 4 numbers.
///
/// # Errors
/// As [`validate_unit_interval_array_entry`].
pub fn validate_color_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    validate_unit_interval_array_entry(x, d, dict_name, entry_name, since, &[0, 1, 3, 4])
}

// ---------------------------------------------------------------------------------------------
// tree_traversal.go

/// `treeVisit`: the state of one name tree or number tree traversal. It rejects cycles and
/// nodes that are reachable from more than one parent. (Go's `treeTraversal` helper, which
/// picks a caller's visit or makes one, is `Option::unwrap_or_else` at the call site.)
#[derive(Debug, Clone)]
pub struct TreeVisit {
    ancestors: BTreeSet<i64>,
    seen: BTreeSet<i64>,
    cycle: Sentinel,
    duplicate: Sentinel,
}

impl TreeVisit {
    /// `newTreeVisit`: a visit of a name tree (`name_tree`) or a number tree. A root that is a
    /// reference counts as entered.
    pub fn new(name_tree: bool, root: Option<&Object>) -> TreeVisit {
        let (cycle, duplicate) = if name_tree {
            (Sentinel::NameTreeCycle, Sentinel::NameTreeDuplicate)
        } else {
            (Sentinel::NumberTreeCycle, Sentinel::NumberTreeDuplicate)
        };
        let mut v = TreeVisit {
            ancestors: BTreeSet::new(),
            seen: BTreeSet::new(),
            cycle,
            duplicate,
        };
        if let Some(Object::Ref(r)) = root {
            v.ancestors.insert(r.num);
            v.seen.insert(r.num);
        }
        v
    }

    /// `enter`: the object number of `o` if it is a reference (0 for a direct object, which is
    /// not tracked).
    ///
    /// # Errors
    /// The cycle or duplicate sentinel of the tree kind, with the object number as context.
    pub fn enter(&mut self, o: &Object) -> Result<i64, Error> {
        let Object::Ref(r) = o else {
            return Ok(0);
        };
        let n = r.num;
        if self.ancestors.contains(&n) {
            return Err(Error::Sentinel(self.cycle).context(format!("obj#{n}")));
        }
        if self.seen.contains(&n) {
            return Err(Error::Sentinel(self.duplicate).context(format!("obj#{n}")));
        }
        self.ancestors.insert(n);
        self.seen.insert(n);
        Ok(n)
    }

    /// `leave`: leaves the node `n` that [`TreeVisit::enter`] returned.
    pub fn leave(&mut self, n: i64) {
        self.ancestors.remove(&n);
    }
}

/// `fatalTreeError`: errors that end a tree walk even where a failing subtree is otherwise
/// skipped: the recursion limit, and the cycle and duplicate errors of both tree kinds.
pub fn fatal_tree_error(err: &Error) -> bool {
    err.is(Sentinel::MaxRecursionDepth)
        || err.is(Sentinel::NameTreeCycle)
        || err.is(Sentinel::NameTreeDuplicate)
        || err.is(Sentinel::NumberTreeCycle)
        || err.is(Sentinel::NumberTreeDuplicate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

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

    fn ints(v: &[i64]) -> Object {
        Object::Array(v.iter().map(|i| Object::Integer(*i)).collect())
    }

    fn is_validation<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::Validation(_)))
    }

    #[test]
    fn array_lengths() {
        let a = vec![Object::Null, Object::Null, Object::Null];
        assert!(validate_array_exact_length(&a, "D", "E", 3).is_ok());
        let e = validate_array_exact_length(&a, "D", "E", 2).unwrap_err();
        assert_eq!(
            e.to_string(),
            "pdf: validation: D.E: invalid array length 3, expected 2"
        );
        assert!(validate_array_allowed_lengths(&a, "D", "E", &[0, 3]).is_ok());
        let e = validate_array_allowed_lengths(&a, "D", "E", &[1, 4]).unwrap_err();
        assert_eq!(
            e.to_string(),
            "pdf: validation: D.E: invalid array length 3, expected one of [1 4]"
        );
        assert!(validate_array_pairs(&a[..2], "D", "E", 1).is_ok());
        assert!(validate_array_pairs(&a, "D", "E", 1).is_err());
        assert!(validate_array_pairs(&[], "D", "E", 1).is_err());
        assert!(validate_array_pairs(&[], "D", "E", 0).is_ok());
    }

    #[test]
    fn a_rectangle_repairs_null_coordinates() {
        let mut x = table();
        let d = dict(&[(
            "Rect",
            Object::Array(vec![
                Object::Integer(1),
                Object::Null,
                Object::Float(2.5),
                Object::Integer(4),
            ]),
        )]);
        let a = validate_rectangle_entry(&mut x, &d, "D", "Rect", REQUIRED, Version::V10, None)
            .unwrap()
            .unwrap();
        assert!(matches!(a[1], Object::Integer(0)));
        let short = dict(&[("Rect", ints(&[1, 2, 3]))]);
        assert!(is_validation(&validate_rectangle_entry(
            &mut x,
            &short,
            "D",
            "Rect",
            REQUIRED,
            Version::V10,
            None
        )));
        let absent = Dict::default();
        assert!(
            validate_rectangle_entry(&mut x, &absent, "D", "Rect", OPTIONAL, Version::V10, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_rectangle_repair_reaches_an_indirect_array() {
        let mut x = table();
        x.table.insert(
            7,
            Entry::gen0(Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Null,
                Object::Integer(9),
            ])),
        );
        let d = dict(&[(
            "Rect",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        )]);
        validate_rectangle_entry(&mut x, &d, "D", "Rect", REQUIRED, Version::V10, None).unwrap();
        let stored = x.table.get(&7).and_then(|e| e.object.as_ref());
        match stored {
            Some(EntryObject::Obj(Object::Array(a))) => {
                assert!(matches!(a[2], Object::Integer(0)));
            }
            _ => panic!("array expected"),
        }
    }

    #[test]
    fn string_array_and_multi_language_text() {
        let mut x = table();
        let ok = dict(&[(
            "Text",
            Object::Array(vec![
                Object::string("en"),
                Object::string("hello"),
                Object::string("de"),
                Object::string("hallo"),
            ]),
        )]);
        assert!(validate_multi_language_text_entry(&mut x, &ok, "D", "Text", Version::V10).is_ok());
        let dup = dict(&[(
            "Text",
            Object::Array(vec![
                Object::string("en"),
                Object::string("a"),
                Object::string("EN"),
                Object::string("b"),
            ]),
        )]);
        assert!(is_validation(&validate_multi_language_text_entry(
            &mut x,
            &dup,
            "D",
            "Text",
            Version::V10
        )));
        let odd = dict(&[("Text", Object::Array(vec![Object::string("en")]))]);
        assert!(is_validation(&validate_multi_language_text_entry(
            &mut x,
            &odd,
            "D",
            "Text",
            Version::V10
        )));
        let bad = dict(&[("Text", Object::Array(vec![Object::Integer(1)]))]);
        assert!(is_validation(&validate_string_array_entry(
            &mut x,
            &bad,
            "D",
            "Text",
            REQUIRED,
            Version::V10,
            None
        )));
    }

    #[test]
    fn colour_arrays() {
        let mut x = table();
        for (v, ok) in [
            (ints(&[]), true),
            (ints(&[1]), true),
            (ints(&[0, 1, 0]), true),
            (ints(&[0, 1]), false),
            // Out of range is accepted in relaxed mode.
            (ints(&[5, 5, 5, 5]), true),
        ] {
            let d = dict(&[("C", v)]);
            let r = validate_color_array_entry(&mut x, &d, "D", "C", Version::V10);
            assert_eq!(r.is_ok(), ok);
        }
        let d = dict(&[("C", Object::Array(vec![Object::name("x")]))]);
        assert!(
            validate_unit_interval_array_entry(&mut x, &d, "D", "C", Version::V10, &[1]).is_err()
        );
    }

    #[test]
    fn typed_entries() {
        let mut x = table();
        let d = dict(&[
            ("N", Object::name("a")),
            ("I", Object::Integer(3)),
            ("S", Object::string("s")),
            (
                "R",
                Object::Ref(Ref {
                    num: 3,
                    generation: 0,
                }),
            ),
            ("A", ints(&[1, 2])),
        ]);
        assert!(
            validate_name_or_string_entry(&mut x, &d, "D", "N", REQUIRED, Version::V10).is_ok()
        );
        assert!(
            validate_name_or_string_entry(&mut x, &d, "D", "I", REQUIRED, Version::V10).is_err()
        );
        assert!(validate_int_or_string_entry(&mut x, &d, "D", "I", REQUIRED, Version::V10).is_ok());
        assert!(validate_int_or_string_entry(&mut x, &d, "D", "S", REQUIRED, Version::V10).is_ok());
        assert!(
            validate_integer_or_array_of_integer_entry(
                &mut x,
                &d,
                "D",
                "A",
                REQUIRED,
                Version::V10
            )
            .is_ok()
        );
        assert!(
            validate_integer_or_array_of_integer_entry(
                &mut x,
                &d,
                "D",
                "S",
                REQUIRED,
                Version::V10
            )
            .is_err()
        );
        assert_eq!(
            validate_ind_ref_entry(&mut x, &d, "D", "R", REQUIRED, Version::V10).unwrap(),
            Some(Ref {
                num: 3,
                generation: 0
            })
        );
        assert!(is_validation(&validate_ind_ref_entry(
            &mut x,
            &d,
            "D",
            "I",
            REQUIRED,
            Version::V10
        )));
        let n = validate_number_entry(&mut x, &d, "D", "I", REQUIRED, Version::V10, None).unwrap();
        assert!(matches!(n, Some(Object::Integer(3))));
        assert!(
            validate_float_for_object(&mut x, &Object::Integer(1), None).is_err(),
            "an integer is no float"
        );
        assert_eq!(
            validate_integer_for_object(&mut x, &Object::Integer(4), Some(&|i: i64| i == 4))
                .unwrap(),
            4
        );
        assert!(validate_name_for_object(&mut x, &Object::Null, None).is_err());
    }

    #[test]
    fn tree_visits_reject_cycles_and_duplicates() {
        let r = |n| {
            Object::Ref(Ref {
                num: n,
                generation: 0,
            })
        };
        let mut v = TreeVisit::new(true, Some(&r(1)));
        assert_eq!(v.enter(&Object::Integer(3)).unwrap(), 0);
        let e = v.enter(&r(1)).unwrap_err();
        assert!(e.is(Sentinel::NameTreeCycle));
        assert_eq!(e.to_string(), "obj#1: circular name tree");
        assert!(fatal_tree_error(&e));
        let n = v.enter(&r(2)).unwrap();
        v.leave(n);
        let e = v.enter(&r(2)).unwrap_err();
        assert!(e.is(Sentinel::NameTreeDuplicate));
        let mut v = TreeVisit::new(false, None);
        v.enter(&r(5)).unwrap();
        let e = v.enter(&r(5)).unwrap_err();
        assert!(e.is(Sentinel::NumberTreeCycle));
        assert!(!fatal_tree_error(&Error::Validation("x".to_string())));
        assert!(fatal_tree_error(&Error::Sentinel(
            Sentinel::MaxRecursionDepth
        )));
    }
}
