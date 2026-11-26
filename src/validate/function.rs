//! Functions (Go: validate/function.go, and `validateFunctionObjects` of object.go:446), see
//! 7.10.
//!
//! A function graph is walked with a [`FunctionTraversal`]: the functions on the path (a cycle
//! is an error) and the depth at which each function was validated (a shared function is
//! validated once per depth). Go makes a new traversal for every entry that holds a function;
//! [`validate_function`] does the same.
//!
//! Relaxed validation (design section 6) drops the strict branches: a wrong exponential function
//! domain and a stitching function with a missing last bound or an empty `Range` are accepted
//! (Go records a notice), and `showDigestedVersionViolation` is not ported.

use super::Validator;
use super::object::{
    OPTIONAL, REQUIRED, array_cardinality_error, validate_array_entry, validate_array_pairs,
    validate_integer_array_entry, validate_integer_entry, validate_number_array_entry,
    validate_number_entry_to_float,
};
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Array, Dict, Object, StreamDict};
use std::collections::{BTreeMap, BTreeSet};

fn entry_ctx(e: Error, dict_name: &str, entry_name: &str) -> Error {
    e.context(format!("{dict_name}.{entry_name}"))
}

/// `validateFunctionDimension`: complete pairs, at least one; the dimension is the pair count.
fn function_dimension(a: &[Object], dict_name: &str, entry_name: &str) -> Result<usize, Error> {
    validate_array_pairs(a, dict_name, entry_name, 1)?;
    Ok(a.len() / 2)
}

fn length_error(
    actual: usize,
    dict_name: &str,
    entry_name: &str,
    length: usize,
    relationship: &str,
) -> Error {
    array_cardinality_error(
        dict_name,
        entry_name,
        actual,
        &format!("{length}, {relationship}"),
    )
}

/// `validateFunctionArrayLength`.
fn function_array_length(
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    length: usize,
    relationship: &str,
) -> Result<(), Error> {
    if a.len() == length {
        return Ok(());
    }
    Err(length_error(
        a.len(),
        dict_name,
        entry_name,
        length,
        relationship,
    ))
}

// ---------------------------------------------------------------------------------------------
// Type 2: exponential interpolation

/// `exponentialFunctionArrays`.
struct ExponentialArrays {
    domain: Array,
    range: Option<Array>,
    c0: Option<Array>,
    c1: Option<Array>,
}

fn exponential_arrays(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<ExponentialArrays, Error> {
    let domain =
        validate_number_array_entry(x, d, dict_name, "Domain", REQUIRED, Version::V13, None)
            .map_err(|e| entry_ctx(e, dict_name, "Domain"))?
            .unwrap_or_default();
    let range = validate_number_array_entry(x, d, dict_name, "Range", OPTIONAL, Version::V13, None)
        .map_err(|e| entry_ctx(e, dict_name, "Range"))?;
    let c0 = validate_number_array_entry(x, d, dict_name, "C0", OPTIONAL, Version::V13, None)
        .map_err(|e| entry_ctx(e, dict_name, "C0"))?;
    let c1 = validate_number_array_entry(x, d, dict_name, "C1", OPTIONAL, Version::V13, None)
        .map_err(|e| entry_ctx(e, dict_name, "C1"))?;
    Ok(ExponentialArrays {
        domain,
        range,
        c0,
        c1,
    })
}

/// `validateExponentialOutputDimension`: the length of `C0` and `C1`, 1 if they are absent.
fn exponential_output_dimension(a: &ExponentialArrays, dict_name: &str) -> Result<usize, Error> {
    let c0_length = match &a.c0 {
        Some(c0) if c0.is_empty() => {
            return Err(array_cardinality_error(
                dict_name,
                "C0",
                0,
                "one or more output values",
            ));
        }
        Some(c0) => c0.len(),
        None => 1,
    };
    let c1_length = match &a.c1 {
        Some(c1) if c1.is_empty() => {
            return Err(array_cardinality_error(
                dict_name,
                "C1",
                0,
                "one or more output values",
            ));
        }
        Some(c1) => c1.len(),
        None => 1,
    };
    if c1_length != c0_length {
        return Err(array_cardinality_error(
            dict_name,
            "C1",
            c1_length,
            &format!("{c0_length}, matching C0 output dimension"),
        ));
    }
    Ok(c0_length)
}

/// `validateExponentialFunctionCardinality`.
fn exponential_cardinality(a: &ExponentialArrays, dict_name: &str) -> Result<(), Error> {
    function_array_length(&a.domain, dict_name, "Domain", 2, "one input dimension")?;
    let n = exponential_output_dimension(a, dict_name)?;
    let Some(range) = &a.range else {
        return Ok(());
    };
    function_array_length(
        range,
        dict_name,
        "Range",
        n.saturating_mul(2),
        "twice the output dimension",
    )
}

/// `checkExponentialFunctionDomain` (7.10.3).
fn check_exponential_function_domain(
    x: &mut XRefTable,
    domain: &[Object],
    n: f64,
) -> Result<(), Error> {
    let [lo, hi] = domain else {
        return Ok(());
    };
    let min = x
        .dereference_number(lo)
        .map_err(|e| e.context("Domain[0]: expected number"))?;
    let max = x
        .dereference_number(hi)
        .map_err(|e| e.context("Domain[1]: expected number"))?;
    if min > max {
        return Err(Error::Validation(format!(
            "Domain: lower bound {min} exceeds upper bound {max}"
        )));
    }
    if n.trunc() != n && min < 0.0 {
        return Err(Error::Validation(format!(
            "Domain: negative inputs with non-integer exponent N={n}"
        )));
    }
    if n < 0.0 && min <= 0.0 && max >= 0.0 {
        return Err(Error::Validation(format!(
            "Domain: includes zero with negative exponent N={n}"
        )));
    }
    Ok(())
}

/// `validateExponentialInterpolationFunctionDict` (7.10.3).
fn validate_exponential_interpolation_function_dict(
    x: &mut XRefTable,
    d: &Dict,
) -> Result<(), Error> {
    let dict_name = "exponentialInterpolationFunctionDict";
    x.validate_version(dict_name, Version::V13)
        .map_err(|e| e.context(dict_name))?;
    let a = exponential_arrays(x, d, dict_name)?;
    exponential_cardinality(&a, dict_name)?;
    let n = validate_number_entry_to_float(x, d, dict_name, "N", REQUIRED, Version::V13, None)
        .map_err(|e| entry_ctx(e, dict_name, "N"))?;
    // Relaxed validation only records a notice for a domain that does not fit the exponent.
    let _ = check_exponential_function_domain(x, &a.domain, n);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Type 3: stitching

/// `stitchingFunctionArrays`.
struct StitchingArrays {
    domain: Array,
    range: Option<Array>,
    functions: Array,
    bounds: Array,
    encode: Array,
}

fn stitching_arrays(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<StitchingArrays, Error> {
    let domain =
        validate_number_array_entry(x, d, dict_name, "Domain", REQUIRED, Version::V13, None)
            .map_err(|e| entry_ctx(e, dict_name, "Domain"))?
            .unwrap_or_default();
    let range = validate_number_array_entry(x, d, dict_name, "Range", OPTIONAL, Version::V13, None)
        .map_err(|e| entry_ctx(e, dict_name, "Range"))?;
    let functions =
        validate_array_entry(x, d, dict_name, "Functions", REQUIRED, Version::V13, None)
            .map_err(|e| entry_ctx(e, dict_name, "Functions"))?
            .unwrap_or_default();
    let bounds =
        validate_number_array_entry(x, d, dict_name, "Bounds", REQUIRED, Version::V13, None)
            .map_err(|e| entry_ctx(e, dict_name, "Bounds"))?
            .unwrap_or_default();
    let encode =
        validate_number_array_entry(x, d, dict_name, "Encode", REQUIRED, Version::V13, None)
            .map_err(|e| entry_ctx(e, dict_name, "Encode"))?
            .unwrap_or_default();
    Ok(StitchingArrays {
        domain,
        range,
        functions,
        bounds,
        encode,
    })
}

/// `validateStitchingFunctionCardinality`. Relaxed validation accepts an empty `Range` that is
/// no dimension, and one bound too few (older FOP versions omitted the final bound of a repeated
/// terminal gradient stop).
fn stitching_cardinality(a: &StitchingArrays, dict_name: &str) -> Result<(), Error> {
    function_array_length(&a.domain, dict_name, "Domain", 2, "one input dimension")?;

    if let Some(range) = &a.range
        && let Err(e) = function_dimension(range, dict_name, "Range")
        && !range.is_empty()
    {
        return Err(e);
    }

    let k = a.functions.len();
    if k == 0 {
        return Err(array_cardinality_error(
            dict_name,
            "Functions",
            0,
            "one or more subfunctions",
        ));
    }

    let bound_count = a.bounds.len();
    let bound_count_expected = k - 1;
    if bound_count != bound_count_expected && bound_count + 1 != bound_count_expected {
        return Err(length_error(
            bound_count,
            dict_name,
            "Bounds",
            bound_count_expected,
            "one fewer than Functions",
        ));
    }

    function_array_length(
        &a.encode,
        dict_name,
        "Encode",
        k.saturating_mul(2),
        "twice the Functions count",
    )
}

/// `validateStitchingFunctionDict`.
fn validate_stitching_function_dict(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    d: &Dict,
    depth: u64,
) -> Result<(), Error> {
    let dict_name = "stitchingFunctionDict";
    v.x.validate_version(dict_name, Version::V13)
        .map_err(|e| e.context(dict_name))?;
    let a = stitching_arrays(v.x, d, dict_name)?;
    stitching_cardinality(&a, dict_name)?;
    for o in &a.functions {
        validate_function_traversal(v, t, o, depth.saturating_add(1))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Type 0: sampled

/// `sampledFunctionArrays`.
struct SampledArrays {
    domain: Array,
    range: Array,
    size: Array,
    encode: Option<Array>,
    decode: Option<Array>,
}

fn sampled_arrays(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    version: Version,
) -> Result<SampledArrays, Error> {
    let domain = validate_number_array_entry(x, d, dict_name, "Domain", REQUIRED, version, None)
        .map_err(|e| entry_ctx(e, dict_name, "Domain"))?
        .unwrap_or_default();
    let range = validate_number_array_entry(x, d, dict_name, "Range", REQUIRED, version, None)
        .map_err(|e| entry_ctx(e, dict_name, "Range"))?
        .unwrap_or_default();
    let size = validate_integer_array_entry(x, d, dict_name, "Size", REQUIRED, version, None)
        .map_err(|e| entry_ctx(e, dict_name, "Size"))?
        .unwrap_or_default();
    let encode = validate_number_array_entry(x, d, dict_name, "Encode", OPTIONAL, version, None)
        .map_err(|e| entry_ctx(e, dict_name, "Encode"))?;
    let decode = validate_number_array_entry(x, d, dict_name, "Decode", OPTIONAL, version, None)
        .map_err(|e| entry_ctx(e, dict_name, "Decode"))?;
    Ok(SampledArrays {
        domain,
        range,
        size,
        encode,
        decode,
    })
}

/// `validateSampledFunctionCardinality`.
fn sampled_cardinality(a: &SampledArrays, dict_name: &str) -> Result<(), Error> {
    let m = function_dimension(&a.domain, dict_name, "Domain")?;
    let n = function_dimension(&a.range, dict_name, "Range")?;
    function_array_length(
        &a.size,
        dict_name,
        "Size",
        m,
        "matching Domain input dimension",
    )?;
    if let Some(encode) = &a.encode {
        function_array_length(
            encode,
            dict_name,
            "Encode",
            m.saturating_mul(2),
            "twice the input dimension",
        )?;
    }
    let Some(decode) = &a.decode else {
        return Ok(());
    };
    function_array_length(
        decode,
        dict_name,
        "Decode",
        n.saturating_mul(2),
        "twice the output dimension",
    )
}

/// `validateSampledFunctionStreamDict`: relaxed validation requires version 1.1.
fn validate_sampled_function_stream_dict(x: &mut XRefTable, sd: &StreamDict) -> Result<(), Error> {
    let version = Version::V11;
    let dict_name = "sampledFunctionStreamDict";
    x.validate_version(dict_name, version)
        .map_err(|e| e.context(dict_name))?;
    let a = sampled_arrays(x, &sd.dict, dict_name, version)?;
    sampled_cardinality(&a, dict_name)?;

    validate_integer_entry(
        x,
        &sd.dict,
        dict_name,
        "BitsPerSample",
        REQUIRED,
        version,
        Some(&|i: i64| matches!(i, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32)),
    )
    .map_err(|e| entry_ctx(e, dict_name, "BitsPerSample"))?;

    validate_integer_entry(
        x,
        &sd.dict,
        dict_name,
        "Order",
        OPTIONAL,
        version,
        Some(&|i: i64| i == 1 || i == 3),
    )
    .map_err(|e| entry_ctx(e, dict_name, "Order"))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Type 4: PostScript calculator

/// `validatePostScriptCalculatorFunctionStreamDict`: relaxed validation requires version 1.2.
fn validate_postscript_calculator_function_stream_dict(
    x: &mut XRefTable,
    sd: &StreamDict,
) -> Result<(), Error> {
    let version = Version::V12;
    let dict_name = "postScriptCalculatorFunctionStreamDict";
    x.validate_version(dict_name, version)
        .map_err(|e| e.context(dict_name))?;
    let domain =
        validate_number_array_entry(x, &sd.dict, dict_name, "Domain", REQUIRED, version, None)
            .map_err(|e| entry_ctx(e, dict_name, "Domain"))?
            .unwrap_or_default();
    let range =
        validate_number_array_entry(x, &sd.dict, dict_name, "Range", REQUIRED, version, None)
            .map_err(|e| entry_ctx(e, dict_name, "Range"))?
            .unwrap_or_default();
    function_dimension(&domain, dict_name, "Domain")?;
    function_dimension(&range, dict_name, "Range")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The traversal

/// `functionTraversal`: the functions on the path being validated, and the depth at which each
/// function was validated. extgstate.rs uses it for the halftone graph as well (Go's
/// `halftoneTraversal` is the same record with another error).
#[derive(Debug, Default)]
pub struct FunctionTraversal {
    ancestors: BTreeSet<i64>,
    validated: BTreeMap<i64, u64>,
}

impl FunctionTraversal {
    /// `enter`: an object number of 0 is a direct object and is not tracked. `cycle` is the
    /// error of an object that is on the path already.
    pub(super) fn enter(&mut self, obj_nr: i64, cycle: Sentinel) -> Result<(), Error> {
        if obj_nr <= 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(cycle).context(format!("obj#{obj_nr}")));
        }
        Ok(())
    }

    pub(super) fn leave(&mut self, obj_nr: i64) {
        if obj_nr > 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    pub(super) fn already_validated(&self, obj_nr: i64, depth: u64) -> bool {
        obj_nr > 0 && self.validated.get(&obj_nr).is_some_and(|d| depth <= *d)
    }

    pub(super) fn mark_validated(&mut self, obj_nr: i64, depth: u64) {
        if obj_nr <= 0 {
            return;
        }
        let e = self.validated.entry(obj_nr).or_insert(depth);
        if depth > *e {
            *e = depth;
        }
    }
}

/// `functionObjectIdentity`: the object number of a reference, 0 for a direct object.
pub fn function_object_identity(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

/// `processFunctionDict`: the functions of types 2 and 3.
fn process_function_dict(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    d: &Dict,
    depth: u64,
) -> Result<(), Error> {
    let Some(function_type) = validate_integer_entry(
        v.x,
        d,
        "functionDict",
        "FunctionType",
        REQUIRED,
        Version::V10,
        Some(&|i: i64| i == 2 || i == 3),
    )
    .map_err(|e| e.context("function dictionary: FunctionType"))?
    else {
        return Ok(());
    };
    match function_type {
        2 => validate_exponential_interpolation_function_dict(v.x, d)
            .map_err(|e| e.context("exponential interpolation function")),
        3 => validate_stitching_function_dict(v, t, d, depth)
            .map_err(|e| e.context("stitching function")),
        _ => Ok(()),
    }
}

/// `processFunctionStreamDict`: the functions of types 0 and 4.
fn process_function_stream_dict(x: &mut XRefTable, sd: &StreamDict) -> Result<(), Error> {
    let Some(function_type) = validate_integer_entry(
        x,
        &sd.dict,
        "functionDict",
        "FunctionType",
        REQUIRED,
        Version::V10,
        Some(&|i: i64| i == 0 || i == 4),
    )
    .map_err(|e| e.context("function stream dictionary: FunctionType"))?
    else {
        return Ok(());
    };
    match function_type {
        0 => {
            validate_sampled_function_stream_dict(x, sd).map_err(|e| e.context("sampled function"))
        }
        4 => validate_postscript_calculator_function_stream_dict(x, sd)
            .map_err(|e| e.context("PostScript calculator function")),
        _ => Ok(()),
    }
}

/// `processFunction`: a function is a dictionary (types 2 and 3) or a stream (types 0 and 4).
fn process_function(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    o: &Object,
    depth: u64,
) -> Result<(), Error> {
    match o {
        Object::Dict(d) => process_function_dict(v, t, d, depth),
        Object::Stream(sd) => process_function_stream_dict(v.x, sd),
        other => Err(Error::Validation(format!(
            "function object: expected dict or stream dict, got {}",
            type_name(other)
        ))),
    }
}

/// `functionTraversal.validateFunction`: validates the function `o` is or refers to, `depth`
/// levels below the outermost one.
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::MaxRecursionDepth`], [`Sentinel::FunctionCycle`],
/// [`Error::Validation`] for an entry that is not valid, and the errors of the dereference.
pub fn validate_function_traversal(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    o: &Object,
    depth: u64,
) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth("function graph", depth)?;
    let function_obj_nr = function_object_identity(o);
    t.enter(function_obj_nr, Sentinel::FunctionCycle)?;
    let r = validate_entered(v, t, o, function_obj_nr, depth);
    t.leave(function_obj_nr);
    r
}

fn validate_entered(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    o: &Object,
    function_obj_nr: i64,
    depth: u64,
) -> Result<(), Error> {
    if t.already_validated(function_obj_nr, depth) {
        return Ok(());
    }
    let resolved =
        v.x.dereference(o)
            .map_err(|e| e.context("function: dereference"))?
            .clone();
    if resolved.is_null() {
        return Err(Error::Validation("function: missing object".to_string()));
    }
    process_function(v, t, &resolved, depth)?;
    t.mark_validated(function_obj_nr, depth);
    Ok(())
}

/// `validateFunction`: validates the function `o` is or refers to, with a traversal of its own.
///
/// # Errors
/// As [`validate_function_traversal`].
pub fn validate_function(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    validate_function_traversal(v, &mut FunctionTraversal::default(), o, 0)
}

/// `validateFunctionObjects`: one function, or every function of an array (null elements are
/// skipped). `raw` is the entry as stored and `resolved` what it refers to; a function that is a
/// direct object is validated as `resolved`.
///
/// # Errors
/// As [`validate_function_traversal`].
pub fn validate_function_objects(
    v: &mut Validator<'_>,
    t: &mut FunctionTraversal,
    raw: &Object,
    resolved: &Object,
) -> Result<(), Error> {
    let Object::Array(a) = resolved else {
        let function = if function_object_identity(raw) == 0 {
            resolved
        } else {
            raw
        };
        return validate_function_traversal(v, t, function, 0);
    };
    for o in a {
        v.check()?;
        if o.is_null() {
            continue;
        }
        validate_function_traversal(v, t, o, 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
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

    fn numbers(n: &[i64]) -> Object {
        Object::Array(n.iter().map(|i| Object::Integer(*i)).collect())
    }

    fn exponential(domain: &[i64], n: i64) -> Object {
        dict(&[
            ("FunctionType", Object::Integer(2)),
            ("Domain", numbers(domain)),
            ("N", Object::Integer(n)),
        ])
    }

    #[test]
    fn an_exponential_function_is_valid() {
        let mut x = table(vec![(1, exponential(&[0, 1], 1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_function(&mut v, &r(1)).is_ok());
    }

    #[test]
    fn an_exponential_function_needs_one_input_dimension() {
        let mut x = table(vec![(1, exponential(&[0, 1, 0, 1], 1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_function(&mut v, &r(1)).unwrap_err();
        assert!(matches!(err.root(), Error::Validation(_)), "{err:?}");
    }

    #[test]
    fn a_bad_exponential_domain_is_accepted_in_relaxed_mode() {
        let mut x = table(vec![(1, exponential(&[1, 0], 1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_function(&mut v, &r(1)).is_ok());
    }

    #[test]
    fn a_stitching_function_checks_its_subfunctions_and_bounds() {
        let stitching = |bounds: &[i64]| {
            dict(&[
                ("FunctionType", Object::Integer(3)),
                ("Domain", numbers(&[0, 1])),
                ("Functions", Object::Array(vec![r(2), r(3)])),
                ("Bounds", numbers(bounds)),
                ("Encode", numbers(&[0, 1, 0, 1])),
            ])
        };
        let mut x = table(vec![
            (1, stitching(&[1])),
            (2, exponential(&[0, 1], 1)),
            (3, exponential(&[0, 1], 1)),
            (4, stitching(&[1, 1, 1])),
        ]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_function(&mut v, &r(1)).is_ok());
        assert!(validate_function(&mut v, &r(4)).is_err());
    }

    #[test]
    fn a_stitching_function_may_miss_the_last_bound() {
        let f = dict(&[
            ("FunctionType", Object::Integer(3)),
            ("Domain", numbers(&[0, 1])),
            ("Functions", Object::Array(vec![r(2), r(2)])),
            ("Bounds", numbers(&[])),
            ("Encode", numbers(&[0, 1, 0, 1])),
        ]);
        let mut x = table(vec![(1, f), (2, exponential(&[0, 1], 1))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_function(&mut v, &r(1)).is_ok());
    }

    #[test]
    fn a_function_that_contains_itself_is_a_cycle() {
        let f = dict(&[
            ("FunctionType", Object::Integer(3)),
            ("Domain", numbers(&[0, 1])),
            ("Functions", Object::Array(vec![r(1)])),
            ("Bounds", numbers(&[])),
            ("Encode", numbers(&[0, 1])),
        ]);
        let mut x = table(vec![(1, f)]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_function(&mut v, &r(1)).unwrap_err();
        assert!(err.is(Sentinel::FunctionCycle), "{err}");
    }

    #[test]
    fn a_missing_function_and_a_wrong_type_are_errors() {
        let mut x = table(vec![(1, Object::Integer(3))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_function(&mut v, &r(9)).is_err());
        assert!(validate_function(&mut v, &r(1)).is_err());
    }
}
