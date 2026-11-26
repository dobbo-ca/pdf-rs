//! Patterns (Go: validate/pattern.go), see 8.7.
//!
//! The traversal record that Go keeps in the `context.Context` is [`PassState::patterns`]: the
//! outermost call of a pass creates it and removes it when it returns. Its depth is
//! [`PassState::pattern_depth`], which `current_graph_depths` reads. A tiling pattern validates
//! its resources, which can contain patterns again, so the graph is walked with a depth limit, a
//! cycle check and a record of the patterns validated at each depth.
//!
//! Relaxed validation (design section 6) accepts any `PaintType` and `TilingType`, as Go only
//! records a notice. A null coordinate of a direct `BBox` array is repaired to 0 in a copy of the
//! stream dictionary only (see `validate_rectangle_entry`).
//!
//! [`PassState::patterns`]: super::state::PassState
//! [`PassState::pattern_depth`]: super::state::PassState

use super::Validator;
use super::extgstate::validate_extgstate_dict;
use super::object::{
    GraphDepths, OPTIONAL, REQUIRED, ValidatedGraph, current_graph_depths, object_context,
    validate_dict_entry, validate_integer_entry, validate_name_entry, validate_number_array_entry,
    validate_number_entry, validate_rectangle_entry,
};
use super::page::validate_resource_dict;
use super::shading::validate_shading;
use crate::error::{Error, Sentinel};
use crate::model::Version;
use crate::object::{Dict, Object, StreamDict};
use std::collections::BTreeSet;

/// `patternTraversal`: the patterns on the path being validated, and those that were validated
/// already.
#[derive(Debug, Clone, Default)]
pub struct PatternTraversal {
    ancestors: BTreeSet<i64>,
    validated: ValidatedGraph,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn entry_ctx(e: Error, dict_name: &str, entry_name: &str) -> Error {
    e.context(format!("{dict_name}.{entry_name}"))
}

/// `patternObjectIdentity`: the object number of a reference, 0 for a direct object.
fn pattern_object_identity(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

/// `patternTraversalFromContext`: runs `f` with the traversal of the pass, creating it if this
/// is the outermost call.
fn with_traversal<T>(v: &mut Validator<'_>, f: impl FnOnce(&mut Validator<'_>) -> T) -> T {
    let created = v.st.patterns.is_none();
    if created {
        v.st.patterns = Some(PatternTraversal::default());
    }
    let r = f(v);
    if created {
        v.st.patterns = None;
    }
    r
}

/// `patternTraversal.enter`: an object that is on the path already is a cycle.
fn enter(v: &mut Validator<'_>, obj_nr: i64) -> Result<(), Error> {
    if obj_nr <= 0 {
        return Ok(());
    }
    let Some(t) = v.st.patterns.as_mut() else {
        return Ok(());
    };
    if !t.ancestors.insert(obj_nr) {
        return Err(Error::Sentinel(Sentinel::PatternCycle));
    }
    Ok(())
}

/// `patternTraversal.leave`.
fn leave(v: &mut Validator<'_>, obj_nr: i64) {
    if obj_nr > 0
        && let Some(t) = v.st.patterns.as_mut()
    {
        t.ancestors.remove(&obj_nr);
    }
    v.st.pattern_depth = v.st.pattern_depth.saturating_sub(1);
}

/// `validateTilingPatternDict` (8.7.3.1).
fn validate_tiling_pattern_dict(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "tilingPatternDict";
    let d = &sd.dict;
    v.x.validate_version(dict_name, since)
        .map_err(|e| e.context(dict_name))?;

    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Pattern"),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Type"))?;

    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "PatternType",
        REQUIRED,
        since,
        Some(&|i: i64| i == 1),
    )
    .map_err(|e| entry_ctx(e, dict_name, "PatternType"))?;

    // Relaxed validation accepts every value of PaintType (1 or 2) and TilingType (1 to 3).
    validate_integer_entry(v.x, d, dict_name, "PaintType", REQUIRED, since, None)
        .map_err(|e| entry_ctx(e, dict_name, "PaintType"))?;
    validate_integer_entry(v.x, d, dict_name, "TilingType", REQUIRED, since, None)
        .map_err(|e| entry_ctx(e, dict_name, "TilingType"))?;

    validate_rectangle_entry(v.x, d, dict_name, "BBox", REQUIRED, since, None)
        .map_err(|e| entry_ctx(e, dict_name, "BBox"))?;

    validate_number_entry(
        v.x,
        d,
        dict_name,
        "XStep",
        REQUIRED,
        since,
        Some(&|f: f64| f != 0.0),
    )
    .map_err(|e| entry_ctx(e, dict_name, "XStep"))?;
    validate_number_entry(
        v.x,
        d,
        dict_name,
        "YStep",
        REQUIRED,
        since,
        Some(&|f: f64| f != 0.0),
    )
    .map_err(|e| entry_ctx(e, dict_name, "YStep"))?;

    validate_number_array_entry(
        v.x,
        d,
        dict_name,
        "Matrix",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 6),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Matrix"))?;

    let Some(resources) = d.find(b"Resources") else {
        return Err(Error::Validation(format!(
            "{dict_name}.Resources: missing required entry"
        )));
    };
    validate_resource_dict(v, resources).map_err(|e| entry_ctx(e, dict_name, "Resources"))?;
    Ok(())
}

/// `validateShadingPatternDict`.
fn validate_shading_pattern_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "shadingPatternDict";
    v.x.validate_version(dict_name, since)
        .map_err(|e| e.context(dict_name))?;

    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Pattern"),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Type"))?;

    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "PatternType",
        REQUIRED,
        since,
        Some(&|i: i64| i == 2),
    )
    .map_err(|e| entry_ctx(e, dict_name, "PatternType"))?;

    validate_number_array_entry(
        v.x,
        d,
        dict_name,
        "Matrix",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 6),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Matrix"))?;

    let ext_g_state = validate_dict_entry(v.x, d, dict_name, "ExtGState", OPTIONAL, since, None)
        .map_err(|e| entry_ctx(e, dict_name, "ExtGState"))?;
    if ext_g_state.is_some() {
        let raw = d.find(b"ExtGState").cloned().unwrap_or(Object::Null);
        validate_extgstate_dict(v, &raw).map_err(|e| entry_ctx(e, dict_name, "ExtGState"))?;
    }

    // Shading: required, dict or stream dict.
    let Some(shading) = d.find(b"Shading") else {
        return Err(Error::Validation(format!(
            "{dict_name}.Shading: missing required entry"
        )));
    };
    validate_shading(v, shading).map_err(|e| entry_ctx(e, dict_name, "Shading"))
}

/// `validatePattern`: validates the pattern `o` is or refers to, a tiling pattern (a stream) or
/// a shading pattern (a dictionary).
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::MaxRecursionDepth`], [`Sentinel::PatternCycle`],
/// [`Error::Validation`] for an entry that is not valid, and the errors of the dereference.
pub fn validate_pattern(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    with_traversal(v, |v| validate_traversal(v, o))
}

/// `patternTraversal.validate`.
fn validate_traversal(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    v.check()?;
    let depth = v.st.pattern_depth.saturating_add(1);
    v.x.check_recursion_depth("Pattern graph", u64::try_from(depth).unwrap_or(u64::MAX))?;
    let depths = current_graph_depths(&v.st);
    let pattern_obj_nr = pattern_object_identity(o);
    enter(v, pattern_obj_nr)?;
    v.st.pattern_depth = depth;
    let r = validate_entered(v, o, pattern_obj_nr, depths);
    leave(v, pattern_obj_nr);
    r
}

fn validate_entered(
    v: &mut Validator<'_>,
    o: &Object,
    pattern_obj_nr: i64,
    depths: GraphDepths,
) -> Result<(), Error> {
    if v.st
        .patterns
        .as_ref()
        .is_some_and(|t| t.validated.already_validated(pattern_obj_nr, depths))
    {
        return Ok(());
    }

    let resolved =
        v.x.dereference(o)
            .map_err(|e| e.context("pattern: dereference"))?
            .clone();
    match &resolved {
        Object::Null => return Ok(()),
        Object::Stream(sd) => validate_tiling_pattern_dict(v, sd, Version::V10)
            .map_err(|e| e.context("tiling pattern"))?,
        Object::Dict(d) => validate_shading_pattern_dict(v, d, Version::V13)
            .map_err(|e| e.context("shading pattern"))?,
        _ => {
            return Err(Error::Validation(
                "corrupt obj type, must be dict or stream dict".to_string(),
            ));
        }
    }

    if let Some(t) = v.st.patterns.as_mut() {
        t.validated.mark_validated(pattern_obj_nr, depths);
    }
    Ok(())
}

/// `validatePatternResourceDict`: the `Pattern` resource dictionary, a dictionary of patterns.
/// The names are visited in order.
///
/// # Errors
/// As [`validate_pattern`], and [`Error::Validation`] if the version is lower than `since`.
pub fn validate_pattern_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("PatternResourceDict", since)
        .map_err(|e| e.context("patternResourceDict"))?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("patternResourceDict: dereference"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };

    with_traversal(v, |v| {
        for (name, o) in d.iter() {
            validate_pattern(v, o).map_err(|e| {
                e.context(object_context(
                    &format!("patternResourceDict.{}", lossy(name)),
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
    fn a_null_pattern_is_fine_and_another_type_is_not() {
        let mut x = table(vec![(1, Object::Null), (2, Object::Integer(1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_pattern(&mut v, &r(1)).is_ok());
        assert!(matches!(
            validate_pattern(&mut v, &r(2)),
            Err(Error::Validation(_))
        ));
        // The traversal of the outermost call is gone, and the depth is back to 0.
        assert!(v.st.patterns.is_none());
        assert_eq!(v.st.pattern_depth, 0);
    }

    #[test]
    fn a_shading_pattern_needs_its_type_and_a_shading() {
        let mut x = table(vec![
            (1, dict(&[("PatternType", Object::Integer(1))])),
            (2, dict(&[("PatternType", Object::Integer(2))])),
        ]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        // The type is wrong for a dictionary, and Shading is missing.
        assert!(validate_pattern(&mut v, &r(1)).is_err());
        assert!(validate_pattern(&mut v, &r(2)).is_err());
        assert_eq!(v.st.pattern_depth, 0);
    }

    #[test]
    fn a_pattern_on_the_path_is_a_cycle() {
        let mut x = table(vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let r = with_traversal(&mut v, |v| {
            assert!(enter(v, 3).is_ok());
            // A direct pattern is not tracked.
            assert!(enter(v, 0).is_ok());
            let again = enter(v, 3);
            leave(v, 3);
            again
        });
        assert!(r.unwrap_err().is(Sentinel::PatternCycle));
    }

    #[test]
    fn a_cancelled_pass_stops() {
        let mut x = table(vec![(1, Object::Null)]);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        assert_eq!(validate_pattern(&mut v, &r(1)), Err(Error::Cancelled));
    }

    #[test]
    fn a_pattern_resource_dict_checks_the_version_and_every_name() {
        let mut x = table(vec![(1, Object::Integer(1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let empty = dict(&[]);
        assert!(validate_pattern_resource_dict(&mut v, &empty, Version::V10).is_ok());
        assert!(validate_pattern_resource_dict(&mut v, &empty, Version::V20).is_err());
        let bad = dict(&[("P0", r(1))]);
        assert!(validate_pattern_resource_dict(&mut v, &bad, Version::V10).is_err());
    }
}
