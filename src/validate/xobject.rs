//! XObjects and image arrays (Go: validate/xObject.go, image_arrays.go).
//!
//! Differences from Go, besides the ones in [`super::object`]:
//!
//! * A stream dictionary that Go repairs through its shared map (`Type`, an inferred `Subtype`, a
//!   null coordinate of `BBox`, a direct `Resources` dictionary that lost keys, a `Metadata` entry
//!   that refers to nothing) is changed on a copy here. [`validate_xobject_stream_dict`] stores
//!   these entries back to the table when it was given a reference; give it the reference, not
//!   the dereferenced stream. [`validate_form_stream_dict`] and
//!   [`validate_xobject_stream_dict_contents`] store them to the `owner` reference the stream was
//!   dereferenced from (Go shares the map with the table there too).
//! * The recursion depth of the image graph (Go: `imageTraversal`, carried in the context) is a
//!   parameter of the functions of this file. The result is the same: the traversal is created
//!   by the outermost image and only images call each other.
//! * Relaxed validation only. What Go reports as a notice is accepted without a call: a zero
//!   image dimension, an `ImageMask` image with a `ColorSpace`, a `BitsPerComponent` of 16 before
//!   PDF 1.5 or one that does not match a Flate, LZW or RunLength sample size, and a colour key
//!   mask that has the components of the base space of an `Indexed` image. `ColorSpace`,
//!   `BitsPerComponent` and the image dimensions are never required.
//! * The link collection of `collectFileSpecificationTarget` is not ported: it does nothing when
//!   `ValidateLinks` is off (docs/scope.md section 3.2). `PieceInfo` is not validated in
//!   relaxed mode (`validateFormStreamPieceInfo`).
//! * `validateOPIDictV13` and `validateOPIDictV20` take the dictionary as a copy: the repair of a
//!   null coordinate in a `CropRect` that is a direct array is not stored (one that is a
//!   reference is).
//! * [`validate_xobject_resource_dict`] takes the resource dictionary as the object it is, as the
//!   resource validator of the pages gives it.

use super::Validator;
use super::colorspace::{ColorSpaceRole, color_space_array_name, validate_color_space_entry};
use super::file_spec::validate_file_spec_entry;
use super::metadata::validate_metadata;
use super::object::{
    OPTIONAL, REQUIRED, array_cardinality_error, object_context, validate_array_entry,
    validate_array_exact_length, validate_array_pairs, validate_boolean_entry, validate_dict_entry,
    validate_entry, validate_integer_array_entry, validate_integer_entry, validate_name_entry,
    validate_number_array_entry, validate_number_entry, validate_rectangle_entry,
    validate_stream_dict_entry, validate_stream_dict_for_object, validate_string_array_entry,
    validate_string_entry,
};
use super::optional_content::validate_optional_content;
use super::page::{validate_group_entry, validate_resource_dict_repaired};
use crate::error::Error;
use crate::filter::{CCITT_FAX, DCT, FLATE, JBIG2, JPX, LZW, RUN_LENGTH};
use crate::model::deref::type_name;
use crate::model::{Entry, EntryObject, Version, XRefTable};
use crate::object::text::string_or_hex_literal;
use crate::object::{Dict, Object, PdfFilter, Ref, StreamDict};

/// `isAlternateImageStreamDict`.
pub const IS_ALTERNATE_IMAGE_STREAM_DICT: bool = true;

/// `isNoAlternateImageStreamDict`.
pub const IS_NO_ALTERNATE_IMAGE_STREAM_DICT: bool = false;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The entries of a stream dictionary that validation repairs.
const REPAIRED_KEYS: [&[u8]; 5] = [b"Type", b"Subtype", b"BBox", b"Resources", b"Metadata"];

/// Stores the repairs of the validators to the stream the reference `r` refers to: the entries
/// `Type`, `Subtype`, `BBox`, `Resources` and `Metadata` of `local` replace those of the stored
/// dictionary (an entry that `local` does not have is removed). Does nothing if `r` is not a
/// stream.
pub fn persist_stream_entries(x: &mut XRefTable, r: Ref, local: &Dict) {
    let Some(Entry {
        object: Some(EntryObject::Obj(Object::Stream(stored))),
        ..
    }) = x.table.get_mut(&r.num)
    else {
        return;
    };
    for key in REPAIRED_KEYS {
        match local.find(key) {
            Some(val) => stored.dict.set(key, val.clone()),
            None => {
                stored.dict.0.remove(key);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reference XObjects, OPI

/// `validateReferenceDictPageEntry`.
fn validate_reference_dict_page_entry(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    match x
        .dereference(o)
        .map_err(|e| e.context("reference dict Page: dereference"))?
    {
        Object::Null | Object::Integer(_) | Object::String(_) | Object::Hex(_) => Ok(()),
        other => Err(Error::Validation(format!(
            "reference dict Page: expected integer or text string, got {}",
            type_name(other)
        ))),
    }
}

/// `validateReferenceDict` (8.10.4 Reference XObjects).
fn validate_reference_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "refDict";

    // F, file spec, required
    validate_file_spec_entry(x, d, dict_name, "F", REQUIRED, Version::V10)
        .map_err(|e| e.context("refDict.F"))?;

    // Page, integer or text string, required
    let Some(o) = d.find(b"Page") else {
        return Err(Error::Validation(
            "refDict.Page: missing required entry".to_string(),
        ));
    };
    validate_reference_dict_page_entry(x, o).map_err(|e| e.context("refDict.Page"))?;

    // ID, string array, optional
    validate_string_array_entry(
        x,
        d,
        dict_name,
        "ID",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )
    .map_err(|e| e.context("refDict.ID"))?;
    Ok(())
}

/// `validateOPIDictV13Part1`.
fn validate_opi_dict_v13_part1(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let since = Version::V10;
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"OPI"),
    )?;
    validate_number_entry(
        x,
        d,
        dict_name,
        "Version",
        REQUIRED,
        since,
        Some(&|f: f64| f == 1.3),
    )?;
    validate_file_spec_entry(x, d, dict_name, "F", REQUIRED, since)?;
    validate_string_entry(x, d, dict_name, "ID", OPTIONAL, since, None)?;
    validate_string_entry(x, d, dict_name, "Comments", OPTIONAL, since, None)?;
    validate_integer_array_entry(
        x,
        d,
        dict_name,
        "Size",
        REQUIRED,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    validate_rectangle_entry(x, d, dict_name, "CropRect", REQUIRED, since, None)?;
    validate_rectangle_entry(x, d, dict_name, "CropFixed", OPTIONAL, since, None)?;
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "Position",
        REQUIRED,
        since,
        Some(&|a: &[Object]| a.len() == 8),
    )?;
    Ok(())
}

/// `validateOPIIntegerArrayEntry`: the optional array entry and its integers.
fn validate_opi_integer_array_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
) -> Result<Option<(Vec<Object>, Vec<i64>)>, Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, OPTIONAL, Version::V10, None)?
    else {
        return Ok(None);
    };
    let mut values = Vec::new();
    for (i, raw) in a.iter().enumerate() {
        let o = x.dereference(raw).map_err(|e| {
            e.context(format!(
                "{dict_name}.{entry_name}[{i}]: dereference integer"
            ))
        })?;
        let Object::Integer(n) = o else {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: expected integer"
            )));
        };
        values.push(*n);
    }
    Ok(Some((a, values)))
}

/// `validateOPIImageType`: the bits per sample (the second value), if there is an `ImageType`.
fn validate_opi_image_type(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<Option<i64>, Error> {
    let Some((a, values)) = validate_opi_integer_array_entry(x, d, dict_name, "ImageType")? else {
        return Ok(None);
    };
    validate_array_exact_length(&a, dict_name, "ImageType", 2)?;
    for (i, value) in values.iter().enumerate() {
        if *value <= 0 {
            return Err(Error::Validation(format!(
                "{dict_name}.ImageType[{i}]: invalid value {value}, expected a positive integer"
            )));
        }
    }
    Ok(values.get(1).copied())
}

/// `validateOPIGrayMapLength`: `2^bits_per_sample` values.
fn validate_opi_gray_map_length(
    a: &[Object],
    bits_per_sample: i64,
    dict_name: &str,
) -> Result<(), Error> {
    let mut expected = format!("2^{bits_per_sample} values, matching ImageType bits per sample");
    if let Some(bits) = u32::try_from(bits_per_sample).ok().filter(|b| *b < 64) {
        let length = 1u64 << bits;
        if u64::try_from(a.len()).is_ok_and(|l| l == length) {
            return Ok(());
        }
        expected = format!("{length} (2^{bits}), matching ImageType bits per sample");
    }
    Err(array_cardinality_error(
        dict_name,
        "GrayMap",
        a.len(),
        &expected,
    ))
}

/// `validateOPIGrayMap`.
fn validate_opi_gray_map(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    bits_per_sample: Option<i64>,
) -> Result<(), Error> {
    let Some((a, values)) = validate_opi_integer_array_entry(x, d, dict_name, "GrayMap")? else {
        return Ok(());
    };
    let Some(bits_per_sample) = bits_per_sample else {
        return Err(Error::Validation(format!(
            "{dict_name}.GrayMap: ImageType is required to determine the expected length"
        )));
    };
    validate_opi_gray_map_length(&a, bits_per_sample, dict_name)?;
    for (i, value) in values.iter().enumerate() {
        if !(0..=65535).contains(value) {
            return Err(Error::Validation(format!(
                "{dict_name}.GrayMap[{i}]: invalid value {value}, expected 0 through 65535"
            )));
        }
    }
    Ok(())
}

/// `validateOPITags`: pairs of a TIFF tag number and a text string.
fn validate_opi_tags(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let entry_name = "Tags";
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };
    validate_array_pairs(&a, dict_name, entry_name, 0)?;
    for (k, pair) in a.chunks_exact(2).enumerate() {
        let i = 2 * k;
        let tag = x.dereference(&pair[0]).map_err(|e| {
            e.context(format!(
                "{dict_name}.{entry_name}[{i}]: dereference tag number"
            ))
        })?;
        if !matches!(tag, Object::Integer(_)) {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{i}]: expected TIFF tag number integer"
            )));
        }
        let text = x.dereference(&pair[1]).map_err(|e| {
            e.context(format!(
                "{dict_name}.{entry_name}[{}]: dereference tag text",
                i + 1
            ))
        })?;
        if string_or_hex_literal(text).is_err() {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}[{}]: expected TIFF tag text string",
                i + 1
            )));
        }
    }
    Ok(())
}

/// `validateOPIDictV13Part2`.
fn validate_opi_dict_v13_part2(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    let since = Version::V10;

    // Resolution, optional, array of numbers, len 2
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "Resolution",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )?;

    // ColorType, optional, name (relaxed validation also accepts Intrinsic)
    validate_name_entry(
        x,
        d,
        dict_name,
        "ColorType",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| matches!(s, b"Process" | b"Spot" | b"Separation" | b"Intrinsic")),
    )?;

    // Color, optional, array, len 5
    validate_array_entry(
        x,
        d,
        dict_name,
        "Color",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 5),
    )?;

    // Tint, optional, number
    validate_number_entry(x, d, dict_name, "Tint", OPTIONAL, since, None)?;

    // Overprint, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Overprint", OPTIONAL, since, None)?;

    // ImageType, optional, array of integers, len 2
    let bits_per_sample = validate_opi_image_type(x, d, dict_name)?;

    // GrayMap, optional, array of integers
    validate_opi_gray_map(x, d, dict_name, bits_per_sample)?;

    // Transparency, optional, boolean
    validate_boolean_entry(x, d, dict_name, "Transparency", OPTIONAL, since, None)?;

    // Tags, optional, array
    validate_opi_tags(x, d, dict_name)
}

/// `validateOPIDictV13` (14.11.7 Open Prepresse interface).
fn validate_opi_dict_v13(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "opiDictV13";
    validate_opi_dict_v13_part1(x, d, dict_name)?;
    validate_opi_dict_v13_part2(x, d, dict_name)
}

/// `validateOPIInksArray`: `/monochrome` and then pairs of a colourant name and a tint.
fn validate_opi_inks_array(x: &mut XRefTable, a: &[Object], dict_name: &str) -> Result<(), Error> {
    if a.len() < 3 || a.len().is_multiple_of(2) {
        return Err(array_cardinality_error(
            dict_name,
            "Inks",
            a.len(),
            "/monochrome followed by one or more colourant string and tint pairs",
        ));
    }
    let marker = x
        .dereference(&a[0])
        .map_err(|e| e.context(format!("{dict_name}.Inks[0]: dereference marker")))?;
    if !matches!(marker, Object::Name(n) if n == b"monochrome") {
        return Err(Error::Validation(format!(
            "{dict_name}.Inks[0]: expected /monochrome marker"
        )));
    }
    for (k, pair) in a[1..].chunks_exact(2).enumerate() {
        let i = 1 + 2 * k;
        let colorant = x
            .dereference(&pair[0])
            .map_err(|e| e.context(format!("{dict_name}.Inks[{i}]: dereference colourant name")))?;
        if string_or_hex_literal(colorant).is_err() {
            return Err(Error::Validation(format!(
                "{dict_name}.Inks[{i}]: expected colourant name string"
            )));
        }
        let tint = x
            .dereference_number(&pair[1])
            .map_err(|e| e.context(format!("{dict_name}.Inks[{}]: expected tint number", i + 1)))?;
        if !(0.0..=1.0).contains(&tint) {
            return Err(Error::Validation(format!(
                "{dict_name}.Inks[{}]: invalid tint {tint}, expected 0 through 1",
                i + 1
            )));
        }
    }
    Ok(())
}

/// `validateOPIDictInks`.
fn validate_opi_dict_inks(x: &mut XRefTable, raw: &Object, dict_name: &str) -> Result<(), Error> {
    let o = x.dereference(raw)?.clone();
    match o {
        Object::Null => Ok(()),
        Object::Name(n) => {
            if n == b"full_color" || n == b"registration" {
                Ok(())
            } else {
                Err(Error::Validation(format!(
                    "{dict_name}.Inks: invalid colourant name {}",
                    lossy(&n)
                )))
            }
        }
        Object::Array(a) => validate_opi_inks_array(x, &a, dict_name),
        other => Err(Error::Validation(format!(
            "{dict_name}.Inks: expected name or array, got {}",
            type_name(&other)
        ))),
    }
}

/// `validateOPIDictV20`.
fn validate_opi_dict_v20(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "opiDictV20";
    let since = Version::V10;

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"OPI"),
    )?;
    validate_number_entry(
        x,
        d,
        dict_name,
        "Version",
        REQUIRED,
        since,
        Some(&|f: f64| f == 2.0),
    )?;
    validate_file_spec_entry(x, d, dict_name, "F", REQUIRED, since)?;
    validate_string_entry(x, d, dict_name, "MainImage", OPTIONAL, since, None)?;
    validate_opi_tags(x, d, dict_name)?;
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "Size",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    validate_rectangle_entry(x, d, dict_name, "CropRect", OPTIONAL, since, None)?;
    validate_boolean_entry(x, d, dict_name, "Overprint", OPTIONAL, since, None)?;
    if let Some(o) = d.find(b"Inks") {
        validate_opi_dict_inks(x, o, dict_name)?;
    }
    validate_integer_array_entry(
        x,
        d,
        dict_name,
        "IncludedImageDimensions",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    validate_integer_entry(
        x,
        d,
        dict_name,
        "IncludedImageQuality",
        OPTIONAL,
        since,
        Some(&|i: i64| (1..=3).contains(&i)),
    )?;
    Ok(())
}

/// `validateOPIVersionDict` (14.11.7): exactly one entry, keyed `1.3` or `2.0`.
fn validate_opi_version_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    if d.len() != 1 {
        return Err(Error::Validation(
            "must have exactly one entry keyed 1.3 or 2.0".to_string(),
        ));
    }
    for (opi_version, obj) in d.iter() {
        if opi_version.as_slice() != b"1.3" && opi_version.as_slice() != b"2.0" {
            return Err(Error::Validation("invalid OPI version".to_string()));
        }
        let Some(d1) = x.dereference_dict(obj)?.cloned() else {
            return Ok(());
        };
        if opi_version.as_slice() == b"1.3" {
            validate_opi_dict_v13(x, &d1)?;
        } else {
            validate_opi_dict_v20(x, &d1)?;
        }
    }
    Ok(())
}

/// `validateEntryOPI`: the `OPI` entry of `d`.
fn validate_entry_opi(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    if let Some(d1) = validate_dict_entry(x, d, dict_name, entry_name, required, since, None)? {
        validate_opi_version_dict(x, &d1)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Image arrays (image_arrays.go)

/// `namedColorSpaceComponents`: the components of a colour space name, 0 for an unknown one.
fn named_color_space_components(name: &[u8]) -> usize {
    match name {
        b"DeviceGray" | b"CalGray" | b"Indexed" | b"Separation" => 1,
        b"DeviceRGB" | b"CalRGB" | b"Lab" => 3,
        b"DeviceCMYK" => 4,
        _ => 0,
    }
}

/// `iccColorSpaceComponents`.
fn icc_color_space_components(x: &mut XRefTable, a: &[Object]) -> Result<usize, Error> {
    if a.len() != 2 {
        return Err(Error::Validation(
            "ICCBased colour-space components: expected profile".to_string(),
        ));
    }
    let sd = validate_stream_dict_for_object(x, &a[1])?;
    let n = validate_integer_entry(
        x,
        &sd.dict,
        "ICCBasedColorSpace",
        "N",
        REQUIRED,
        Version::V10,
        Some(&|n: i64| n == 1 || n == 3 || n == 4),
    )?;
    Ok(usize::try_from(n.unwrap_or(0)).unwrap_or(0))
}

/// `colorSpaceComponents`: the number of components of a colour space that was validated,
/// without walking its graph. 0 is an absent colour space or a name that relaxed validation
/// accepts.
///
/// # Errors
/// [`Error::Validation`] for a colour space that is no name or array, an empty array, a missing
/// profile or an empty `DeviceN` name list, and the errors of the dereference.
pub fn color_space_components(x: &mut XRefTable, o: &Object) -> Result<usize, Error> {
    let target = x.dereference(o)?.clone();
    let a = match target {
        Object::Null => return Ok(0),
        Object::Name(n) => return Ok(named_color_space_components(&n)),
        Object::Array(a) => a,
        other => {
            return Err(Error::Validation(format!(
                "colour-space components: expected name or array, got {}",
                type_name(&other)
            )));
        }
    };
    if a.is_empty() {
        return Err(Error::Validation(
            "colour-space components: empty array".to_string(),
        ));
    }
    let name = color_space_array_name(x, &a)?;
    match name.as_slice() {
        b"ICCBased" => icc_color_space_components(x, &a),
        b"DeviceN" => {
            if a.len() < 2 {
                return Err(Error::Validation(
                    "DeviceN colour-space components: missing names".to_string(),
                ));
            }
            let n = x.dereference_array(&a[1])?.map_or(0, |names| names.len());
            if n == 0 {
                return Err(Error::Validation(
                    "DeviceN colour-space components: empty names".to_string(),
                ));
            }
            Ok(n)
        }
        other => Ok(named_color_space_components(other)),
    }
}

/// `imageSampleFilter`: the last filter of the stream, which produces the samples.
fn image_sample_filter(sd: &StreamDict) -> Option<&PdfFilter> {
    sd.filter_pipeline.last()
}

fn is_jpx(sd: &StreamDict) -> bool {
    image_sample_filter(sd).is_some_and(|f| f.name == JPX)
}

/// `validateImageArrayLength`: two values per component, or any number of pairs if the number of
/// components is not known.
fn validate_image_array_length(
    a: &[Object],
    dict_name: &str,
    entry_name: &str,
    components: usize,
) -> Result<(), Error> {
    if components == 0 {
        return validate_array_pairs(a, dict_name, entry_name, 1);
    }
    if a.len().is_multiple_of(2) && a.len() / 2 == components {
        return Ok(());
    }
    let expected = format!("two values per colour component ({components} components)");
    Err(array_cardinality_error(
        dict_name,
        entry_name,
        a.len(),
        &expected,
    ))
}

/// `validateImageDecode`: the `Decode` array of an image.
fn validate_image_decode(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    components: usize,
    image_mask: bool,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(v.x, d, dict_name, "Decode", OPTIONAL, Version::V10, None)?
    else {
        return Ok(());
    };
    validate_image_array_length(&a, dict_name, "Decode", components)?;
    let mut previous = 0.0f64;
    for (i, o) in a.iter().enumerate() {
        v.check()?;
        let n =
            v.x.dereference_number(o)
                .map_err(|e| e.context(format!("{dict_name}.Decode[{i}]: expected number")))?;
        if image_mask && ((n != 0.0 && n != 1.0) || (i == 1 && n == previous)) {
            return Err(Error::Validation(format!(
                "{dict_name}.Decode[{i}]: image mask requires [0 1] or [1 0]"
            )));
        }
        previous = n;
    }
    Ok(())
}

/// `relaxedIndexedMaskComponents`: relaxed validation accepts the mask pairs of the base space of
/// an `Indexed` image, which older producers write. The colour space has been validated.
fn relaxed_indexed_mask_components(
    x: &mut XRefTable,
    d: &Dict,
    a: &[Object],
    components: usize,
) -> usize {
    if a.len() == components.saturating_mul(2) {
        return components;
    }
    let Some(raw) = d.find(b"ColorSpace") else {
        return components;
    };
    let Some(cs) = x.dereference_array(raw).ok().flatten().cloned() else {
        return components;
    };
    if cs.len() != 4 {
        return components;
    }
    match color_space_array_name(x, &cs) {
        Ok(name) if name == b"Indexed" => {}
        _ => return components,
    }
    match color_space_components(x, &cs[1]) {
        Ok(n) if n > 1 && a.len() == n.saturating_mul(2) => n,
        _ => components,
    }
}

/// `validateColorKeyMaskValue`.
fn validate_color_key_mask_value(
    n: i64,
    previous: i64,
    bits: Option<i64>,
    index: usize,
    dict_name: &str,
) -> Result<(), Error> {
    if let Some(bits) = bits
        && bits <= 0
    {
        return Err(Error::Validation(format!(
            "{dict_name}.Mask: invalid BitsPerComponent {bits}, expected positive integer"
        )));
    }
    if n < 0 {
        return Err(Error::Validation(format!(
            "{dict_name}.Mask[{index}]: invalid sample {n}, expected nonnegative integer"
        )));
    }
    // Compare the sample's bit length without an overflowing shift, including when the bit depth
    // is unavailable.
    if let Some(bits) = bits {
        let sample = u64::try_from(n).unwrap_or(0);
        let shifted = u32::try_from(bits)
            .ok()
            .and_then(|s| sample.checked_shr(s))
            .unwrap_or(0);
        if shifted != 0 {
            return Err(Error::Validation(format!(
                "{dict_name}.Mask[{index}]: sample {n} exceeds BitsPerComponent {bits}"
            )));
        }
    }
    if index % 2 == 1 && n < previous {
        return Err(Error::Validation(format!(
            "{dict_name}.Mask[{index}]: maximum {n} is less than minimum {previous}"
        )));
    }
    Ok(())
}

/// `validateColorKeyMask`.
fn validate_color_key_mask(
    v: &mut Validator<'_>,
    a: &[Object],
    dict_name: &str,
    components: usize,
    bits: Option<i64>,
) -> Result<(), Error> {
    validate_image_array_length(a, dict_name, "Mask", components)?;
    let mut previous = 0;
    for (i, o) in a.iter().enumerate() {
        v.check()?;
        let n = match v.x.dereference_integer(o) {
            Err(e) => {
                return Err(e.context(format!("{dict_name}.Mask[{i}]: expected integer")));
            }
            Ok(None) => {
                return Err(Error::Validation(format!(
                    "{dict_name}.Mask[{i}]: expected integer, got null"
                )));
            }
            Ok(Some(n)) => n,
        };
        validate_color_key_mask_value(n, previous, bits, i, dict_name)?;
        previous = n;
    }
    Ok(())
}

/// `validateImageArrays`: the `Mask` and `Decode` entries of an image (8.9.5).
fn validate_image_arrays(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    dict_name: &str,
    image_mask: bool,
    bpc: Option<i64>,
    depth: u64,
) -> Result<(), Error> {
    let mut components = 1;
    if !image_mask {
        let raw = sd.dict.find(b"ColorSpace").unwrap_or(&Object::Null);
        components = color_space_components(v.x, raw)
            .map_err(|e| e.context(format!("{dict_name}.ColorSpace")))?;
        let bits = if is_jpx(sd) { None } else { bpc };
        validate_mask_entry(
            v,
            &sd.dict,
            dict_name,
            "Mask",
            OPTIONAL,
            Version::V13,
            components,
            bits,
            depth,
        )?;
        // JPX without an explicit colour space ignores Decode; its component count comes from the
        // codestream.
        if components == 0 && is_jpx(sd) && v.x.dereference(raw)?.is_null() {
            return Ok(());
        }
    }
    validate_image_decode(v, &sd.dict, dict_name, components, image_mask)
}

// ---------------------------------------------------------------------------------------------
// Images

/// `validateMaskStreamDict`: an explicit mask, which is an image.
fn validate_mask_stream_dict(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
    depth: u64,
) -> Result<(), Error> {
    let (t, _) =
        v.x.dereference_name_entry(&sd.dict, b"Type")
            .map_err(|e| e.context("mask stream dict Type"))?;
    if let Some(t) = t
        && t != b"XObject"
    {
        return Err(Error::Validation(format!(
            "mask stream dict Type: expected XObject, got {:?}",
            lossy(&t)
        )));
    }

    let (subtype, _) =
        v.x.dereference_name_entry(&sd.dict, b"Subtype")
            .map_err(|e| e.context("mask stream dict Subtype"))?;
    if subtype.as_deref() != Some(&b"Image"[..]) {
        return Err(Error::Validation(
            "mask stream dict Subtype: expected Image".to_string(),
        ));
    }

    validate_image_stream_dict_inner(v, sd, IS_NO_ALTERNATE_IMAGE_STREAM_DICT, depth)
        .map_err(|e| e.context("mask image stream dict"))
}

/// `validateMaskEntry`: a stream (explicit masking, another image) or an array of colours (colour
/// key masking).
#[allow(clippy::too_many_arguments)]
fn validate_mask_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    components: usize,
    bits: Option<i64>,
    depth: u64,
) -> Result<(), Error> {
    let context = |e: Error| e.context(format!("{dict_name}.{entry_name}"));
    let Some(o) =
        validate_entry(v.x, d, dict_name, entry_name, required, since).map_err(context)?
    else {
        return Ok(());
    };
    match o {
        Object::Stream(mut sd) => validate_mask_stream_dict(v, &mut sd, depth).map_err(context),
        Object::Array(a) => {
            let components = relaxed_indexed_mask_components(v.x, d, &a, components);
            validate_color_key_mask(v, &a, dict_name, components, bits)
        }
        other => Err(context(Error::Validation(format!(
            "expected image stream dict or color key array, got {}",
            type_name(&other)
        )))),
    }
}

/// `validateAlternateImageStreamDicts`.
fn validate_alternate_image_stream_dicts(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
    depth: u64,
) -> Result<(), Error> {
    let context = |e: Error| e.context(format!("{dict_name}.{entry_name}"));
    let a = validate_array_entry(v.x, d, dict_name, entry_name, required, since, None)
        .map_err(context)?;
    let Some(a) = a else {
        if required {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}: missing required entry"
            )));
        }
        return Ok(());
    };

    for (i, o) in a.iter().enumerate() {
        v.check()?;
        let context = |e: Error| e.context(format!("{dict_name}.{entry_name}[{i}]"));
        let mut sd = validate_stream_dict_for_object(v.x, o).map_err(context)?;
        validate_image_stream_dict_inner(v, &mut sd, IS_ALTERNATE_IMAGE_STREAM_DICT, depth)
            .map_err(context)?;
    }
    Ok(())
}

/// `validateImageMaskEntries`: an image mask has neither `ColorSpace` nor `Mask`. Relaxed
/// validation accepts a `ColorSpace`.
fn validate_image_mask_entries(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<(), Error> {
    // see 8.9.5
    validate_entry(x, d, dict_name, "ColorSpace", OPTIONAL, Version::V10)?;
    if validate_entry(x, d, dict_name, "Mask", OPTIONAL, Version::V10)?.is_some() {
        return Err(Error::Validation(format!(
            "{dict_name}.Mask: not permitted for an image mask"
        )));
    }
    Ok(())
}

/// `imageFilterBits`: the sample size a filter implies, 0 if it implies none.
fn image_filter_bits(x: &mut XRefTable, f: &PdfFilter) -> Result<i64, Error> {
    let name = f.name.as_slice();
    if name == CCITT_FAX || name == JBIG2 {
        return Ok(1);
    }
    if name == RUN_LENGTH || name == DCT {
        return Ok(8);
    }
    if name == FLATE || name == LZW {
        let Some(parms) = f.decode_parms.as_ref() else {
            return Ok(0);
        };
        let Some(predictor) = parms.find(b"Predictor") else {
            return Ok(0);
        };
        if x.dereference_integer(predictor)?.is_none_or(|p| p <= 1) {
            return Ok(0);
        }
        let bits = validate_integer_entry(
            x,
            parms,
            "image DecodeParms",
            "BitsPerComponent",
            OPTIONAL,
            Version::V10,
            Some(&|i: i64| matches!(i, 1 | 2 | 4 | 8 | 16)),
        )?;
        return Ok(bits.unwrap_or(8));
    }
    Ok(0)
}

/// `validateImageFilterBits`: `BitsPerComponent` against the sample size of the filter (8.9.5).
/// Relaxed validation accepts a mismatch for Flate, LZW and RunLength.
fn validate_image_filter_bits(
    x: &mut XRefTable,
    sd: &StreamDict,
    bpc: Option<i64>,
    is_image_mask: bool,
) -> Result<(), Error> {
    if bpc.is_none() && !is_image_mask {
        return Ok(());
    }
    let bits = bpc.unwrap_or(1);
    let Some(f) = image_sample_filter(sd) else {
        return Ok(());
    };
    let expected = image_filter_bits(x, f)
        .map_err(|e| e.context(format!("image {} DecodeParms", lossy(&f.name))))?;
    if expected != 0
        && bits != expected
        && !(f.name == FLATE || f.name == LZW || f.name == RUN_LENGTH)
    {
        return Err(Error::Validation(format!(
            "image BitsPerComponent {bits} incompatible with {} sample size {expected}",
            lossy(&f.name)
        )));
    }
    Ok(())
}

/// `validateImageStreamDictPart1`: the dimensions, `ImageMask` and `ColorSpace`. The result says
/// whether the image is an image mask.
fn validate_image_stream_dict_part1(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    dict_name: &str,
) -> Result<bool, Error> {
    // see 8.9.5. Relaxed validation takes a zero dimension, and requires nothing.
    for key in ["Width", "Height"] {
        validate_integer_entry(
            v.x,
            &sd.dict,
            dict_name,
            key,
            OPTIONAL,
            Version::V10,
            Some(&|i: i64| i >= 0),
        )?;
    }

    // ImageMask, boolean, optional
    let image_mask = validate_boolean_entry(
        v.x,
        &sd.dict,
        dict_name,
        "ImageMask",
        OPTIONAL,
        Version::V10,
        None,
    )?
    .unwrap_or(false);
    if image_mask {
        validate_image_mask_entries(v.x, &sd.dict, dict_name)?;
        return Ok(true);
    }

    // ColorSpace, name or array; not allowed for imagemasks.
    validate_color_space_entry(
        v,
        &sd.dict,
        dict_name,
        "ColorSpace",
        OPTIONAL,
        ColorSpaceRole::NoPattern,
    )?;
    Ok(false)
}

/// `validateImageBitsPerComponent`: `BitsPerComponent`, if there is one.
fn validate_image_bits_per_component(
    x: &mut XRefTable,
    sd: &StreamDict,
    dict_name: &str,
    is_image_mask: bool,
) -> Result<Option<i64>, Error> {
    // see 8.9.5
    let jpx = is_jpx(sd);
    let bpc = validate_integer_entry(
        x,
        &sd.dict,
        dict_name,
        "BitsPerComponent",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| {
            if is_image_mask {
                // For imageMasks BitsPerComponent must be 1.
                i == 1
            } else {
                jpx || matches!(i, 1 | 2 | 4 | 8 | 16)
            }
        }),
    )
    .map_err(|e| e.context(format!("{dict_name}: invalid BitsPerComponent")))?;
    if jpx {
        return Ok(bpc);
    }
    // A BitsPerComponent of 16 needs PDF 1.5; relaxed validation only adds a notice.
    validate_image_filter_bits(x, sd, bpc, is_image_mask)?;
    Ok(bpc)
}

/// `validateImageStreamDictPart2`.
fn validate_image_stream_dict_part2(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    dict_name: &str,
    is_image_mask: bool,
    is_alternate: bool,
    depth: u64,
) -> Result<(), Error> {
    // see 8.9.5
    let bpc = validate_image_bits_per_component(v.x, sd, dict_name, is_image_mask)?;

    // Note 8.6.5.8: If a PDF processor does not recognise the specified name, it shall use the
    // RelativeColorimetric intent by default.
    validate_name_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Intent",
        OPTIONAL,
        Version::V11,
        None,
    )?;

    validate_image_arrays(v, sd, dict_name, is_image_mask, bpc, depth)?;

    // Interpolate, boolean, optional
    validate_boolean_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Interpolate",
        OPTIONAL,
        Version::V10,
        None,
    )?;

    // Alternates, array, optional, since V1.3
    if !is_alternate {
        validate_alternate_image_stream_dicts(
            v,
            &sd.dict,
            dict_name,
            "Alternates",
            OPTIONAL,
            Version::V13,
            depth,
        )?;
    }
    Ok(())
}

/// `validateImageStreamDictBody`.
fn validate_image_stream_dict_body(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
    is_alternate: bool,
    depth: u64,
) -> Result<(), Error> {
    let dict_name = "imageStreamDict";

    let is_image_mask = validate_image_stream_dict_part1(v, sd, dict_name)?;
    validate_image_stream_dict_part2(v, sd, dict_name, is_image_mask, is_alternate, depth)?;

    // SMask, stream, optional, since V1.4 (V1.2 in relaxed validation)
    if let Some(mut sd1) = validate_stream_dict_entry(
        v.x,
        &sd.dict,
        dict_name,
        "SMask",
        OPTIONAL,
        Version::V12,
        None,
    )? {
        validate_image_stream_dict_inner(v, &mut sd1, IS_NO_ALTERNATE_IMAGE_STREAM_DICT, depth)?;
    }

    // SMaskInData, integer, optional
    validate_integer_entry(
        v.x,
        &sd.dict,
        dict_name,
        "SMaskInData",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // StructParent, integer, optional
    validate_integer_entry(
        v.x,
        &sd.dict,
        dict_name,
        "StructParent",
        OPTIONAL,
        Version::V13,
        None,
    )?;

    // ID, byte string, optional, since V1.3
    validate_string_entry(v.x, &sd.dict, dict_name, "ID", OPTIONAL, Version::V13, None)?;

    // OPI, dict, optional since V1.2
    validate_entry_opi(v.x, &sd.dict, dict_name, "OPI", OPTIONAL, Version::V12)?;

    // Metadata, stream, optional since V1.4
    validate_metadata(v.x, &mut sd.dict, OPTIONAL, Version::V14)?;

    // OC, dict, optional since V1.5
    validate_optional_content(v.x, &sd.dict, dict_name, "OC", OPTIONAL, Version::V15)
}

/// `imageTraversal.validate`: one more level of the image graph.
fn validate_image_stream_dict_inner(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
    is_alternate: bool,
    depth: u64,
) -> Result<(), Error> {
    v.check()?;
    let depth = depth + 1;
    v.x.check_recursion_depth("image graph", depth)?;
    validate_image_stream_dict_body(v, sd, is_alternate, depth)
}

/// `validateImageStreamDict` (8.9.5): the dictionary of an image XObject. The pass counts the
/// image graph from here (an image that is part of another one's graph is validated through the
/// functions of this file).
///
/// # Errors
/// [`Error::Cancelled`], `Sentinel::MaxRecursionDepth`, [`Error::Validation`] for an invalid
/// entry, and the errors of the lookups.
pub fn validate_image_stream_dict(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    is_alternate: bool,
) -> Result<(), Error> {
    validate_image_stream_dict_inner(v, &mut sd.clone(), is_alternate, 0)
}

// ---------------------------------------------------------------------------------------------
// Forms

/// `validateFormStreamDictPart1`.
fn validate_form_stream_dict_part1(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
    dict_name: &str,
) -> Result<(), Error> {
    validate_number_entry(
        v.x,
        &sd.dict,
        dict_name,
        "FormType",
        OPTIONAL,
        Version::V10,
        Some(&|f: f64| f == 1.0),
    )?;

    // BBox; a null coordinate of a direct array is repaired in the dictionary.
    let bbox = validate_rectangle_entry(
        v.x,
        &sd.dict,
        dict_name,
        "BBox",
        REQUIRED,
        Version::V10,
        None,
    )?;
    if let Some(a) = bbox
        && matches!(sd.dict.find(b"BBox"), Some(Object::Array(_)))
    {
        sd.dict.set(b"BBox", Object::Array(a));
    }

    validate_number_array_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Matrix",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 6),
    )?;

    // Resources, dict, optional, since V1.2; a direct dictionary is repaired in the dictionary.
    if let Some(o) = sd.dict.find(b"Resources").cloned()
        && let (_, Some(d)) = validate_resource_dict_repaired(v, &o)?
    {
        sd.dict.set(b"Resources", Object::Dict(d));
    }

    // Group, dict, optional, since V1.4
    validate_group_entry(v, &sd.dict, dict_name, OPTIONAL, Version::V14)?;

    // Ref, dict, optional, since V1.4
    if let Some(d) = validate_dict_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Ref",
        OPTIONAL,
        Version::V14,
        None,
    )? {
        validate_reference_dict(v.x, &d)?;
    }

    // Metadata, stream, optional, since V1.4
    validate_metadata(v.x, &mut sd.dict, OPTIONAL, Version::V14)
}

/// `validateFormStreamDictPart2`. Relaxed validation does not look at `PieceInfo`.
fn validate_form_stream_dict_part2(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // StructParent, integer
    let sp = validate_integer_entry(
        x,
        d,
        dict_name,
        "StructParent",
        OPTIONAL,
        Version::V13,
        None,
    )?;

    // StructParents, integer
    let sps = validate_integer_entry(
        x,
        d,
        dict_name,
        "StructParents",
        OPTIONAL,
        Version::V13,
        None,
    )?;
    if sp.is_some() && sps.is_some() {
        return Err(Error::Validation(
            "only \"StructParent\" or \"StructParents\" allowed".to_string(),
        ));
    }

    // OPI, dict, optional, since V1.2
    validate_entry_opi(x, d, dict_name, "OPI", OPTIONAL, Version::V12)?;

    // OC, optional, content group dict or content membership dict (V1.5; relaxed validation
    // takes V1.2)
    validate_optional_content(x, d, dict_name, "OC", OPTIONAL, Version::V12)?;

    // Name, name, optional (required in 1.0)
    let required = x.version()? == Version::V10;
    validate_name_entry(x, d, dict_name, "Name", required, Version::V10, None)?;
    Ok(())
}

/// `validateFormStreamDict` on a dictionary that is changed in place.
fn validate_form_stream_dict_inner(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
) -> Result<(), Error> {
    // 8.10 Form XObjects
    let dict_name = "formStreamDict";
    validate_form_stream_dict_part1(v, sd, dict_name)?;
    validate_form_stream_dict_part2(v.x, &sd.dict, dict_name)
}

/// `validateFormStreamDict` (8.10 Form XObjects): the dictionary of a form XObject. The repairs
/// are stored to `owner`, the reference `sd` was dereferenced from, if there is one (see the
/// module documentation).
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an invalid entry, and the errors of the
/// lookups.
pub fn validate_form_stream_dict(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    owner: Option<Ref>,
) -> Result<(), Error> {
    let mut sd = sd.clone();
    let r = validate_form_stream_dict_inner(v, &mut sd);
    if let Some(ir) = owner {
        persist_stream_entries(v.x, ir, &sd.dict);
    }
    r
}

// ---------------------------------------------------------------------------------------------
// XObjects

/// `validateXObjectType`: `Type` is `XObject`, or `Xobject`, which is repaired.
///
/// # Errors
/// [`Error::Validation`] for another name.
pub fn validate_xobject_type(x: &mut XRefTable, sd: &mut StreamDict) -> Result<(), Error> {
    let n = validate_name_entry(
        x,
        &sd.dict,
        "xObjectStreamDict",
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"XObject" || s == b"Xobject"),
    )
    .map_err(|e| e.context("xObjectStreamDict.Type"))?;

    // Repair "Xobject" to "XObject".
    if n.as_deref() == Some(&b"Xobject"[..]) {
        sd.dict.set(b"Type", Object::name("XObject"));
    }
    Ok(())
}

/// `validateXObjectStreamDictMissingSubtype`: a form if there is a `BBox`, an image if there are
/// `Width` and `Height`; the inferred `Subtype` is set.
fn validate_xobject_stream_dict_missing_subtype(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
) -> Result<(), Error> {
    if sd.dict.find(b"BBox").is_some() {
        validate_form_stream_dict_inner(v, sd)
            .map_err(|e| e.context("xObject form stream dict"))?;
        sd.dict.set(b"Subtype", Object::name("Form"));
        return Ok(());
    }

    if sd.dict.find(b"Width").is_none() || sd.dict.find(b"Height").is_none() {
        return Ok(());
    }

    validate_image_stream_dict_inner(v, sd, IS_NO_ALTERNATE_IMAGE_STREAM_DICT, 0)
        .map_err(|e| e.context("xObject image stream dict"))?;
    sd.dict.set(b"Subtype", Object::name("Image"));
    Ok(())
}

/// `validateXObjectStreamDictSubtype`.
fn validate_xobject_stream_dict_subtype(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
    subtype: &[u8],
) -> Result<(), Error> {
    let r = match subtype {
        b"Form" => validate_form_stream_dict_inner(v, sd),
        b"Image" => validate_image_stream_dict_inner(v, sd, IS_NO_ALTERNATE_IMAGE_STREAM_DICT, 0),
        b"PS" => Err(Error::Validation(
            "PostScript XObjects should not be used".to_string(),
        )),
        _ => {
            return Err(Error::Validation(format!(
                "xObjectStreamDict.Subtype: unknown subtype {:?}",
                lossy(subtype)
            )));
        }
    };
    r.map_err(|e| e.context(format!("xObject {} stream dict", lossy(subtype))))
}

/// `validateXObjectStreamDictContents` on a dictionary that is changed in place. Relaxed
/// validation does not require `Subtype`.
pub(super) fn validate_xobject_contents_inner(
    v: &mut Validator<'_>,
    sd: &mut StreamDict,
) -> Result<(), Error> {
    let dict_name = "xObjectStreamDict";

    validate_xobject_type(v.x, sd)?;

    let subtype = validate_name_entry(
        v.x,
        &sd.dict,
        dict_name,
        "Subtype",
        OPTIONAL,
        Version::V10,
        None,
    )
    .map_err(|e| e.context(format!("{dict_name}.Subtype")))?;

    match subtype {
        Some(s) if !s.is_empty() => validate_xobject_stream_dict_subtype(v, sd, &s),
        _ => validate_xobject_stream_dict_missing_subtype(v, sd),
    }
}

/// `validateXObjectStreamDictContents`: the stream dictionary of an XObject, which was
/// dereferenced already. The repairs are stored to `owner`, the reference `sd` was dereferenced
/// from, if there is one (see the module documentation).
///
/// # Errors
/// As [`validate_xobject_stream_dict`].
pub fn validate_xobject_stream_dict_contents(
    v: &mut Validator<'_>,
    sd: &StreamDict,
    owner: Option<Ref>,
) -> Result<(), Error> {
    let mut sd = sd.clone();
    let r = validate_xobject_contents_inner(v, &mut sd);
    if let Some(ir) = owner {
        persist_stream_entries(v.x, ir, &sd.dict);
    }
    r
}

/// `validateXObjectStreamDict` (8.8 External Objects): the form or image XObject `o` is or refers
/// to. An XObject is validated once (forms may be recursive, as Microsoft's are). The repairs are
/// stored to the table if `o` is a reference (see the module documentation).
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for an invalid XObject, and the errors of the
/// dereference.
pub fn validate_xobject_stream_dict(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    if o.is_null() {
        return Ok(());
    }

    // Dereference stream dict and ensure it is validated exactly once in order to handle
    // XObjects (forms) with recursive structures like produced by Microsoft.
    let (sd, valid) = v.x.dereference_stream_dict(o).map_err(|e| {
        e.context("xObject stream dict: dereference stream dict")
            .context(object_context("xObject", Some(o)))
    })?;
    if valid {
        return Ok(());
    }
    let Some(mut sd) = sd else {
        return Ok(());
    };

    let r = validate_xobject_contents_inner(v, &mut sd);
    if let Object::Ref(ir) = o {
        persist_stream_entries(v.x, *ir, &sd.dict);
    }
    r
}

/// `validateGroupAttributesDict` (11.6.6 Transparency Group XObjects).
///
/// # Errors
/// [`Error::Validation`] for an invalid entry, and the errors of the dereference.
pub fn validate_group_attributes_dict(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("group attributes dict: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };

    let dict_name = "groupAttributesDict";

    // Type, name, optional
    validate_name_entry(
        v.x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Group"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Type")))?;

    // S, name, required
    validate_name_entry(
        v.x,
        &d,
        dict_name,
        "S",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Transparency"),
    )
    .map_err(|e| e.context(format!("{dict_name}.S")))?;

    // CS, colorSpace, optional
    validate_color_space_entry(v, &d, dict_name, "CS", OPTIONAL, ColorSpaceRole::NoPattern)
        .map_err(|e| e.context(format!("{dict_name}.CS")))?;

    // I, boolean, optional
    validate_boolean_entry(v.x, &d, dict_name, "I", OPTIONAL, Version::V10, None)
        .map_err(|e| e.context(format!("{dict_name}.I")))?;
    Ok(())
}

/// `validateXObjectResourceDict`: the `XObject` resource dictionary `o` is or refers to, whose
/// entries are validated in key order.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for a version that is too low or an invalid
/// XObject, and the errors of the dereference.
pub fn validate_xobject_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("XObjectResourceDict", since)
        .map_err(|e| e.context("XObject resource dict: version"))?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("XObject resource dict: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Ok(());
    };

    // Iterate over XObject resource dictionary
    for (name, entry) in d.iter() {
        v.check()?;
        // Process XObject dict
        validate_xobject_stream_dict(v, entry).map_err(|e| {
            e.context(object_context(
                &format!("XObject resource {}", lossy(name)),
                Some(entry),
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;

    fn table(objects: Vec<(i64, Object)>) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn r(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn numbers(v: &[i64]) -> Object {
        Object::Array(v.iter().map(|i| Object::Integer(*i)).collect())
    }

    fn stream(entries: &[(&str, Object)]) -> Object {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        Object::Stream(Box::new(StreamDict::new(d, 0, None, None, vec![])))
    }

    fn form_xobject(resources: Option<Object>) -> Object {
        let mut entries = vec![
            ("Type", Object::name("XObject")),
            ("Subtype", Object::name("Form")),
            ("BBox", numbers(&[0, 0, 1, 1])),
        ];
        if let Some(r) = resources {
            entries.push(("Resources", r));
        }
        stream(&entries)
    }

    fn dict_of(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn valid(x: &XRefTable, n: i64) -> bool {
        x.table.get(&n).is_some_and(|e| e.valid)
    }

    #[test]
    fn a_cancelled_resource_traversal_validates_nothing() {
        let mut x = table((1..=4).map(|n| (n, form_xobject(None))).collect());
        let mut resources = Dict::default();
        for n in 1..=4i64 {
            resources.set(&[b'A' + u8::try_from(n - 1).unwrap()], r(n));
        }
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        let got = validate_xobject_resource_dict(&mut v, &Object::Dict(resources), Version::V10);
        assert!(matches!(got, Err(Error::Cancelled)));
        for n in 1..=4 {
            assert!(!valid(&x, n), "XObject {n}");
        }
    }

    #[test]
    fn recursive_and_shared_forms_are_validated_once() {
        let nested = Object::Dict(dict_of(&[(
            "XObject",
            Object::Dict(dict_of(&[("Self", r(1))])),
        )]));
        for (resources, nested) in [
            (dict_of(&[("Root", r(1))]), Some(nested)),
            (dict_of(&[("First", r(1)), ("Second", r(1))]), None),
        ] {
            let mut x = table(vec![(1, form_xobject(nested))]);
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            validate_xobject_resource_dict(&mut v, &Object::Dict(resources), Version::V10).unwrap();
            assert!(valid(&x, 1));
        }
    }

    #[test]
    fn a_missing_subtype_is_inferred_and_stored() {
        let mut x = table(vec![(
            1,
            stream(&[
                ("Type", Object::name("Xobject")),
                ("BBox", numbers(&[0, 0, 1, 1])),
            ]),
        )]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_xobject_stream_dict(&mut v, &r(1)).unwrap();
        let Some(Entry {
            object: Some(EntryObject::Obj(Object::Stream(sd))),
            ..
        }) = x.table.get(&1)
        else {
            panic!("not a stream");
        };
        assert_eq!(sd.dict.name_entry(b"Subtype"), Some(&b"Form"[..]));
        assert_eq!(sd.dict.name_entry(b"Type"), Some(&b"XObject"[..]));
    }

    fn stored_stream(x: &XRefTable) -> &StreamDict {
        let Some(Entry {
            object: Some(EntryObject::Obj(Object::Stream(sd))),
            ..
        }) = x.table.get(&1)
        else {
            panic!("not a stream");
        };
        sd
    }

    /// Go repairs the stream through the map it shares with the table: a null `BBox` coordinate
    /// and a direct `Resources` dictionary with a key that is not allowed are stored to the
    /// owner reference, and nothing is stored without one.
    #[test]
    fn form_repairs_are_stored_to_the_owner() {
        for owner in [
            Some(Ref {
                num: 1,
                generation: 0,
            }),
            None,
        ] {
            let mut bbox = numbers(&[0, 0, 0, 1]);
            if let Object::Array(a) = &mut bbox {
                a[2] = Object::Null;
            }
            let resources = Object::Dict(dict_of(&[("Bogus", Object::Integer(1))]));
            let mut x = table(vec![(
                1,
                stream(&[
                    ("Subtype", Object::name("Form")),
                    ("BBox", bbox),
                    ("Resources", resources),
                ]),
            )]);
            let sd = stored_stream(&x).clone();
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            validate_xobject_stream_dict_contents(&mut v, &sd, owner).unwrap();
            validate_form_stream_dict(&mut v, &sd, owner).unwrap();
            let stored = stored_stream(&x);
            let Some(Object::Array(a)) = stored.dict.find(b"BBox") else {
                panic!("no BBox");
            };
            let Some(Object::Dict(res)) = stored.dict.find(b"Resources") else {
                panic!("no Resources");
            };
            assert_eq!(matches!(a[2], Object::Integer(0)), owner.is_some());
            assert_eq!(res.find(b"Bogus").is_none(), owner.is_some());
        }
    }

    #[test]
    fn color_space_component_counts() {
        let mut x = table(vec![]);
        assert_eq!(
            color_space_components(&mut x, &Object::name("DeviceRGB")).unwrap(),
            3
        );
        assert_eq!(color_space_components(&mut x, &Object::Null).unwrap(), 0);
        let device_n = Object::Array(vec![
            Object::name("DeviceN"),
            Object::Array(vec![Object::name("A"), Object::name("B")]),
        ]);
        assert_eq!(color_space_components(&mut x, &device_n).unwrap(), 2);
        let indexed = Object::Array(vec![Object::name("Indexed")]);
        assert_eq!(color_space_components(&mut x, &indexed).unwrap(), 1);
        assert!(color_space_components(&mut x, &Object::Array(vec![])).is_err());
        assert!(color_space_components(&mut x, &Object::Integer(1)).is_err());
    }

    #[test]
    fn image_array_lengths() {
        let four = [Object::Null, Object::Null, Object::Null, Object::Null];
        assert!(validate_image_array_length(&four, "D", "Decode", 2).is_ok());
        assert!(validate_image_array_length(&four, "D", "Decode", 3).is_err());
        assert!(validate_image_array_length(&four, "D", "Decode", 0).is_ok());
        assert!(validate_image_array_length(&four[..1], "D", "Decode", 0).is_err());
    }

    #[test]
    fn color_key_mask_values() {
        assert!(validate_color_key_mask_value(255, 0, Some(8), 0, "D").is_ok());
        assert!(validate_color_key_mask_value(256, 0, Some(8), 0, "D").is_err());
        assert!(validate_color_key_mask_value(-1, 0, None, 0, "D").is_err());
        assert!(validate_color_key_mask_value(1, 0, Some(0), 0, "D").is_err());
        assert!(validate_color_key_mask_value(1, 2, None, 1, "D").is_err());
        assert!(validate_color_key_mask_value(1 << 40, 0, None, 0, "D").is_ok());
        assert!(validate_color_key_mask_value(1, 0, Some(70), 0, "D").is_ok());
    }

    #[test]
    fn opi_inks_and_gray_maps() {
        let mut x = table(vec![]);
        let inks = [
            Object::name("monochrome"),
            Object::String(b"Black".to_vec()),
            Object::Float(0.5),
        ];
        assert!(validate_opi_inks_array(&mut x, &inks, "D").is_ok());
        let bad_tint = [
            Object::name("monochrome"),
            Object::String(b"Black".to_vec()),
            Object::Integer(2),
        ];
        assert!(validate_opi_inks_array(&mut x, &bad_tint, "D").is_err());
        assert!(validate_opi_inks_array(&mut x, &inks[..2], "D").is_err());

        let map = vec![Object::Integer(0); 4];
        assert!(validate_opi_gray_map_length(&map, 2, "D").is_ok());
        assert!(validate_opi_gray_map_length(&map, 3, "D").is_err());
        assert!(validate_opi_gray_map_length(&map, 64, "D").is_err());
    }
}
