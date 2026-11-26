//! Extended graphics states (Go: validate/extGState.go), see 8.4.5.
//!
//! Relaxed validation (design section 6): a graphics value that is out of range is accepted (Go's
//! `validGraphicsValue` returns true and records a notice), so `LW`, `D`, `PaintType` and the like
//! are checked for their type only, and the entries that were introduced late are accepted from
//! the earlier versions Go lists. `showDigestedVersionViolation` is not ported.
//!
//! The graphs of functions and halftones are walked with a [`FunctionTraversal`] each; a
//! halftone dictionary can contain other halftones (type 5), and the functions of a halftone
//! are validated by function.rs.

use super::Validator;
use super::function::{
    FunctionTraversal, function_object_identity, validate_function, validate_function_traversal,
};
use super::object::{
    OPTIONAL, REQUIRED, object_context, validate_array_entry, validate_array_exact_length,
    validate_boolean_entry, validate_entry, validate_integer_entry, validate_name_entry,
    validate_number_array, validate_number_entry, validate_number_for_object,
    validate_stream_dict_entry, validate_string_entry,
};
use super::shading::validate_soft_mask_backdrop;
use super::xobject::validate_xobject_stream_dict_contents;
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, Ref, StreamDict};

fn entry_ctx(e: Error, dict_name: &str, entry_name: &str) -> Error {
    e.context(format!("{dict_name}.{entry_name}"))
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `validateBlendMode` (11.3.5, table 136).
fn valid_blend_mode(s: &[u8]) -> bool {
    matches!(
        s,
        b"None"
            | b"Normal"
            | b"Compatible"
            | b"Multiply"
            | b"Mult"
            | b"Screen"
            | b"Overlay"
            | b"Darken"
            | b"Lighten"
            | b"ColorDodge"
            | b"ColorBurn"
            | b"HardLight"
            | b"SoftLight"
            | b"Difference"
            | b"Exclusion"
            | b"Hue"
            | b"Saturation"
            | b"Color"
            | b"Luminosity"
    )
}

/// `validateLineDashPatternEntry` (8.4.3.6): `[dashArray dashPhase]`. Relaxed validation does not
/// check the lengths of the dash array.
fn validate_line_dash_pattern_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(
        x,
        d,
        dict_name,
        entry_name,
        required,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )
    .map_err(|e| entry_ctx(e, dict_name, entry_name))?
    else {
        return Ok(());
    };
    let [dash, phase] = a.as_slice() else {
        return Ok(());
    };
    validate_number_array(x, dash)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}[0]")))?;
    validate_number_for_object(x, phase)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}[1]")))?;
    Ok(())
}

/// `functionEntryObject`: a function that is stored as a reference is validated as that
/// reference (its object number identifies it in the graph), a direct one as its value.
fn function_entry_object(d: &Dict, entry_name: &str, resolved: &Object) -> Object {
    match d.find(entry_name.as_bytes()) {
        Some(raw) if function_object_identity(raw) > 0 => raw.clone(),
        _ => resolved.clone(),
    }
}

/// `validateFunctionOrNameEntry`: a function, or a name `valid_name` accepts.
fn validate_function_or_name_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    valid_name: &dyn Fn(&[u8]) -> bool,
) -> Result<(), Error> {
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, since)
        .map_err(|e| entry_ctx(e, dict_name, entry_name))?
    else {
        return Ok(());
    };
    match &o {
        Object::Name(n) => {
            if !valid_name(n.as_slice()) {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Dict(_) | Object::Stream(_) => {
            validate_function(v, &function_entry_object(d, entry_name, &o))
                .map_err(|e| entry_ctx(e, dict_name, entry_name))
        }
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected function or name, got {}",
            type_name(other)
        ))),
    }
}

/// The three entries that hold a function, a name or four functions.
#[derive(Clone, Copy)]
enum TransferKind {
    /// `validateTransferFunction`: `Identity`, or a function.
    TransferFunction,
    /// `validateTR`: `Identity`, or a function, or four functions (an element can be `Identity`).
    Tr,
    /// `validateTR2`: as `TR`, and `Default`.
    Tr2,
}

impl TransferKind {
    fn label(self) -> &'static str {
        match self {
            TransferKind::TransferFunction => "transfer function",
            TransferKind::Tr => "TR",
            TransferKind::Tr2 => "TR2",
        }
    }

    fn valid_name(self, n: &[u8]) -> bool {
        n == b"Identity" || (matches!(self, TransferKind::Tr2) && n == b"Default")
    }

    fn names_in_array(self) -> bool {
        !matches!(self, TransferKind::TransferFunction)
    }
}

/// `validateTransferFunction`, `validateTR` and `validateTR2`: `o` is the entry as stored.
fn validate_transfer_object(
    v: &mut Validator<'_>,
    o: &Object,
    kind: TransferKind,
) -> Result<(), Error> {
    let label = kind.label();
    v.check()?;
    let resolved =
        v.x.dereference(o)
            .map_err(|e| e.context(format!("{label}: dereference")))?
            .clone();
    let mut t = FunctionTraversal::default();
    match &resolved {
        Object::Name(n) => {
            if !kind.valid_name(n) {
                return Err(Error::Validation(format!(
                    "{label}: invalid name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Array(a) => {
            if a.len() != 4 {
                return Err(Error::Validation(format!(
                    "{label} array: invalid length {}, expected 4",
                    a.len()
                )));
            }
            for (i, e) in a.iter().enumerate() {
                let element =
                    v.x.dereference(e)
                        .map_err(|err| err.context(format!("{label} array[{i}]: dereference")))?
                        .clone();
                if element.is_null() {
                    continue;
                }
                if kind.names_in_array()
                    && let Object::Name(n) = &element
                {
                    if !kind.valid_name(n) {
                        return Err(Error::Validation(format!(
                            "{label} array[{i}]: invalid name {}",
                            lossy(n)
                        )));
                    }
                    continue;
                }
                validate_function_traversal(v, &mut t, e, 0)
                    .map_err(|err| err.context(format!("{label} array[{i}]")))?;
            }
            Ok(())
        }
        Object::Dict(_) | Object::Stream(_) => validate_function_traversal(v, &mut t, o, 0),
        other => Err(Error::Validation(format!(
            "{label}: expected function, name or function array, got {}",
            type_name(other)
        ))),
    }
}

/// `validateTransferFunctionEntry`, `validateTREntry` and `validateTR2Entry`.
fn validate_transfer_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    kind: TransferKind,
) -> Result<(), Error> {
    let present = validate_entry(v.x, d, dict_name, entry_name, required, since)
        .map_err(|e| entry_ctx(e, dict_name, entry_name))?
        .is_some();
    if !present {
        return Ok(());
    }
    let raw = d
        .find(entry_name.as_bytes())
        .cloned()
        .unwrap_or(Object::Null);
    validate_transfer_object(v, &raw, kind).map_err(|e| entry_ctx(e, dict_name, entry_name))
}

/// `validateSpotFunctionEntry`: a name of the predefined spot functions, or a function.
fn validate_spot_function_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match &o {
        Object::Name(n) => {
            let valid = matches!(
                n.as_slice(),
                b"SimpleDot"
                    | b"InvertedSimpleDot"
                    | b"DoubleDot"
                    | b"InvertedDoubleDot"
                    | b"CosineDot"
                    | b"Double"
                    | b"InvertedDouble"
                    | b"Line"
                    | b"LineX"
                    | b"LineY"
                    | b"Round"
                    | b"Ellipse"
                    | b"EllipseA"
                    | b"InvertedEllipseA"
                    | b"EllipseB"
                    | b"EllipseC"
                    | b"InvertedEllipseC"
                    | b"Square"
                    | b"Cross"
                    | b"Rhomboid"
            );
            if !valid {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid spot function name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Dict(_) | Object::Stream(_) => {
            validate_function(v, &function_entry_object(d, entry_name, &o))
                .map_err(|e| entry_ctx(e, dict_name, entry_name))
        }
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected spot function name or function, got {}",
            type_name(other)
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// Halftones (10.5)

/// `halftoneTraversal`: the halftones on the path (a cycle is an error) and the depth at which
/// each was validated, and the version the entries are validated against.
struct HalftoneTraversal {
    graph: FunctionTraversal,
    since: Version,
}

/// `validateType1HalftoneDict`.
fn validate_type1_halftone_dict(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "type1HalftoneDict";
    validate_string_entry(v.x, d, dict_name, "HalftoneName", OPTIONAL, since, None)?;
    validate_number_entry(v.x, d, dict_name, "Frequency", REQUIRED, since, None)?;
    validate_number_entry(v.x, d, dict_name, "Angle", REQUIRED, since, None)?;
    validate_spot_function_entry(v, d, dict_name, "SpotFunction", REQUIRED, since)?;
    validate_transfer_entry(
        v,
        d,
        dict_name,
        "TransferFunction",
        OPTIONAL,
        since,
        TransferKind::TransferFunction,
    )?;
    validate_boolean_entry(v.x, d, dict_name, "AccurateScreens", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateType5HalftoneDict`: the components are halftones, `Default` is required.
fn validate_type5_halftone_dict(
    v: &mut Validator<'_>,
    t: &mut HalftoneTraversal,
    d: &Dict,
    depth: u64,
) -> Result<(), Error> {
    let dict_name = "type5HalftoneDict";
    let since = t.since;
    validate_string_entry(v.x, d, dict_name, "HalftoneName", OPTIONAL, since, None)?;
    for component in [
        "Gray", "Red", "Green", "Blue", "Cyan", "Magenta", "Yellow", "Black",
    ] {
        validate_halftone_entry(
            v,
            t,
            d,
            dict_name,
            component,
            OPTIONAL,
            depth.saturating_add(1),
        )?;
    }
    validate_halftone_entry(
        v,
        t,
        d,
        dict_name,
        "Default",
        REQUIRED,
        depth.saturating_add(1),
    )
}

/// `validateType6HalftoneStreamDict`, `validateType10HalftoneStreamDict` and
/// `validateType16HalftoneStreamDict`: the integer entries (with their `required`) and the
/// transfer function.
fn validate_halftone_stream_fields(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    dict_name: &str,
    since: Version,
    integers: &[(&str, bool)],
) -> Result<(), Error> {
    validate_string_entry(
        v.x,
        &sd.dict,
        dict_name,
        "HalftoneName",
        OPTIONAL,
        since,
        None,
    )?;
    for (name, required) in integers {
        validate_integer_entry(v.x, &sd.dict, dict_name, name, *required, since, None)?;
    }
    validate_transfer_entry(
        v,
        &sd.dict,
        dict_name,
        "TransferFunction",
        OPTIONAL,
        since,
        TransferKind::TransferFunction,
    )
}

/// `halftoneTraversal.validateDict`: type 1 and type 5 halftone dictionaries.
fn validate_halftone_dict(
    v: &mut Validator<'_>,
    t: &mut HalftoneTraversal,
    d: &Dict,
    depth: u64,
) -> Result<(), Error> {
    let dict_name = "halfToneDict";
    let since = t.since;
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Halftone"),
    )?;
    let halftone_type =
        validate_integer_entry(v.x, d, dict_name, "HalftoneType", REQUIRED, since, None)?
            .unwrap_or_default();
    match halftone_type {
        1 => validate_type1_halftone_dict(v, d, since),
        5 => validate_type5_halftone_dict(v, t, d, depth),
        n => Err(Error::Validation(format!("unknown halftoneTyp: {n}"))),
    }
}

/// `validateHalfToneStreamDict`: type 6, 10 and 16 halftone streams.
fn validate_halftone_stream_dict(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    since: Version,
) -> Result<(), Error> {
    let dict_name = "writeHalfToneStreamDict";
    validate_name_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Halftone"),
    )?;
    let Some(halftone_type) = validate_integer_entry(
        v.x,
        &sd.dict,
        dict_name,
        "HalftoneType",
        REQUIRED,
        since,
        None,
    )?
    else {
        return Ok(());
    };
    match halftone_type {
        6 => validate_halftone_stream_fields(
            v,
            sd,
            "type6HalftoneDict",
            since,
            &[("Width", REQUIRED), ("Height", REQUIRED)],
        ),
        10 => validate_halftone_stream_fields(
            v,
            sd,
            "type10HalftoneDict",
            since,
            &[("Xsquare", REQUIRED), ("Ysquare", REQUIRED)],
        ),
        16 => validate_halftone_stream_fields(
            v,
            sd,
            "type16HalftoneDict",
            since,
            &[
                ("Width", REQUIRED),
                ("Height", REQUIRED),
                ("Width2", OPTIONAL),
                ("Height2", OPTIONAL),
            ],
        ),
        n => Err(Error::Validation(format!("unknown halftoneTyp: {n}"))),
    }
}

/// `halftoneTraversal.validateObject`: the halftone an entry refers to, resolved.
fn validate_halftone_object(
    v: &mut Validator<'_>,
    t: &mut HalftoneTraversal,
    o: &Object,
    dict_name: &str,
    entry_name: &str,
    depth: u64,
) -> Result<(), Error> {
    match o {
        Object::Name(n) => {
            if n != b"Default" {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid halftone name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Dict(d) => {
            validate_halftone_dict(v, t, d, depth).map_err(|e| entry_ctx(e, dict_name, entry_name))
        }
        Object::Stream(sd) => validate_halftone_stream_dict(v, sd, t.since)
            .map_err(|e| entry_ctx(e, dict_name, entry_name)),
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected halftone dict, stream dict or Default name, got {}",
            type_name(other)
        ))),
    }
}

/// `halftoneTraversal.validateEntry` (10.5): an entry that holds a halftone.
fn validate_halftone_entry(
    v: &mut Validator<'_>,
    t: &mut HalftoneTraversal,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    depth: u64,
) -> Result<(), Error> {
    v.check()?;
    let raw = match d.find(entry_name.as_bytes()) {
        None | Some(Object::Null) => {
            validate_entry(v.x, d, dict_name, entry_name, required, t.since)?;
            return Ok(());
        }
        Some(raw) => raw,
    };
    v.x.check_recursion_depth("halftone graph", depth)?;
    let halftone_obj_nr = function_object_identity(raw);
    t.graph.enter(halftone_obj_nr, Sentinel::HalftoneCycle)?;
    let r = validate_halftone_entered(v, t, d, dict_name, entry_name, required, depth);
    t.graph.leave(halftone_obj_nr);
    r
}

/// The part of `validateEntry` after the halftone is on the path.
fn validate_halftone_entered(
    v: &mut Validator<'_>,
    t: &mut HalftoneTraversal,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    depth: u64,
) -> Result<(), Error> {
    let halftone_obj_nr = d
        .find(entry_name.as_bytes())
        .map_or(0, function_object_identity);
    if t.graph.already_validated(halftone_obj_nr, depth) {
        return Ok(());
    }
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, t.since)? else {
        return Ok(());
    };
    validate_halftone_object(v, t, &o, dict_name, entry_name, depth)?;
    t.graph.mark_validated(halftone_obj_nr, depth);
    Ok(())
}

/// `validateHalfToneEntry` (`HT`): a halftone dictionary or stream, or the name `Default`.
fn validate_half_tone_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let mut t = HalftoneTraversal {
        graph: FunctionTraversal::default(),
        since,
    };
    validate_halftone_entry(v, &mut t, d, dict_name, entry_name, required, 0)
}

// ---------------------------------------------------------------------------------------------
// Transparency (11.3.5, 11.6)

/// `validateBlendModeEntry`: a blend mode name, or an array of them.
fn validate_blend_mode_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match &o {
        Object::Name(_) => {
            x.dereference_name(&o, since, Some(&valid_blend_mode))?;
            Ok(())
        }
        Object::Array(a) => {
            for (i, e) in a.iter().enumerate() {
                x.dereference_name(e, since, Some(&valid_blend_mode))
                    .map_err(|err| err.context(format!("{dict_name}.{entry_name}[{i}]")))?;
            }
            Ok(())
        }
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected name or array, got {}",
            type_name(other)
        ))),
    }
}

/// `validateSoftMaskTransferFunctionEntry`: the `Identity` name, or a function.
fn validate_soft_mask_transfer_function_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match &o {
        Object::Name(n) => {
            if n != b"Identity" {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid transfer function name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Dict(_) | Object::Stream(_) => {
            validate_function(v, &function_entry_object(d, entry_name, &o))
                .map_err(|e| entry_ctx(e, dict_name, entry_name))
        }
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected function or Identity name, got {}",
            type_name(other)
        ))),
    }
}

/// `validateSoftMaskDict` (11.6.5.2).
fn validate_soft_mask_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "softMaskDict";

    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Mask"),
    )?;

    let subtype = validate_name_entry(
        v.x,
        d,
        dict_name,
        "S",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Alpha" || s == b"Luminosity"),
    )?;

    // G: a transparency group XObject, the source of the alpha or colour values of the mask.
    let group = validate_stream_dict_entry(v.x, d, dict_name, "G", REQUIRED, Version::V10, None)?;
    if let Some(sd) = &group {
        validate_xobject_stream_dict_contents(v, sd, d.indirect_ref_entry(b"G"))?;
    }

    // TR: the function that derives the mask values.
    validate_soft_mask_transfer_function_entry(v, d, dict_name, "TR", OPTIONAL, Version::V10)?;

    // BC: the backdrop colour the group is composited against.
    let luminosity = subtype.as_deref().is_some_and(|s| s == b"Luminosity");
    validate_soft_mask_backdrop(v, d, group.as_ref(), luminosity)
}

/// `validateSoftMaskEntry` (11.3.7.2, 11.6.4.3).
fn validate_soft_mask_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };
    match &o {
        Object::Name(n) => {
            if !valid_blend_mode(n) {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid soft mask name {}",
                    lossy(n)
                )));
            }
            Ok(())
        }
        Object::Dict(m) => {
            validate_soft_mask_dict(v, m).map_err(|e| entry_ctx(e, dict_name, entry_name))
        }
        other => Err(Error::Validation(format!(
            "{dict_name}.{entry_name}: expected name or soft mask dict, got {}",
            type_name(other)
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// The graphics state parameter dictionary (8.4.5)

/// `validateExtGStateFontReference`: an indirect reference to a font dictionary.
fn validate_extgstate_font_reference(
    x: &mut XRefTable,
    o: &Object,
    dict_name: &str,
) -> Result<(), Error> {
    let Object::Ref(r) = o else {
        return Err(Error::Validation(format!(
            "{dict_name}.Font[0]: expected indirect font reference"
        )));
    };
    check_font_dictionary(x, *r).map_err(|e| e.context(format!("{dict_name}.Font[0]")))
}

fn check_font_dictionary(x: &mut XRefTable, r: Ref) -> Result<(), Error> {
    let font = x
        .dereference_dict_ref(r)?
        .cloned()
        .ok_or_else(|| Error::Validation("missing font dictionary".to_string()))?;
    validate_name_entry(
        x,
        &font,
        "fontDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Font"),
    )?;
    Ok(())
}

/// `validateExtGStateFont`: `[font size]`.
fn validate_extgstate_font(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, "Font", OPTIONAL, Version::V13, None)?
    else {
        return Ok(());
    };
    validate_array_exact_length(&a, dict_name, "Font", 2)?;
    let [font, size] = a.as_slice() else {
        return Ok(());
    };
    validate_extgstate_font_reference(x, font, dict_name)?;
    x.dereference_number(size)
        .map_err(|e| e.context(format!("{dict_name}.Font[1]: expected font size number")))?;
    Ok(())
}

/// `validateExtGStateDictPart1`: the line and overprint parameters.
fn validate_extgstate_dict_part1(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // LW (a negative width is accepted), LC, LJ, ML: since 1.3.
    validate_number_entry(x, d, dict_name, "LW", OPTIONAL, Version::V13, None)?;
    validate_integer_entry(x, d, dict_name, "LC", OPTIONAL, Version::V13, None)?;
    validate_integer_entry(x, d, dict_name, "LJ", OPTIONAL, Version::V13, None)?;
    validate_number_entry(x, d, dict_name, "ML", OPTIONAL, Version::V13, None)?;

    // D: [dashArray dashPhase], since 1.3.
    validate_line_dash_pattern_entry(x, d, dict_name, "D", OPTIONAL, Version::V13)?;

    // RI: since 1.3.
    validate_name_entry(x, d, dict_name, "RI", OPTIONAL, Version::V13, None)?;

    // OP, op (since 1.3, accepted from 1.2) and OPM (the same).
    validate_boolean_entry(x, d, dict_name, "OP", OPTIONAL, Version::V10, None)?;
    validate_boolean_entry(x, d, dict_name, "op", OPTIONAL, Version::V12, None)?;
    validate_integer_entry(x, d, dict_name, "OPM", OPTIONAL, Version::V12, None)?;

    // Font: since 1.3.
    validate_extgstate_font(x, d, dict_name)
}

/// `validateExtGStateDictPart2`: the black generation, undercolor removal, transfer and
/// halftone parameters.
fn validate_extgstate_dict_part2(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let v10 = Version::V10;
    // BG and UCR: a function (relaxed validation accepts the name Identity too), see 10.3.4.
    validate_function_or_name_entry(v, d, dict_name, "BG", OPTIONAL, v10, &|s: &[u8]| {
        s == b"Identity"
    })?;
    // BG2 and UCR2: a function or the name Default.
    validate_function_or_name_entry(v, d, dict_name, "BG2", OPTIONAL, v10, &|s: &[u8]| {
        s == b"Default"
    })?;
    validate_function_or_name_entry(v, d, dict_name, "UCR", OPTIONAL, v10, &|s: &[u8]| {
        s == b"Identity"
    })?;
    validate_function_or_name_entry(v, d, dict_name, "UCR2", OPTIONAL, v10, &|s: &[u8]| {
        s == b"Default"
    })?;

    // TR and TR2: a function, an array of 4 functions or a name (10.4).
    validate_transfer_entry(v, d, dict_name, "TR", OPTIONAL, v10, TransferKind::Tr)?;
    validate_transfer_entry(v, d, dict_name, "TR2", OPTIONAL, v10, TransferKind::Tr2)?;

    // HT: a halftone dictionary or stream, or Default (10.5). Since 1.2, accepted from 1.1.
    validate_half_tone_entry(v, d, dict_name, "HT", OPTIONAL, Version::V11)?;

    // FL: flatness tolerance (10.6.2), SM: smoothness tolerance (since 1.3, accepted from 1.2),
    // SA: automatic stroke adjustment (10.6.5).
    validate_number_entry(v.x, d, dict_name, "FL", OPTIONAL, Version::V13, None)?;
    validate_number_entry(v.x, d, dict_name, "SM", OPTIONAL, Version::V12, None)?;
    validate_boolean_entry(v.x, d, dict_name, "SA", OPTIONAL, v10, None)?;
    Ok(())
}

/// `validateExtGStateDictPart3`: the transparency parameters, since 1.4 (accepted from the
/// versions Go lists).
fn validate_extgstate_dict_part3(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_blend_mode_entry(v.x, d, dict_name, "BM", OPTIONAL, Version::V12)?;
    validate_soft_mask_entry(v, d, dict_name, "SMask", OPTIONAL, Version::V13)?;
    // CA and ca: the stroking and nonstroking alpha constants.
    validate_number_entry(v.x, d, dict_name, "CA", OPTIONAL, Version::V12, None)?;
    validate_number_entry(v.x, d, dict_name, "ca", OPTIONAL, Version::V11, None)?;
    // AIS: alpha is shape, TK: text knockout.
    validate_boolean_entry(v.x, d, dict_name, "AIS", OPTIONAL, Version::V13, None)?;
    validate_boolean_entry(v.x, d, dict_name, "TK", OPTIONAL, Version::V13, None)?;
    Ok(())
}

/// `validateExtGStateDictPDF20` (8.4.5, table 57).
fn validate_extgstate_dict_pdf20(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if !x.pdf20()? {
        return Ok(());
    }
    let hto = validate_array_entry(
        x,
        d,
        dict_name,
        "HTO",
        OPTIONAL,
        Version::V20,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    for (i, o) in hto.iter().flatten().enumerate() {
        x.dereference_number(o)
            .map_err(|e| e.context(format!("{dict_name}.HTO[{i}]: expected number")))?;
    }
    validate_name_entry(
        x,
        d,
        dict_name,
        "UseBlackPtComp",
        OPTIONAL,
        Version::V20,
        Some(&|s: &[u8]| matches!(s, b"ON" | b"OFF" | b"Default")),
    )?;
    Ok(())
}

/// `validateExtGStateDict`: validates the graphics state parameter dictionary `o` is or refers
/// to, see 8.4.5. A missing dictionary is accepted in relaxed mode. An `AAPL:AA` entry marks the
/// table as having custom extensions.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an entry that is not valid, and the errors of
/// the lookups.
pub fn validate_extgstate_dict(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("ExtGState: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };
    let dict_name = "extGStateDict";

    validate_name_entry(
        v.x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"ExtGState"),
    )
    .map_err(|e| e.context("ExtGState.Type"))?;

    validate_extgstate_dict_part1(v.x, &d, dict_name)
        .map_err(|e| e.context("ExtGState graphics state parameters"))?;
    validate_extgstate_dict_part2(v, &d, dict_name)
        .map_err(|e| e.context("ExtGState transfer and halftone parameters"))?;
    validate_extgstate_dict_part3(v, &d, dict_name)
        .map_err(|e| e.context("ExtGState transparency parameters"))?;

    // Check for AAPL extensions.
    let (aapl, _) = d
        .entry(dict_name, b"AAPL:AA", OPTIONAL)
        .map_err(|e| e.context("ExtGState.AAPL:AA"))?;
    if aapl.is_some() {
        v.x.custom_extensions = true;
    }

    validate_extgstate_dict_pdf20(v.x, &d, dict_name)
}

/// `validateExtGStateResourceDict`: the `ExtGState` resource dictionary, a dictionary of
/// graphics state parameter dictionaries. The names are visited in order.
///
/// # Errors
/// As [`validate_extgstate_dict`], and [`Error::Validation`] if the version is lower than
/// `since`.
pub fn validate_extgstate_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("ExtGState resource dict: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };

    v.x.validate_version("ExtGStateResourceDict", since)
        .map_err(|e| e.context("ExtGState resource dict: version"))?;

    for (name, o) in d.iter() {
        v.check()?;
        validate_extgstate_dict(v, o).map_err(|e| {
            e.context(object_context(
                &format!("ExtGState resource {}", lossy(name)),
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

    fn dict(entries: &[(&str, Object)]) -> Object {
        Object::Dict(dict_of(entries))
    }

    fn check(objects: Vec<(i64, Object)>, gs: Object) -> Result<(), Error> {
        let mut x = table(objects);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_extgstate_dict(&mut v, &gs)
    }

    fn exponential() -> Object {
        dict(&[
            ("FunctionType", Object::Integer(2)),
            (
                "Domain",
                Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
            ),
            ("N", Object::Integer(1)),
        ])
    }

    #[test]
    fn a_graphics_state_with_plain_parameters_is_valid() {
        let gs = dict(&[
            ("Type", Object::name("ExtGState")),
            ("LW", Object::Float(-1.0)),
            ("LC", Object::Integer(1)),
            ("CA", Object::Float(0.5)),
            ("ca", Object::Integer(1)),
            ("BM", Object::name("Multiply")),
            ("AIS", Object::Bool(false)),
            (
                "D",
                Object::Array(vec![
                    Object::Array(vec![Object::Integer(3)]),
                    Object::Integer(0),
                ]),
            ),
        ]);
        assert!(check(vec![], gs).is_ok());
    }

    #[test]
    fn a_wrong_type_name_or_blend_mode_is_an_error() {
        assert!(check(vec![], dict(&[("Type", Object::name("Font"))])).is_err());
        assert!(check(vec![], dict(&[("CA", Object::name("x"))])).is_err());
        assert!(check(vec![], dict(&[("BM", Object::name("Blur"))])).is_err());
        let bad = Object::Array(vec![Object::name("Normal"), Object::name("Blur")]);
        assert!(check(vec![], dict(&[("BM", bad)])).is_err());
        let ok = Object::Array(vec![Object::name("Normal"), Object::name("Hue")]);
        assert!(check(vec![], dict(&[("BM", ok)])).is_ok());
        assert!(check(vec![], dict(&[("SMask", Object::name("None"))])).is_ok());
        assert!(check(vec![], dict(&[("SMask", Object::name("Blur"))])).is_err());
    }

    #[test]
    fn the_dash_pattern_is_an_array_and_a_phase() {
        let one = Object::Array(vec![Object::Array(vec![])]);
        assert!(check(vec![], dict(&[("D", one)])).is_err());
        let bad_phase = Object::Array(vec![Object::Array(vec![]), Object::name("x")]);
        assert!(check(vec![], dict(&[("D", bad_phase)])).is_err());
    }

    #[test]
    fn the_font_entry_is_a_font_reference_and_a_size() {
        let font = dict(&[("Type", Object::name("Font"))]);
        let objects = vec![(1, font), (2, dict(&[]))];
        let entry =
            |font: Object| dict(&[("Font", Object::Array(vec![font, Object::Integer(12)]))]);
        assert!(check(objects.clone(), entry(r(1))).is_ok());
        assert!(check(objects.clone(), entry(r(2))).is_err());
        assert!(check(objects.clone(), entry(r(9))).is_err());
        assert!(check(objects, entry(dict(&[]))).is_err());
    }

    #[test]
    fn bg_ucr_and_the_transfer_functions_take_names_or_functions() {
        let objects = vec![(1, exponential())];
        assert!(check(objects.clone(), dict(&[("BG", Object::name("Identity"))])).is_ok());
        assert!(check(objects.clone(), dict(&[("BG", Object::name("Default"))])).is_err());
        assert!(check(objects.clone(), dict(&[("BG2", Object::name("Default"))])).is_ok());
        assert!(check(objects.clone(), dict(&[("UCR", r(1))])).is_ok());
        assert!(check(objects.clone(), dict(&[("UCR2", Object::Integer(1))])).is_err());
        assert!(check(objects.clone(), dict(&[("TR", Object::name("Identity"))])).is_ok());
        assert!(check(objects.clone(), dict(&[("TR", Object::name("Default"))])).is_err());
        assert!(check(objects.clone(), dict(&[("TR2", Object::name("Default"))])).is_ok());

        let four = |e: Object| Object::Array(vec![e.clone(), e.clone(), e.clone(), e]);
        assert!(check(objects.clone(), dict(&[("TR", four(r(1)))])).is_ok());
        assert!(
            check(
                objects.clone(),
                dict(&[("TR", four(Object::name("Identity")))])
            )
            .is_ok()
        );
        assert!(
            check(
                objects.clone(),
                dict(&[("TR2", four(Object::name("Default")))])
            )
            .is_ok()
        );
        assert!(
            check(
                objects.clone(),
                dict(&[("TR", four(Object::name("Default")))])
            )
            .is_err()
        );
        let three = Object::Array(vec![r(1), r(1), r(1)]);
        assert!(check(objects, dict(&[("TR", three)])).is_err());
    }

    #[test]
    fn halftones_are_checked_by_type_and_a_cycle_is_an_error() {
        let type1 = dict(&[
            ("HalftoneType", Object::Integer(1)),
            ("Frequency", Object::Integer(60)),
            ("Angle", Object::Integer(45)),
            ("SpotFunction", Object::name("Round")),
        ]);
        assert!(check(vec![], dict(&[("HT", type1.clone())])).is_ok());
        assert!(check(vec![], dict(&[("HT", Object::name("Default"))])).is_ok());
        assert!(check(vec![], dict(&[("HT", Object::name("Other"))])).is_err());
        let unknown = dict(&[("HalftoneType", Object::Integer(2))]);
        assert!(check(vec![], dict(&[("HT", unknown)])).is_err());

        // A type 5 halftone needs Default, and may nest others.
        let type5 = dict(&[
            ("HalftoneType", Object::Integer(5)),
            ("Default", r(1)),
            ("Gray", Object::name("Default")),
        ]);
        assert!(check(vec![(1, type1.clone())], dict(&[("HT", type5)])).is_ok());
        let no_default = dict(&[("HalftoneType", Object::Integer(5))]);
        assert!(check(vec![], dict(&[("HT", no_default)])).is_err());

        let cyclic = dict(&[("HalftoneType", Object::Integer(5)), ("Default", r(2))]);
        let err = check(vec![(2, cyclic)], dict(&[("HT", r(2))])).unwrap_err();
        assert!(err.is(Sentinel::HalftoneCycle), "{err}");
    }

    #[test]
    fn a_resource_dict_validates_every_graphics_state() {
        let mut x = table(vec![(1, dict(&[("CA", Object::name("x"))]))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let good = dict(&[("GS0", dict(&[("CA", Object::Float(1.0))]))]);
        assert!(validate_extgstate_resource_dict(&mut v, &good, Version::V10).is_ok());
        assert!(validate_extgstate_resource_dict(&mut v, &good, Version::V20).is_err());
        let bad = dict(&[("GS0", r(1))]);
        assert!(validate_extgstate_resource_dict(&mut v, &bad, Version::V10).is_err());
        // A missing dictionary is accepted.
        assert!(validate_extgstate_resource_dict(&mut v, &Object::Null, Version::V10).is_ok());
    }

    #[test]
    fn aapl_aa_marks_custom_extensions_and_pdf20_entries_are_checked() {
        let mut x = table(vec![]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let gs = dict(&[("AAPL:AA", Object::Bool(true))]);
        assert!(validate_extgstate_dict(&mut v, &gs).is_ok());
        assert!(v.x.custom_extensions);

        // HTO and UseBlackPtComp are only read for PDF 2.0.
        let hto = dict(&[("HTO", Object::Array(vec![Object::Integer(1)]))]);
        assert!(validate_extgstate_dict(&mut v, &hto).is_ok());
        v.x.header_version = Some(Version::V20);
        assert!(validate_extgstate_dict(&mut v, &hto).is_err());
        let ok = dict(&[
            (
                "HTO",
                Object::Array(vec![Object::Integer(1), Object::Float(0.5)]),
            ),
            ("UseBlackPtComp", Object::name("ON")),
        ]);
        assert!(validate_extgstate_dict(&mut v, &ok).is_ok());
    }
}
