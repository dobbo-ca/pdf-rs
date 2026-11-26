//! Shadings (Go: validate/shading.go, shading_arrays.go), see 8.7.4.3.
//!
//! Relaxed validation (design section 6): a `Shading` resource dictionary may be older than
//! version 1.3 (the minimum is 1.2), and `showDigestedVersionViolation` is not ported.
//!
//! The colour space of a shading is validated by `colorspace::validate_color_space_entry` (WP
//! 19), its component count comes from `xobject::color_space_components` (WP 19, Go's
//! image_arrays.go), and the group of a soft mask is validated by
//! `xobject::validate_xobject_stream_dict_contents` (called from extgstate.rs).
//!
//! A null coordinate of a direct `BBox` array is repaired to 0 in a copy of the dictionary only
//! (see [`validate_rectangle_entry`]); Go repairs the stored array.

use super::Validator;
use super::colorspace::{ColorSpaceRole, validate_color_space_entry};
use super::function::{FunctionTraversal, function_object_identity, validate_function_objects};
use super::object::{
    OPTIONAL, REQUIRED, array_cardinality_error, object_context, validate_array_entry,
    validate_array_exact_length, validate_array_pairs, validate_boolean_array_entry,
    validate_boolean_entry, validate_entry, validate_integer_entry, validate_number_array_entry,
    validate_rectangle_entry, validate_stream_dict_for_object,
};
use super::xobject::color_space_components;
use crate::error::Error;
use crate::model::Version;
use crate::model::XRefTable;
use crate::model::deref::type_name;
use crate::object::{Dict, Object, StreamDict};
use std::collections::BTreeSet;

fn entry_ctx(e: Error, dict_name: &str, entry_name: &str) -> Error {
    e.context(format!("{dict_name}.{entry_name}"))
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `validateBitsPerComponent`.
fn valid_bits_per_component(i: i64) -> bool {
    matches!(i, 1 | 2 | 4 | 8 | 12 | 16)
}

/// `validateBitsPerCoordinate`.
fn valid_bits_per_coordinate(i: i64) -> bool {
    matches!(i, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32)
}

/// `validateBitsPerFlag`.
fn valid_bits_per_flag(i: i64) -> bool {
    matches!(i, 2 | 4 | 8)
}

/// The component count of the colour space entry `key` of `d`, 0 if there is none.
fn colour_space_components(x: &mut XRefTable, d: &Dict, key: &[u8]) -> Result<usize, Error> {
    color_space_components(x, d.find(key).unwrap_or(&Object::Null))
}

// ---------------------------------------------------------------------------------------------
// shading_arrays.go

/// `validateNumericArrayValues`: every element is a number.
fn validate_numeric_array_values(
    v: &mut Validator<'_>,
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
) -> Result<(), Error> {
    for (i, o) in a.iter().enumerate() {
        v.check()?;
        v.x.dereference_number(o)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]: expected number")))?;
    }
    Ok(())
}

/// `validateComponentArrayEntry`: an optional array of numbers, one per colour component if the
/// component count is known.
fn validate_component_array_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    components: usize,
) -> Result<(), Error> {
    let Some(a) =
        validate_array_entry(v.x, d, dict_name, entry_name, OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };
    if components > 0 {
        validate_array_exact_length(&a, dict_name, entry_name, components)?;
    }
    validate_numeric_array_values(v, &a, dict_name, entry_name)
}

/// `singleOutputFunctions`: checks function graphs that are validated already, without
/// expanding a shared subfunction twice. Every function of a `Function` array must have one
/// output.
#[derive(Default)]
struct SingleOutputFunctions {
    checked: BTreeSet<i64>,
}

impl SingleOutputFunctions {
    fn validate(&mut self, v: &mut Validator<'_>, o: &Object, depth: u64) -> Result<(), Error> {
        v.check()?;
        v.x.check_recursion_depth("shading function outputs", depth)?;
        let identity = function_object_identity(o);
        if identity > 0 && self.checked.contains(&identity) {
            return Ok(());
        }
        let d = match v.x.dereference(o)? {
            Object::Dict(d) => d.clone(),
            Object::Stream(sd) => sd.dict.clone(),
            other => {
                return Err(Error::Validation(format!(
                    "expected function dictionary or stream, got {}",
                    type_name(other)
                )));
            }
        };
        self.validate_dict(v, &d, depth)?;
        if identity > 0 {
            self.checked.insert(identity);
        }
        Ok(())
    }

    fn validate_dict(&mut self, v: &mut Validator<'_>, d: &Dict, depth: u64) -> Result<(), Error> {
        let function_type =
            v.x.dereference_integer(d.find(b"FunctionType").unwrap_or(&Object::Null))?;
        let Some(function_type) = function_type else {
            return Err(Error::Validation("missing FunctionType".to_string()));
        };
        if function_type == 2 {
            let c0 =
                v.x.dereference_array(d.find(b"C0").unwrap_or(&Object::Null))?
                    .cloned();
            let Some(c0) = c0 else {
                return Ok(());
            };
            return validate_array_exact_length(&c0, "shadingFunction", "C0", 1);
        }
        let range =
            v.x.dereference_array(d.find(b"Range").unwrap_or(&Object::Null))?
                .cloned()
                .unwrap_or_default();
        if function_type != 3 || !range.is_empty() {
            validate_array_exact_length(&range, "shadingFunction", "Range", 2)?;
        }
        if function_type == 3 {
            return self.validate_children(v, d, depth);
        }
        Ok(())
    }

    fn validate_children(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        depth: u64,
    ) -> Result<(), Error> {
        let a =
            v.x.dereference_array(d.find(b"Functions").unwrap_or(&Object::Null))?
                .cloned()
                .unwrap_or_default();
        for (i, f) in a.iter().enumerate() {
            self.validate(v, f, depth.saturating_add(1))
                .map_err(|e| e.context(format!("Functions[{i}]")))?;
        }
        Ok(())
    }
}

/// `validateShadingFunctionEntry`: the `Function` entry, one function or an array of functions
/// with one output each. Says whether the entry is present.
fn validate_shading_function_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    required: bool,
    components: usize,
) -> Result<bool, Error> {
    v.check()?;
    let Some(o) = validate_entry(v.x, d, dict_name, "Function", required, Version::V10)? else {
        return Ok(false);
    };
    if let Object::Array(a) = &o {
        if a.is_empty() {
            return Err(array_cardinality_error(
                dict_name,
                "Function",
                0,
                "one or more functions",
            ));
        }
        if components > 0 {
            validate_array_exact_length(a, dict_name, "Function", components)?;
        }
    }
    let raw = d.find(b"Function").cloned().unwrap_or(Object::Null);
    validate_function_objects(v, &mut FunctionTraversal::default(), &raw, &o)?;
    if let Object::Array(a) = &o {
        let mut t = SingleOutputFunctions::default();
        for (i, f) in a.iter().enumerate() {
            t.validate(v, f, 0)
                .map_err(|e| e.context(format!("{dict_name}.Function[{i}]")))?;
        }
    }
    Ok(true)
}

/// `validateMeshShadingArrays`: `Decode` has the 4 coordinate bounds and two values per colour
/// component (one if there is a `Function`).
fn validate_mesh_shading_arrays(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    components: usize,
) -> Result<(), Error> {
    let a = validate_array_entry(v.x, d, dict_name, "Decode", REQUIRED, Version::V10, None)
        .map_err(|e| entry_ctx(e, dict_name, "Decode"))?
        .unwrap_or_default();
    let present = validate_shading_function_entry(v, d, dict_name, OPTIONAL, components)
        .map_err(|e| entry_ctx(e, dict_name, "Function"))?;
    let components = if present { 1 } else { components };
    if components == 0 {
        // Without a known colour space, still require the coordinate pairs and at least one
        // colour pair.
        validate_array_pairs(&a, dict_name, "Decode", 3)?;
    } else if a.len() < 4 || !(a.len() - 4).is_multiple_of(2) || (a.len() - 4) / 2 != components {
        return Err(array_cardinality_error(
            dict_name,
            "Decode",
            a.len(),
            &format!("4 coordinate values and two values per component ({components} components)"),
        ));
    }
    validate_numeric_array_values(v, &a, dict_name, "Decode")
}

/// `validateSoftMaskBackdrop`: `BC` of a soft mask. It is consulted only for a luminosity mask,
/// where it has one value per component of the colour space of the group `G`. `g` is the group
/// as the stream-entry validator returned it (`None` for a group validated already).
///
/// # Errors
/// [`Error::Validation`] for a `BC` that does not fit, and the errors of the lookups.
pub fn validate_soft_mask_backdrop(
    v: &mut Validator<'_>,
    d: &Dict,
    g: Option<&StreamDict>,
    luminosity: bool,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(v.x, d, "softMaskDict", "BC", OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };
    if luminosity {
        let fetched;
        let sd = match g {
            Some(sd) => sd,
            None => {
                fetched =
                    validate_stream_dict_for_object(v.x, d.find(b"G").unwrap_or(&Object::Null))
                        .map_err(|e| e.context("softMaskDict.G"))?;
                &fetched
            }
        };
        let components =
            soft_mask_backdrop_components(v.x, sd).map_err(|e| e.context("softMaskDict.BC"))?;
        validate_array_exact_length(&a, "softMaskDict", "BC", components)?;
    }
    validate_numeric_array_values(v, &a, "softMaskDict", "BC")
}

/// `softMaskBackdropComponents`: the component count of the colour space of the transparency
/// group of `sd`.
fn soft_mask_backdrop_components(x: &mut XRefTable, sd: &StreamDict) -> Result<usize, Error> {
    let group = x
        .dereference_dict(sd.dict.find(b"Group").unwrap_or(&Object::Null))?
        .cloned();
    let Some(group) = group else {
        return Err(Error::Validation(
            "G.Group: missing transparency group attributes".to_string(),
        ));
    };
    let components = colour_space_components(x, &group, b"CS")?;
    if components == 0 {
        return Err(Error::Validation(
            "G.Group.CS: colour space required for luminosity backdrop".to_string(),
        ));
    }
    Ok(components)
}

// ---------------------------------------------------------------------------------------------
// shading.go

/// `shadingInfo`.
struct ShadingInfo {
    kind: i64,
    components: usize,
}

/// `validateShadingDictCommonEntries`.
fn validate_shading_dict_common_entries(
    v: &mut Validator<'_>,
    dict: &Dict,
) -> Result<ShadingInfo, Error> {
    let dict_name = "shadingDictCommonEntries";

    let shading_type = validate_integer_entry(
        v.x,
        dict,
        dict_name,
        "ShadingType",
        REQUIRED,
        Version::V10,
        Some(&|i: i64| (1..=7).contains(&i)),
    )
    .map_err(|e| entry_ctx(e, dict_name, "ShadingType"))?;

    validate_color_space_entry(
        v,
        dict,
        dict_name,
        "ColorSpace",
        OPTIONAL,
        ColorSpaceRole::NoPattern,
    )
    .map_err(|e| entry_ctx(e, dict_name, "ColorSpace"))?;

    let components = colour_space_components(v.x, dict, b"ColorSpace")
        .map_err(|e| entry_ctx(e, dict_name, "ColorSpace"))?;
    validate_component_array_entry(v, dict, dict_name, "Background", components)
        .map_err(|e| entry_ctx(e, dict_name, "Background"))?;

    validate_rectangle_entry(v.x, dict, dict_name, "BBox", OPTIONAL, Version::V10, None)
        .map_err(|e| entry_ctx(e, dict_name, "BBox"))?;

    validate_boolean_entry(
        v.x,
        dict,
        dict_name,
        "AntiAlias",
        OPTIONAL,
        Version::V10,
        None,
    )
    .map_err(|e| entry_ctx(e, dict_name, "AntiAlias"))?;

    Ok(ShadingInfo {
        kind: shading_type.unwrap_or_default(),
        components,
    })
}

/// `validateFunctionBasedShadingDict` (type 1).
fn validate_function_based_shading_dict(
    v: &mut Validator<'_>,
    dict: &Dict,
    components: usize,
) -> Result<(), Error> {
    let dict_name = "functionBasedShadingDict";
    validate_number_array_entry(
        v.x,
        dict,
        dict_name,
        "Domain",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 4),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Domain"))?;
    validate_number_array_entry(
        v.x,
        dict,
        dict_name,
        "Matrix",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 6),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Matrix"))?;
    validate_shading_function_entry(v, dict, dict_name, REQUIRED, components)
        .map_err(|e| entry_ctx(e, dict_name, "Function"))?;
    Ok(())
}

/// `validateAxialShadingDict` (type 2, `coords` = 4) and `validateRadialShadingDict` (type 3,
/// `coords` = 6).
fn validate_axial_or_radial_shading_dict(
    v: &mut Validator<'_>,
    dict: &Dict,
    dict_name: &str,
    coords: usize,
    components: usize,
) -> Result<(), Error> {
    validate_number_array_entry(
        v.x,
        dict,
        dict_name,
        "Coords",
        REQUIRED,
        Version::V10,
        Some(&|a: &[Object]| a.len() == coords),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Coords"))?;
    validate_number_array_entry(
        v.x,
        dict,
        dict_name,
        "Domain",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Domain"))?;
    validate_shading_function_entry(v, dict, dict_name, REQUIRED, components)
        .map_err(|e| entry_ctx(e, dict_name, "Function"))?;
    validate_boolean_array_entry(
        v.x,
        dict,
        dict_name,
        "Extend",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Extend"))?;
    Ok(())
}

/// The mesh shadings: free-form (4) and Coons and tensor-product patch meshes (6, 7) have
/// `BitsPerFlag`; the lattice-form mesh (5) has `VerticesPerRow` instead.
fn validate_mesh_shading_dict(
    v: &mut Validator<'_>,
    dict: &Dict,
    dict_name: &str,
    lattice: bool,
    components: usize,
) -> Result<(), Error> {
    validate_integer_entry(
        v.x,
        dict,
        dict_name,
        "BitsPerCoordinate",
        REQUIRED,
        Version::V10,
        Some(&valid_bits_per_coordinate),
    )
    .map_err(|e| entry_ctx(e, dict_name, "BitsPerCoordinate"))?;
    validate_integer_entry(
        v.x,
        dict,
        dict_name,
        "BitsPerComponent",
        REQUIRED,
        Version::V10,
        Some(&valid_bits_per_component),
    )
    .map_err(|e| entry_ctx(e, dict_name, "BitsPerComponent"))?;
    if lattice {
        validate_integer_entry(
            v.x,
            dict,
            dict_name,
            "VerticesPerRow",
            REQUIRED,
            Version::V10,
            Some(&|i: i64| i >= 2),
        )
        .map_err(|e| entry_ctx(e, dict_name, "VerticesPerRow"))?;
    } else {
        validate_integer_entry(
            v.x,
            dict,
            dict_name,
            "BitsPerFlag",
            REQUIRED,
            Version::V10,
            Some(&valid_bits_per_flag),
        )
        .map_err(|e| entry_ctx(e, dict_name, "BitsPerFlag"))?;
    }
    validate_mesh_shading_arrays(v, dict, dict_name, components)
}

/// `validateShadingDict`: the shadings of types 1 to 3.
fn validate_shading_dict(v: &mut Validator<'_>, dict: &Dict) -> Result<(), Error> {
    let info =
        validate_shading_dict_common_entries(v, dict).map_err(|e| e.context("shading dict"))?;
    let r = match info.kind {
        1 => validate_function_based_shading_dict(v, dict, info.components),
        2 => validate_axial_or_radial_shading_dict(v, dict, "axialShadingDict", 4, info.components),
        3 => {
            validate_axial_or_radial_shading_dict(v, dict, "radialShadingDict", 6, info.components)
        }
        kind => {
            return Err(Error::Validation(format!("unexpected shadingType: {kind}")));
        }
    };
    r.map_err(|e| e.context(format!("shading dict type {}", info.kind)))
}

/// `validateShadingStreamDict`: the shadings of types 2 and 4 to 7.
fn validate_shading_stream_dict(v: &mut Validator<'_>, dict: &Dict) -> Result<(), Error> {
    let info = validate_shading_dict_common_entries(v, dict)
        .map_err(|e| e.context("shading stream dict"))?;
    let c = info.components;
    let r = match info.kind {
        2 => validate_axial_or_radial_shading_dict(v, dict, "axialShadingDict", 4, c),
        4 => {
            validate_mesh_shading_dict(v, dict, "freeFormGouraudShadedTriangleMeshesDict", false, c)
        }
        5 => validate_mesh_shading_dict(
            v,
            dict,
            "latticeFormGouraudShadedTriangleMeshesDict",
            true,
            c,
        ),
        6 => validate_mesh_shading_dict(v, dict, "coonsPatchMeshesDict", false, c),
        7 => validate_mesh_shading_dict(v, dict, "tensorProductPatchMeshesDict", false, c),
        kind => {
            return Err(Error::Validation(format!("unexpected shadingType: {kind}")));
        }
    };
    r.map_err(|e| e.context(format!("shading stream dict type {}", info.kind)))
}

/// `validateShading`: validates the shading dictionary or stream `o` is or refers to, see
/// 8.7.4.3.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an entry that is not valid, and the errors of
/// the lookups.
pub fn validate_shading(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    let resolved =
        v.x.dereference(o)
            .map_err(|e| e.context("shading: dereference"))?
            .clone();
    match &resolved {
        Object::Null => Ok(()),
        Object::Dict(d) => validate_shading_dict(v, d).map_err(|e| e.context("shading dictionary")),
        Object::Stream(sd) => {
            validate_shading_stream_dict(v, &sd.dict).map_err(|e| e.context("shading stream"))
        }
        other => Err(Error::Validation(format!(
            "shading: expected dict or stream dict, got {}",
            type_name(other)
        ))),
    }
}

/// `validateShadingResourceDict`: the `Shading` resource dictionary, a dictionary of shadings.
/// The names are visited in order. Relaxed validation accepts version 1.2 where `since` is
/// higher.
///
/// # Errors
/// As [`validate_shading`], and [`Error::Validation`] if the version is too low.
pub fn validate_shading_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    let version = if v.x.version()? < since {
        Version::V12
    } else {
        since
    };
    v.x.validate_version("shadingResourceDict", version)
        .map_err(|e| e.context("shadingResourceDict"))?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("shadingResourceDict: dereference"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };
    for (name, o) in d.iter() {
        v.check()?;
        validate_shading(v, o).map_err(|e| {
            e.context(object_context(
                &format!("shadingResourceDict.{}", lossy(name)),
                Some(o),
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::{Array, Ref};
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

    fn dict_of(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn numbers(n: &[i64]) -> Array {
        n.iter().map(|i| Object::Integer(*i)).collect()
    }

    fn exponential(c0: &[i64]) -> Object {
        Object::Dict(dict_of(&[
            ("FunctionType", Object::Integer(2)),
            ("Domain", Object::Array(numbers(&[0, 1]))),
            ("C0", Object::Array(numbers(c0))),
            ("C1", Object::Array(numbers(c0))),
            ("N", Object::Integer(1)),
        ]))
    }

    #[test]
    fn a_function_entry_is_one_function_or_single_output_functions() {
        let mut x = table(vec![(1, exponential(&[0])), (2, exponential(&[0, 1]))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);

        let single = dict_of(&[("Function", r(1))]);
        assert_eq!(
            validate_shading_function_entry(&mut v, &single, "axialShadingDict", REQUIRED, 3),
            Ok(true)
        );

        let ok = dict_of(&[("Function", Object::Array(vec![r(1), r(1), r(1)]))]);
        assert_eq!(
            validate_shading_function_entry(&mut v, &ok, "axialShadingDict", REQUIRED, 3),
            Ok(true)
        );

        // The count must match the colour components, and every function has one output.
        assert!(
            validate_shading_function_entry(&mut v, &ok, "axialShadingDict", REQUIRED, 4).is_err()
        );
        let two = dict_of(&[("Function", Object::Array(vec![r(2)]))]);
        assert!(
            validate_shading_function_entry(&mut v, &two, "axialShadingDict", REQUIRED, 1).is_err()
        );

        let empty = dict_of(&[("Function", Object::Array(vec![]))]);
        assert!(
            validate_shading_function_entry(&mut v, &empty, "axialShadingDict", REQUIRED, 0)
                .is_err()
        );

        let none = Dict::default();
        assert_eq!(
            validate_shading_function_entry(&mut v, &none, "axialShadingDict", OPTIONAL, 0),
            Ok(false)
        );
        assert!(
            validate_shading_function_entry(&mut v, &none, "axialShadingDict", REQUIRED, 0)
                .is_err()
        );
    }

    #[test]
    fn mesh_decode_arrays_follow_the_component_count() {
        let mut x = table(vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let decode = |n: usize| dict_of(&[("Decode", Object::Array(numbers(&vec![0; n])))]);
        // 4 coordinate values and 3 components of 2 values.
        assert!(validate_mesh_shading_arrays(&mut v, &decode(10), "meshDict", 3).is_ok());
        assert!(validate_mesh_shading_arrays(&mut v, &decode(8), "meshDict", 3).is_err());
        // An unknown colour space needs the coordinates and one colour pair.
        assert!(validate_mesh_shading_arrays(&mut v, &decode(6), "meshDict", 0).is_ok());
        assert!(validate_mesh_shading_arrays(&mut v, &decode(5), "meshDict", 0).is_err());
        // A missing Decode is an error.
        assert!(validate_mesh_shading_arrays(&mut v, &Dict::default(), "meshDict", 3).is_err());
    }

    #[test]
    fn a_shading_that_is_no_dictionary_is_an_error() {
        let mut x = table(vec![(1, Object::Integer(1)), (2, Object::Null)]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_shading(&mut v, &r(1)).is_err());
        assert!(validate_shading(&mut v, &r(2)).is_ok());
    }

    #[test]
    fn a_shading_resource_dict_checks_the_version() {
        let mut x = table(vec![]);
        x.header_version = Some(Version::V11);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let empty = Object::Dict(Dict::default());
        // Relaxed validation lowers the minimum to 1.2, which 1.1 does not reach.
        assert!(validate_shading_resource_dict(&mut v, &empty, Version::V13).is_err());
        v.x.header_version = Some(Version::V12);
        assert!(validate_shading_resource_dict(&mut v, &empty, Version::V13).is_ok());
    }
}
