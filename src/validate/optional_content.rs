//! Optional content (Go: validate/optionalContent.go): optional content groups and membership
//! dictionaries (8.11.2), visibility expressions, usage dictionaries, and the `OCProperties`
//! entry of the catalog (8.11.4).
//!
//! Differences from Go, besides those listed in [`super::object`]:
//!
//! * The validators take the dictionary by reference: nothing in this file changes a dictionary.
//! * A cycle in a visibility expression is an [`Error::Validation`] (Go has a plain error value
//!   that nothing tests for).

use super::Validator;
use super::object::{
    OPTIONAL, REQUIRED, object_context, validate_array_entry, validate_dict_entry, validate_entry,
    validate_ind_ref_array_entry, validate_name_array_entry, validate_name_entry,
    validate_string_entry,
};
use crate::error::Error;
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object};
use std::collections::{BTreeMap, BTreeSet};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The context `dict.entry` for an error.
fn at(dict_name: &str, entry_name: &str) -> impl FnOnce(Error) -> Error + use<> {
    let msg = format!("{dict_name}.{entry_name}");
    move |e| e.context(msg)
}

fn member_of(s: &[u8], set: &[&str]) -> bool {
    set.iter().any(|m| m.as_bytes() == s)
}

/// `visibilityExpressionTraversal`: the expressions on the path being validated, and the depth
/// each expression was validated at.
#[derive(Default)]
struct VisibilityTraversal {
    ancestors: BTreeSet<i64>,
    validated: BTreeMap<i64, u64>,
}

/// `visibilityExpressionIdentity`: the object number of a reference, 0 for any other object.
fn visibility_expression_identity(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

impl VisibilityTraversal {
    /// `enter`.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr <= 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Validation(format!(
                "obj#{obj_nr}: circular visibility expression"
            )));
        }
        Ok(())
    }

    /// `leave`.
    fn leave(&mut self, obj_nr: i64) {
        if obj_nr > 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `alreadyValidated`: a shared expression succeeded at `depth` or deeper. A deeper path
    /// needs revalidation to keep the depth checks of the descendants.
    fn already_validated(&self, obj_nr: i64, depth: u64) -> bool {
        if obj_nr <= 0 {
            return false;
        }
        self.validated
            .get(&obj_nr)
            .is_some_and(|done| depth <= *done)
    }

    /// `markValidated`.
    fn mark_validated(&mut self, obj_nr: i64, depth: u64) {
        if obj_nr <= 0 {
            return;
        }
        let deeper = self.validated.get(&obj_nr).is_none_or(|prev| depth > *prev);
        if deeper {
            self.validated.insert(obj_nr, depth);
        }
    }

    /// `validateOperand`: an operand is an optional content group or a visibility expression.
    fn validate_operand(
        &mut self,
        x: &mut XRefTable,
        raw: &Object,
        path: &str,
        depth: u64,
    ) -> Result<(), Error> {
        let o = x
            .dereference(raw)
            .map_err(|e| e.context(format!("{path}: dereference operand")))?
            .clone();
        match o {
            Object::Dict(d) => validate_optional_content_group_dict(x, &d, Version::V15)
                .map_err(|e| e.context(path.to_string())),
            Object::Array(a) => {
                self.validate_array(x, &a, visibility_expression_identity(raw), path, depth)
            }
            other => Err(Error::Validation(format!(
                "{path}: expected optional content group or visibility expression, got {}",
                type_name(&other)
            ))),
        }
    }

    /// `validateArray`: the visibility expression `a`, which is the object `identity` (0 if it
    /// is direct).
    fn validate_array(
        &mut self,
        x: &mut XRefTable,
        a: &[Object],
        identity: i64,
        path: &str,
        depth: u64,
    ) -> Result<(), Error> {
        x.check_recursion_depth("visibility expression", depth)?;
        self.enter(identity)?;
        let r = self.validate_entered(x, a, identity, path, depth);
        self.leave(identity);
        r
    }

    fn validate_entered(
        &mut self,
        x: &mut XRefTable,
        a: &[Object],
        identity: i64,
        path: &str,
        depth: u64,
    ) -> Result<(), Error> {
        if self.already_validated(identity, depth) {
            return Ok(());
        }
        let Some(first) = a.first() else {
            return Err(Error::Validation(format!(
                "{path}: invalid array length 0, expected an operator and one or more operands"
            )));
        };
        let operator = match x
            .dereference(first)
            .map_err(|e| e.context(format!("{path}[0]: dereference operator")))?
        {
            Object::Name(n) if member_of(n, &["And", "Or", "Not"]) => n.clone(),
            _ => {
                return Err(Error::Validation(format!(
                    "{path}[0]: expected And, Or, or Not operator"
                )));
            }
        };
        visibility_expression_cardinality(a, path, &operator)?;
        for (i, operand) in a.iter().enumerate().skip(1) {
            self.validate_operand(x, operand, &format!("{path}[{i}]"), depth.saturating_add(1))
                .map_err(|e| e.context(path.to_string()))?;
        }
        self.mark_validated(identity, depth);
        Ok(())
    }
}

/// `visibilityExpressionCardinality`: `Not` takes exactly one operand, `And` and `Or` one or
/// more.
fn visibility_expression_cardinality(
    a: &[Object],
    path: &str,
    operator: &[u8],
) -> Result<(), Error> {
    let not = operator == b"Not";
    if (not && a.len() == 2) || (!not && a.len() >= 2) {
        return Ok(());
    }
    let expected = if not {
        "Not and exactly one operand"
    } else {
        "an operator and one or more operands"
    };
    Err(Error::Validation(format!(
        "{path}: invalid array length {}, expected {expected}",
        a.len()
    )))
}

/// `validateVisibilityExpressionEntry`: the `VE` entry of a membership dictionary.
fn validate_visibility_expression_entry(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    const DICT_NAME: &str = "OCMDict";
    const ENTRY_NAME: &str = "VE";
    let identity = d
        .find(ENTRY_NAME.as_bytes())
        .map_or(0, visibility_expression_identity);
    let Some(a) = validate_array_entry(x, d, DICT_NAME, ENTRY_NAME, OPTIONAL, Version::V16, None)?
    else {
        return Ok(());
    };
    VisibilityTraversal::default().validate_array(x, &a, identity, "OCMDict.VE", 0)
}

/// `validateOptionalContentGroupIntent` (8.11.2.1): a name or an array of names, each `View`,
/// `Design` or `All`.
fn validate_optional_content_group_intent(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)
        .map_err(at(dict_name, entry_name))?
    else {
        return Ok(());
    };
    let valid = |s: &[u8]| member_of(s, &["View", "Design", "All"]);
    match o {
        Object::Name(n) => {
            if !valid(&n) {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid intent: {}",
                    lossy(&n)
                )));
            }
        }
        Object::Array(a) => {
            for (i, raw) in a.iter().enumerate() {
                if raw.is_null() {
                    continue;
                }
                let n = match x
                    .dereference(raw)
                    .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?
                {
                    Object::Name(n) => n.clone(),
                    _ => {
                        return Err(Error::Validation(format!(
                            "{dict_name}.{entry_name}[{i}]: invalid type"
                        )));
                    }
                };
                if !valid(&n) {
                    return Err(Error::Validation(format!(
                        "{dict_name}.{entry_name}[{i}]: invalid intent: {}",
                        lossy(&n)
                    )));
                }
            }
        }
        _ => {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}: invalid type"
            )));
        }
    }
    Ok(())
}

/// `validateOptionalContentGroupUsageDict` (8.11.4.4): the dictionary is `d`'s entry
/// `entry_name`; its entries are dictionaries.
fn validate_optional_content_group_usage_dict(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)
        .map_err(at(dict_name, entry_name))?
    else {
        return Ok(());
    };
    for name in [
        "CreatorInfo",
        "Language",
        "Export",
        "Zoom",
        "Print",
        "View",
        "User",
        "PageElement",
    ] {
        validate_dict_entry(x, &d1, "OCUsageDict", name, OPTIONAL, since, None)
            .map_err(at("OCUsageDict", name))?;
    }
    Ok(())
}

/// `validateOptionalContentGroupDict` (8.11).
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_optional_content_group_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "optionalContentGroupDict";

    // Type, required, name, OCG
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        REQUIRED,
        since,
        Some(&|s: &[u8]| s == b"OCG"),
    )
    .map_err(at(dict_name, "Type"))?;

    // Name, required, text string
    validate_string_entry(x, d, dict_name, "Name", REQUIRED, since, None)
        .map_err(at(dict_name, "Name"))?;

    // Intent, optional, name or array
    validate_optional_content_group_intent(x, d, dict_name, "Intent", OPTIONAL, since)
        .map_err(at(dict_name, "Intent"))?;

    // Usage, optional, usage dict
    validate_optional_content_group_usage_dict(x, d, dict_name, "Usage", OPTIONAL, since)
}

/// `validateOptionalContentGroupArray`: the optional array `dict_entry` of `d` holds optional
/// content groups (null elements and null groups are skipped).
fn validate_optional_content_group_array(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    dict_entry: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, dict_entry, OPTIONAL, since, None)
        .map_err(at(dict_name, dict_entry))?
    else {
        return Ok(());
    };
    for (i, v) in a.iter().enumerate() {
        if v.is_null() {
            continue;
        }
        let context = object_context(&format!("{dict_name}.{dict_entry}[{i}]"), Some(v));
        let group = x
            .dereference_dict(v)
            .map_err(|e| {
                e.context(format!(
                    "{context}: dereference optional content group dict"
                ))
            })?
            .cloned();
        let Some(group) = group else {
            continue;
        };
        validate_optional_content_group_dict(x, &group, since).map_err(|e| e.context(context))?;
    }
    Ok(())
}

/// `validateOCGs` (8.11.2.2): the `OCGs` entry of a membership dictionary is a group or an array
/// of groups.
fn validate_ocgs(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<(), Error> {
    let (o, _) = d
        .entry(dict_name, entry_name.as_bytes(), OPTIONAL)
        .map_err(at(dict_name, entry_name))?;
    let Some(raw) = o else {
        return Ok(());
    };

    // Version check.
    x.validate_version("OCGs", since)
        .map_err(at(dict_name, entry_name))?;

    let context = object_context(&format!("{dict_name}.{entry_name}"), Some(raw));
    let o = x
        .dereference(raw)
        .map_err(|e| e.context(format!("{context}: dereference")))?
        .clone();
    match o {
        Object::Null => Ok(()),
        Object::Dict(d1) => {
            validate_optional_content_group_dict(x, &d1, since).map_err(|e| e.context(context))
        }
        _ => validate_optional_content_group_array(x, d, dict_name, entry_name, since),
    }
}

/// `validateOptionalContentMembershipDict` (8.11.2.2).
fn validate_optional_content_membership_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "OCMDict";

    // OCGs, optional, dict or array
    validate_ocgs(x, d, dict_name, "OCGs", since).map_err(at(dict_name, "OCGs"))?;

    // P, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "P",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["AllOn", "AnyOn", "AnyOff", "AllOff"])),
    )
    .map_err(at(dict_name, "P"))?;

    // VE, optional, array, since V1.6
    validate_visibility_expression_entry(x, d).map_err(at(dict_name, "VE"))
}

/// `validateOptionalContent`: the optional content entry `entry_name` of `d` (`OC` of an
/// annotation or an XObject), a group or a membership dictionary.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_optional_content(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let raw = d.find(entry_name.as_bytes());
    let context = object_context(&format!("{dict_name}.{entry_name}"), raw);
    let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)
        .map_err(|e| e.context(context.clone()))?
    else {
        return Ok(());
    };

    let t = validate_name_entry(
        x,
        &d1,
        "optionalContent",
        "Type",
        REQUIRED,
        since,
        Some(&|s: &[u8]| s == b"OCG" || s == b"OCMD"),
    )
    .map_err(|e| e.context(format!("{context}.Type")))?;

    if t.as_deref() == Some(b"OCG".as_slice()) {
        return validate_optional_content_group_dict(x, &d1, since).map_err(|e| e.context(context));
    }
    validate_optional_content_membership_dict(x, &d1, since).map_err(|e| e.context(context))
}

/// `validateUsageApplicationDict` (8.11.4.4).
fn validate_usage_application_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "usageAppDict";

    // Event, required, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Event",
        REQUIRED,
        since,
        Some(&|s: &[u8]| member_of(s, &["View", "Print", "Export"])),
    )
    .map_err(at(dict_name, "Event"))?;

    // OCGs, optional, array of content groups
    validate_optional_content_group_array(x, d, dict_name, "OCGs", since)
        .map_err(at(dict_name, "OCGs"))?;

    // Category, required, array of names
    validate_name_array_entry(x, d, dict_name, "Category", REQUIRED, since, None)
        .map_err(at(dict_name, "Category"))?;
    Ok(())
}

/// `validateUsageApplicationDictArray`.
fn validate_usage_application_dict_array(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    dict_entry: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, dict_entry, required, since, None)
        .map_err(at(dict_name, dict_entry))?
    else {
        return Ok(());
    };
    for (i, v) in a.iter().enumerate() {
        if v.is_null() {
            continue;
        }
        let context = object_context(&format!("{dict_name}.{dict_entry}[{i}]"), Some(v));
        let usage = x
            .dereference_dict(v)
            .map_err(|e| e.context(format!("{context}: dereference usage application dict")))?
            .cloned();
        let Some(usage) = usage else {
            continue;
        };
        validate_usage_application_dict(x, &usage, since).map_err(|e| e.context(context))?;
    }
    Ok(())
}

/// `validateOptionalContentConfigBaseState`: `ON` is checked unless the base state is `ON`, and
/// `OFF` unless it is `OFF`.
fn validate_optional_content_config_base_state(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    // BaseState, optional, name
    let base_state = validate_name_entry(
        x,
        d,
        dict_name,
        "BaseState",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["ON", "OFF", "UNCHANGED"])),
    )
    .map_err(at(dict_name, "BaseState"))?;

    let Some(base_state) = base_state else {
        return Ok(());
    };
    if base_state != b"ON" {
        // ON, optional, content group array
        validate_optional_content_group_array(x, d, dict_name, "ON", since)
            .map_err(at(dict_name, "ON"))?;
    }
    if base_state != b"OFF" {
        // OFF, optional, content group array
        validate_optional_content_group_array(x, d, dict_name, "OFF", since)
            .map_err(at(dict_name, "OFF"))?;
    }
    Ok(())
}

/// `validateOptionalContentConfigurationDict` (8.11.4.3).
fn validate_optional_content_configuration_dict(
    x: &mut XRefTable,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "optContentConfigDict";

    // Name, optional, string
    validate_string_entry(x, d, dict_name, "Name", OPTIONAL, since, None)
        .map_err(at(dict_name, "Name"))?;

    // Creator, optional, string
    validate_string_entry(x, d, dict_name, "Creator", OPTIONAL, since, None)
        .map_err(at(dict_name, "Creator"))?;

    validate_optional_content_config_base_state(x, d, dict_name, since)?;

    // Intent, optional, name or array
    validate_optional_content_group_intent(x, d, dict_name, "Intent", OPTIONAL, since)
        .map_err(at(dict_name, "Intent"))?;

    // AS, optional, usage application dicts array
    validate_usage_application_dict_array(x, d, dict_name, "AS", OPTIONAL, since)
        .map_err(at(dict_name, "AS"))?;

    // Order, optional, array
    validate_array_entry(x, d, dict_name, "Order", OPTIONAL, since, None)
        .map_err(at(dict_name, "Order"))?;

    // ListMode, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "ListMode",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| member_of(s, &["AllPages", "VisiblePages"])),
    )
    .map_err(at(dict_name, "ListMode"))?;

    // RBGroups, optional, array
    validate_array_entry(x, d, dict_name, "RBGroups", OPTIONAL, since, None)
        .map_err(at(dict_name, "RBGroups"))?;

    // Locked, optional, array; since V1.6, relaxed V1.5
    validate_optional_content_group_array(x, d, dict_name, "Locked", Version::V15)
        .map_err(at(dict_name, "Locked"))
}

/// `validateOCPropertiesD`: the default configuration, optional in relaxed validation.
fn validate_oc_properties_d(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(x, d, dict_name, "D", OPTIONAL, since, None)
        .map_err(at(dict_name, "D"))?
    else {
        // A missing required entry is a digested spec violation.
        return Ok(());
    };
    validate_optional_content_configuration_dict(x, &d1, since).map_err(at(dict_name, "D"))
}

/// `validateOCProperties` (8.11.4): the `OCProperties` entry of the catalog. Relaxed validation
/// takes the version as 1.4, whatever the caller says.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_oc_properties(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    _since: Version,
) -> Result<(), Error> {
    let since = Version::V14;
    let d = validate_dict_entry(v.x, root, "rootDict", "OCProperties", required, since, None)
        .map_err(at("rootDict", "OCProperties"))?;
    let Some(d) = d.filter(|d| !d.is_empty()) else {
        return Ok(());
    };

    let dict_name = "optContentPropertiesDict";

    // "OCGs" required array of already written indRefs
    validate_ind_ref_array_entry(v.x, &d, dict_name, "OCGs", OPTIONAL, since, None)
        .map_err(at(dict_name, "OCGs"))?;

    // "D" required dict, default viewing optional content configuration dict.
    validate_oc_properties_d(v.x, &d, dict_name, since)?;

    // "Configs" optional array of alternate optional content configuration dicts.
    let configs = validate_array_entry(v.x, &d, dict_name, "Configs", OPTIONAL, since, None)
        .map_err(at(dict_name, "Configs"))?;
    for (i, o) in configs.iter().flatten().enumerate() {
        v.check()?;
        let context = object_context(&format!("{dict_name}.Configs[{i}]"), Some(o));
        let config =
            v.x.dereference_dict(o)
                .map_err(|e| {
                    e.context(format!(
                        "{context}: dereference optional content configuration dict"
                    ))
                })?
                .cloned();
        let Some(config) = config else {
            continue;
        };
        validate_optional_content_configuration_dict(v.x, &config, since)
            .map_err(|e| e.context(context))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::Sentinel;
    use crate::model::Entry;
    use crate::object::Ref;
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

    fn table(objects: Vec<(i64, Object)>) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn ocg() -> Object {
        Object::Dict(dict(&[
            ("Type", Object::name("OCG")),
            ("Name", Object::string("x")),
        ]))
    }

    fn and(operands: &[Object]) -> Object {
        let mut a = vec![Object::name("And")];
        a.extend(operands.iter().cloned());
        Object::Array(a)
    }

    /// An annotation-like holder whose `OC` is the membership dictionary 7, whose `VE` is the
    /// expression 8.
    fn holder_and_objects(mut objects: Vec<(i64, Object)>) -> (Dict, Vec<(i64, Object)>) {
        objects.push((6, ocg()));
        objects.push((
            7,
            Object::Dict(dict(&[
                ("Type", Object::name("OCMD")),
                ("OCGs", r(6)),
                ("VE", r(8)),
            ])),
        ));
        (dict(&[("OC", r(7))]), objects)
    }

    #[test]
    fn a_group_and_a_membership_dict_validate() {
        let (holder, objects) = holder_and_objects(vec![(8, and(&[r(6), r(6)]))]);
        let mut x = table(objects);
        assert!(
            validate_optional_content(&mut x, &holder, "annotDict", "OC", OPTIONAL, Version::V15)
                .is_ok()
        );
        let bad = dict(&[("OC", Object::Dict(dict(&[("Type", Object::name("Other"))])))]);
        assert!(
            validate_optional_content(&mut x, &bad, "annotDict", "OC", OPTIONAL, Version::V15)
                .is_err()
        );
        // A missing optional entry is fine, a missing required one is not.
        let none = Dict::default();
        assert!(
            validate_optional_content(&mut x, &none, "annotDict", "OC", OPTIONAL, Version::V15)
                .is_ok()
        );
        assert!(
            validate_optional_content(&mut x, &none, "annotDict", "OC", REQUIRED, Version::V15)
                .is_err()
        );
    }

    #[test]
    fn visibility_expression_operator_and_cardinality() {
        let mut x = table(vec![(6, ocg())]);
        let mut t = VisibilityTraversal::default();
        let ok = [Object::name("Not"), r(6)];
        assert!(t.validate_array(&mut x, &ok, 0, "p", 0).is_ok());
        let two = [Object::name("Not"), r(6), r(6)];
        assert!(t.validate_array(&mut x, &two, 0, "p", 0).is_err());
        let one = [Object::name("Or")];
        assert!(t.validate_array(&mut x, &one, 0, "p", 0).is_err());
        let empty: [Object; 0] = [];
        assert!(t.validate_array(&mut x, &empty, 0, "p", 0).is_err());
        let op = [Object::name("Xor"), r(6)];
        assert!(t.validate_array(&mut x, &op, 0, "p", 0).is_err());
        let operand = [Object::name("And"), Object::Integer(1)];
        assert!(t.validate_array(&mut x, &operand, 0, "p", 0).is_err());
    }

    #[test]
    fn a_visibility_expression_cycle_is_an_error() {
        let mut x = table(vec![
            (6, ocg()),
            (8, and(&[r(6), r(9)])),
            (9, and(&[r(8), r(6)])),
        ]);
        let mut t = VisibilityTraversal::default();
        let a = [Object::name("And"), r(8)];
        assert!(t.validate_array(&mut x, &a, 0, "p", 0).is_err());
    }

    /// GHSA-vwmc-mhq7-fqhc: 48 levels of expressions that refer twice to the next one validate
    /// once per object, not once per path.
    #[test]
    fn shared_expressions_validate_once() {
        let levels = 48;
        let mut objects = vec![];
        for i in 0..levels {
            objects.push((8 + i, and(&[r(9 + i), r(9 + i)])));
        }
        objects.push((8 + levels, and(&[r(6), r(6)])));
        let (holder, objects) = holder_and_objects(objects);
        let mut x = table(objects);
        assert!(
            validate_optional_content(&mut x, &holder, "annotDict", "OC", OPTIONAL, Version::V15)
                .is_ok()
        );
    }

    /// GHSA-vwmc-mhq7-fqhc: a shared expression validated on a short path is validated again
    /// when a longer path reaches it, so the depth limit applies to its descendants.
    #[test]
    fn a_shared_expression_checks_the_deeper_path() {
        let objects = vec![
            (8, and(&[r(10), r(20)])),
            (10, and(&[r(11), r(6)])),
            (11, and(&[r(12), r(6)])),
            (12, and(&[r(6), r(6)])),
            (20, and(&[r(21), r(6)])),
            (21, and(&[r(22), r(6)])),
            (22, and(&[r(10), r(6)])),
        ];
        let (holder, objects) = holder_and_objects(objects);
        let mut x = table(objects.clone());
        x.conf.limits.max_recursion_depth = 5;
        let err =
            validate_optional_content(&mut x, &holder, "annotDict", "OC", OPTIONAL, Version::V15)
                .expect_err("depth 6 exceeds the limit of 5");
        assert!(err.is(Sentinel::MaxRecursionDepth));
        let mut x = table(objects);
        x.conf.limits.max_recursion_depth = 6;
        assert!(
            validate_optional_content(&mut x, &holder, "annotDict", "OC", OPTIONAL, Version::V15)
                .is_ok()
        );
    }

    #[test]
    fn oc_properties_with_default_configuration() {
        let ocp = dict(&[
            ("OCGs", Object::Array(vec![r(6)])),
            (
                "D",
                Object::Dict(dict(&[
                    ("BaseState", Object::name("OFF")),
                    ("ON", Object::Array(vec![r(6)])),
                    ("Intent", Object::name("View")),
                    ("ListMode", Object::name("AllPages")),
                ])),
            ),
            (
                "Configs",
                Object::Array(vec![Object::Dict(dict(&[("Name", Object::string("c"))]))]),
            ),
        ]);
        let mut root = dict(&[("OCProperties", Object::Dict(ocp))]);
        let mut x = table(vec![(6, ocg())]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_oc_properties(&mut v, &mut root, OPTIONAL, Version::V15).is_ok());

        // An invalid intent, a bad ListMode and a bad base state are rejected.
        for (key, value) in [
            ("Intent", Object::name("Print")),
            ("ListMode", Object::name("Some")),
            ("BaseState", Object::name("Maybe")),
        ] {
            let ocp = dict(&[("D", Object::Dict(dict(&[(key, value)])))]);
            let mut root = dict(&[("OCProperties", Object::Dict(ocp))]);
            assert!(validate_oc_properties(&mut v, &mut root, OPTIONAL, Version::V15).is_err());
        }

        // An absent entry is fine, an empty dictionary too, a wrong type is not.
        let mut empty = Dict::default();
        assert!(validate_oc_properties(&mut v, &mut empty, OPTIONAL, Version::V15).is_ok());
        let mut zero = dict(&[("OCProperties", Object::Dict(Dict::default()))]);
        assert!(validate_oc_properties(&mut v, &mut zero, OPTIONAL, Version::V15).is_ok());
        let mut wrong = dict(&[("OCProperties", Object::Integer(1))]);
        assert!(validate_oc_properties(&mut v, &mut wrong, OPTIONAL, Version::V15).is_err());
    }

    #[test]
    fn an_ocg_needs_type_and_name() {
        let mut x = table(vec![]);
        let good = dict(&[("Type", Object::name("OCG")), ("Name", Object::string("n"))]);
        assert!(validate_optional_content_group_dict(&mut x, &good, Version::V15).is_ok());
        let no_name = dict(&[("Type", Object::name("OCG"))]);
        assert!(validate_optional_content_group_dict(&mut x, &no_name, Version::V15).is_err());
        let bad_intent = dict(&[
            ("Type", Object::name("OCG")),
            ("Name", Object::string("n")),
            (
                "Intent",
                Object::Array(vec![Object::name("View"), Object::name("Nope")]),
            ),
        ]);
        assert!(validate_optional_content_group_dict(&mut x, &bad_intent, Version::V15).is_err());
    }
}
