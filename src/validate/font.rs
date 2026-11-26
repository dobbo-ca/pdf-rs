//! Fonts (Go: validate/font.go).
//!
//! Relaxed validation only. The entry validators of [`super::object`] return owned copies, so a
//! repair that Go makes through the map it was given is written back here: a font dictionary that
//! is a reference is stored back into the table after its repairs, a direct font dictionary is
//! changed in the copy of the font resource dictionary, which [`validate_font_resource_dict`]
//! stores back if it is a reference and [`validate_font_resource_dict_repaired`] returns if it is
//! direct.
//!
//! Differences from Go:
//!
//! * The validators return the font name together with the error where Go does, as
//!   [`FontErr`]: `validateFontResources` keeps the name of a font it skips.
//! * `font: dereference dict: %w: %w` (two wrapped errors) is [`Sentinel::MissingFont`] with the
//!   original error in the message: the one place that tests for it swallows the error.
//! * A null coordinate in a direct `FontBBox` is not repaired in the stored descriptor (a
//!   `FontBBox` that is a reference is, by `validate_rectangle_entry`).

use super::Validator;
use super::metadata::validate_metadata;
use super::object::{
    OPTIONAL, REQUIRED, array_cardinality_error, object_context, validate_array_entry,
    validate_dict_entry, validate_entry, validate_integer_entry, validate_name_entry,
    validate_number_array, validate_number_array_entry, validate_number_entry,
    validate_rectangle_entry, validate_stream_dict_entry, validate_string_entry,
};
use super::page::validate_resource_dict;
use super::xobject::persist_stream_entries;
use crate::error::{Error, Sentinel};
use crate::model::{Entry, EntryObject, Version, XRefTable};
use crate::object::text::string_literal_to_string;
use crate::object::{Dict, Object, Ref, StreamDict};
use std::collections::{BTreeMap, BTreeSet};

/// A failure of a font validator, and the font name Go returns with it (empty if it returns
/// none).
#[derive(Debug)]
pub struct FontErr {
    pub name: Vec<u8>,
    pub err: Error,
}

impl From<Error> for FontErr {
    fn from(err: Error) -> FontErr {
        FontErr {
            name: Vec::new(),
            err,
        }
    }
}

type FontResult<T> = Result<T, FontErr>;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `ErrMissingFont`.
fn missing_font() -> Error {
    Error::Sentinel(Sentinel::MissingFont)
}

/// Stores `d` as the dictionary of the object `r` (Go changes the stored map in place).
fn store_dict(x: &mut XRefTable, r: Ref, d: Dict) {
    if let Some(Entry {
        object: Some(EntryObject::Obj(stored @ Object::Dict(_))),
        ..
    }) = x.table.get_mut(&r.num)
    {
        *stored = Object::Dict(d);
    }
}

/// `validateStandardType1Font`.
fn validate_standard_type1_font(s: &[u8]) -> bool {
    matches!(
        s,
        b"Times-Roman"
            | b"Times-Bold"
            | b"Times-Italic"
            | b"Times-BoldItalic"
            | b"Helvetica"
            | b"Helvetica-Bold"
            | b"Helvetica-Oblique"
            | b"Helvetica-BoldOblique"
            | b"Courier"
            | b"Courier-Bold"
            | b"Courier-Oblique"
            | b"Courier-BoldOblique"
            | b"Symbol"
            | b"ZapfDingbats"
    )
}

/// `fontDictStringEntry`: the text of a string literal entry, `None` for any other entry.
fn font_dict_string_entry(d: &Dict, key: &[u8]) -> Option<String> {
    match d.find(key)? {
        Object::String(s) => string_literal_to_string(s).ok(),
        _ => None,
    }
}

/// `repairStringType1FontDict`: a standard Type1 font whose `Type`, `Subtype` and `BaseFont` are
/// strings gets names. True if it changed `d`.
fn repair_string_type1_font_dict(d: &mut Dict) -> bool {
    let (Some(dict_type), Some(subtype), Some(base_font)) = (
        font_dict_string_entry(d, b"Type"),
        font_dict_string_entry(d, b"Subtype"),
        font_dict_string_entry(d, b"BaseFont"),
    ) else {
        return false;
    };
    if dict_type != "Font"
        || subtype != "Type1"
        || !validate_standard_type1_font(base_font.as_bytes())
    {
        return false;
    }
    d.update(b"Type", Object::name(dict_type));
    d.update(b"Subtype", Object::name(subtype));
    d.update(b"BaseFont", Object::name(base_font));
    true
}

/// `repairSelfReferentialFontToUnicode`: a `ToUnicode` that refers to the font itself is
/// removed. True if it changed `d`.
fn repair_self_referential_font_to_unicode(d: &mut Dict, ind_ref: Option<Ref>) -> bool {
    let Some(r) = ind_ref else {
        return false;
    };
    if d.indirect_ref_entry(b"ToUnicode") != Some(r) {
        return false;
    }
    d.delete(b"ToUnicode");
    true
}

/// `validateFontFile3SubType`: a hint about the font program. Relaxed validation accepts any
/// `Subtype` of a Type1 font.
fn validate_font_file3_subtype(
    x: &mut XRefTable,
    sd: &StreamDict,
    font_type: &str,
) -> Result<(), Error> {
    let (st, _) = x
        .dereference_name_entry(&sd.dict, b"Subtype")
        .map_err(|e| e.context("font file stream Subtype"))?;
    let Some(s) = st else {
        return Err(Error::Validation("missing Subtype".to_string()));
    };
    let unexpected = || Error::Validation(format!("{font_type}: unexpected Subtype {}", lossy(&s)));
    match font_type {
        "MMType1" if s != b"Type1C" => Err(unexpected()),
        "CIDFontType0" if s != b"CIDFontType0C" && s != b"OpenType" => Err(unexpected()),
        "CIDFontType2" | "TrueType" if s != b"OpenType" => Err(unexpected()),
        _ => Ok(()),
    }
}

/// `validateFontFile`.
fn validate_font_file(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    font_type: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(mut sd) =
        validate_stream_dict_entry(x, d, dict_name, entry_name, required, since, None)?
    else {
        return Ok(());
    };

    // Process font file stream dict entries.
    let compact_font_format = entry_name == "FontFile3";
    if compact_font_format {
        validate_font_file3_subtype(x, &sd, font_type)?;
    }

    let d_name = "fontFileStreamDict";
    validate_integer_entry(
        x,
        &sd.dict,
        d_name,
        "Length1",
        (font_type == "Type1" || font_type == "TrueType") && !compact_font_format,
        Version::V10,
        None,
    )?;
    validate_integer_entry(
        x,
        &sd.dict,
        d_name,
        "Length2",
        font_type == "Type1" && !compact_font_format,
        Version::V10,
        None,
    )?;
    validate_integer_entry(
        x,
        &sd.dict,
        d_name,
        "Length3",
        font_type == "Type1" && !compact_font_format,
        Version::V10,
        None,
    )?;

    // Metadata, stream, optional, since 1.4
    let r = validate_metadata(x, &mut sd.dict, OPTIONAL, Version::V14);
    // Go deletes a `Metadata` entry through the alias of the stored stream's map.
    if let Some(Object::Ref(ir)) = d.find(entry_name.as_bytes()) {
        persist_stream_entries(x, *ir, &sd.dict);
    }
    r
}

/// `validateFontDescriptorType`: relaxed validation accepts a missing `Type`.
fn validate_font_descriptor_type(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let (dict_type, _) = x
        .dereference_name_entry(d, b"Type")
        .map_err(|e| e.context("font descriptor Type"))?;
    match dict_type {
        Some(t) if t != b"FontDescriptor" && t != b"Font" => Err(Error::Validation(
            "corrupt font descriptor dict".to_string(),
        )),
        _ => Ok(()),
    }
}

/// `validateFontDescriptorFontName`: relaxed validation does not require the entry and accepts a
/// name or a string.
fn validate_font_descriptor_font_name(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if validate_name_entry(x, d, dict_name, "FontName", OPTIONAL, Version::V10, None).is_err() {
        let _ = validate_string_entry(x, d, dict_name, "FontName", OPTIONAL, Version::V10, None);
    }
    Ok(())
}

/// `validateFontDescriptorFontFamily`.
fn validate_font_descriptor_font_family(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if validate_name_entry(x, d, dict_name, "FontFamily", OPTIONAL, Version::V13, None).is_err() {
        let _ = validate_string_entry(x, d, dict_name, "FontFamily", OPTIONAL, Version::V13, None);
    }
    Ok(())
}

/// `validateFontDescriptorFontWeight`: a number, or in relaxed validation one of three names.
fn validate_font_descriptor_font_weight(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    if validate_number_entry(x, d, dict_name, "FontWeight", OPTIONAL, Version::V11, None).is_err() {
        validate_name_entry(
            x,
            d,
            dict_name,
            "FontWeight",
            OPTIONAL,
            Version::V11,
            Some(&|s: &[u8]| matches!(s, b"Regular" | b"Bold" | b"Italic")),
        )?;
    }
    Ok(())
}

/// `validateFontDescriptorPart1`.
fn validate_font_descriptor_part1(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    font_dict_type: &str,
) -> Result<(), Error> {
    validate_font_descriptor_type(x, d)?;
    validate_font_descriptor_font_name(x, d, dict_name)?;
    validate_font_descriptor_font_family(x, d, dict_name)?;
    validate_name_entry(x, d, dict_name, "FontStretch", OPTIONAL, Version::V12, None)?;
    validate_font_descriptor_font_weight(x, d, dict_name)?;
    // Relaxed: a missing "Flags" and a missing or corrupt "FontBBox" are skipped.
    let _ = validate_integer_entry(x, d, dict_name, "Flags", REQUIRED, Version::V10, None);
    let _ = validate_rectangle_entry(
        x,
        d,
        dict_name,
        "FontBBox",
        font_dict_type != "Type3",
        Version::V10,
        None,
    );
    // ItalicAngle: not required in relaxed validation.
    validate_number_entry(x, d, dict_name, "ItalicAngle", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateFontDescriptorFontFile`.
fn validate_font_descriptor_font_file(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    font_dict_type: &str,
) -> Result<(), Error> {
    match font_dict_type {
        "Type1" | "MMType1" => {
            validate_font_file(
                x,
                d,
                dict_name,
                "FontFile",
                font_dict_type,
                OPTIONAL,
                Version::V10,
            )?;
            validate_font_file(
                x,
                d,
                dict_name,
                "FontFile3",
                font_dict_type,
                OPTIONAL,
                Version::V12,
            )
        }
        "TrueType" | "CIDFontType2" => validate_font_file(
            x,
            d,
            dict_name,
            "FontFile2",
            font_dict_type,
            OPTIONAL,
            Version::V11,
        ),
        "CIDFontType0" => validate_font_file(
            x,
            d,
            dict_name,
            "FontFile3",
            font_dict_type,
            OPTIONAL,
            Version::V13,
        ),
        // No fontfile.
        "Type3" => Ok(()),
        other => Err(Error::Validation(format!("unknown fontDictType: {other}"))),
    }
}

/// `validateFontDescriptorPart2`: the metrics that relaxed validation tolerates as missing are
/// `Ascent`, `Descent` and `StemV`.
fn validate_font_descriptor_part2(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    font_dict_type: &str,
) -> Result<(), Error> {
    let not_type3 = font_dict_type != "Type3";
    for key in ["Ascent", "Descent"] {
        let _ = validate_number_entry(x, d, dict_name, key, not_type3, Version::V10, None);
    }
    for key in ["Leading", "CapHeight", "XHeight"] {
        validate_number_entry(x, d, dict_name, key, OPTIONAL, Version::V10, None)?;
    }
    let _ = validate_number_entry(x, d, dict_name, "StemV", not_type3, Version::V10, None);
    for key in ["StemH", "AvgWidth", "MaxWidth", "MissingWidth"] {
        validate_number_entry(x, d, dict_name, key, OPTIONAL, Version::V10, None)?;
    }
    validate_font_descriptor_font_file(x, d, dict_name, font_dict_type)?;
    validate_string_entry(x, d, dict_name, "CharSet", OPTIONAL, Version::V11, None)?;
    Ok(())
}

/// `validateFontDescriptor`.
fn validate_font_descriptor(
    x: &mut XRefTable,
    d: &Dict,
    font_dict_name: &str,
    font_dict_type: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d1) = validate_dict_entry(
        x,
        d,
        font_dict_name,
        "FontDescriptor",
        required,
        since,
        None,
    )?
    else {
        return Ok(());
    };

    let dict_name = "fdDict";

    // Process font descriptor dict
    validate_font_descriptor_part1(x, &d1, dict_name, font_dict_type)?;
    validate_font_descriptor_part2(x, &d1, dict_name, font_dict_type)?;

    if font_dict_type == "CIDFontType0" || font_dict_type == "CIDFontType2" {
        // Style, optional, dict (9.8.3.2)
        validate_dict_entry(
            x,
            &d1,
            dict_name,
            "Style",
            OPTIONAL,
            Version::V10,
            Some(&|d: &Dict| d.len() == 1 && d.find(b"Panose").is_some()),
        )?;

        // Lang, optional, name
        validate_name_entry(x, &d1, dict_name, "Lang", OPTIONAL, Version::V13, None)?;

        // FD, optional, dict
        validate_dict_entry(x, &d1, dict_name, "FD", OPTIONAL, Version::V10, None)?;

        // CIDSet, optional, stream
        validate_stream_dict_entry(x, &d1, dict_name, "CIDSet", OPTIONAL, Version::V10, None)?;
    }

    Ok(())
}

/// `isZapfDingbatsFont`.
fn is_zapf_dingbats_font(x: &mut XRefTable, d: &Dict) -> Result<bool, Error> {
    let (subtype, _) = x.dereference_name_entry(d, b"Subtype")?;
    if !matches!(subtype.as_deref(), Some(b"Type1")) {
        return Ok(false);
    }
    let (base_font, _) = x.dereference_name_entry(d, b"BaseFont")?;
    Ok(matches!(base_font.as_deref(), Some(b"ZapfDingbats")))
}

/// `validateFontEncoding`: a name (relaxed validation knows more of them than the spec) or an
/// encoding dictionary.
fn validate_font_encoding(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
) -> Result<(), Error> {
    let entry_name = "Encoding";
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, Version::V10)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}")))?
    else {
        return Ok(());
    };

    let zapf_dingbats = is_zapf_dingbats_font(x, d)
        .map_err(|e| e.context(format!("{dict_name}: classify ZapfDingbats font")))?;
    let valid_name = |n: &[u8]| {
        matches!(
            n,
            b"MacRomanEncoding"
                | b"MacExpertEncoding"
                | b"WinAnsiEncoding"
                | b"FontSpecific"
                | b"StandardEncoding"
                | b"SymbolEncoding"
                | b"SymbolSetEncoding"
                | b"PDFDocEncoding"
        ) || (zapf_dingbats && n == b"ZapfDingbatsEncoding")
    };

    match o {
        Object::Name(n) => {
            if !valid_name(&n) {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}: invalid encoding name {:?}",
                    lossy(&n)
                )));
            }
        }
        // no further processing
        Object::Dict(_) => {}
        other => {
            return Err(Error::Validation(format!(
                "{dict_name}.{entry_name}: expected name or encoding dictionary, got {}",
                crate::model::deref::type_name(&other)
            )));
        }
    }
    Ok(())
}

/// `simpleFontWidthCount`: the number of characters from `first` to `last`.
fn simple_font_width_count(first: i64, last: i64) -> Option<i64> {
    if last < first {
        return None;
    }
    let span = last.checked_sub(first)?;
    if span == i64::MAX {
        return None;
    }
    span.checked_add(1)
}

/// `relaxedSimpleFontWidthCount`: one width too few, or any number too many.
fn relaxed_simple_font_width_count(actual: i64, expected: i64) -> bool {
    actual > 0 && (actual == expected.saturating_sub(1) || actual > expected)
}

/// `validateSimpleFontWidths`: `LastChar` and `Widths`. Relaxed validation accepts the empty
/// range of `FirstChar` 65535 and `LastChar` 0, and a `Widths` array that is one short or longer
/// than the range.
fn validate_simple_font_widths(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    required: bool,
    first_char: Option<i64>,
) -> Result<(), Error> {
    let last_char =
        validate_integer_entry(x, d, dict_name, "LastChar", required, Version::V10, None)?;
    let widths =
        validate_number_array_entry(x, d, dict_name, "Widths", required, Version::V10, None)?;
    let (Some(first), Some(last), Some(widths)) = (first_char, last_char, widths) else {
        return Ok(());
    };
    let actual = i64::try_from(widths.len()).unwrap_or(i64::MAX);
    if first == 65535 && last == 0 && actual == 0 {
        return Ok(());
    }
    let Some(expected) = simple_font_width_count(first, last) else {
        return Err(Error::Validation(format!(
            "{dict_name}.LastChar: invalid value {last}, expected at least FirstChar {first}"
        )));
    };
    if actual == expected || relaxed_simple_font_width_count(actual, expected) {
        return Ok(());
    }
    Err(array_cardinality_error(
        dict_name,
        "Widths",
        widths.len(),
        &format!("{expected} to match FirstChar {first} and LastChar {last}"),
    ))
}

/// `validateTrueTypeFontDict` (9.6.3): the font name.
fn validate_true_type_font_dict(x: &mut XRefTable, d: &Dict) -> Result<Vec<u8>, Error> {
    let dict_name = "trueTypeFontDict";

    // Name, name, obsolet and should not be used.

    // BaseFont, required, name
    let bf = validate_name_entry(x, d, dict_name, "BaseFont", REQUIRED, Version::V10, None)?;
    let font_name = bf.unwrap_or_default();

    // FirstChar: not required in relaxed validation.
    let first_char =
        validate_integer_entry(x, d, dict_name, "FirstChar", OPTIONAL, Version::V10, None)?;

    validate_simple_font_widths(x, d, dict_name, OPTIONAL, first_char)?;

    // FontDescriptor, optional in relaxed validation
    validate_font_descriptor(x, d, dict_name, "TrueType", OPTIONAL, Version::V10)?;

    // Encoding, optional, name or dict
    validate_font_encoding(x, d, dict_name, OPTIONAL)?;

    // ToUnicode, optional, stream
    validate_stream_dict_entry(x, d, dict_name, "ToUnicode", OPTIONAL, Version::V12, None)?;

    Ok(font_name)
}

/// `validateCIDToGIDMap`: the name `Identity` or a stream.
fn validate_cid_to_gid_map(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    let o = x
        .dereference(o)
        .map_err(|e| e.context("CIDToGIDMap: dereference"))?;
    match o {
        Object::Null => Err(Error::Validation("CIDToGIDMap: missing object".to_string())),
        Object::Name(n) => {
            if n.as_slice() != b"Identity" {
                return Err(Error::Validation(format!(
                    "CIDToGIDMap: invalid name {:?}, must be \"Identity\"",
                    lossy(n)
                )));
            }
            Ok(())
        }
        // no further processing
        Object::Stream(_) => Ok(()),
        other => Err(Error::Validation(format!(
            "CIDToGIDMap: expected Identity name or stream dict, got {}",
            crate::model::deref::type_name(other)
        ))),
    }
}

/// `validateCIDFontGlyphWidths`: the `W` and `W2` arrays hold numbers and number arrays.
fn validate_cid_font_glyph_widths(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(x, d, dict_name, entry_name, required, since, None)
        .map_err(|e| e.context(format!("{dict_name}.{entry_name}")))?
    else {
        return Ok(());
    };

    for (i, raw) in a.iter().enumerate() {
        let o = x
            .dereference(raw)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]: dereference")))?
            .clone();
        match &o {
            Object::Null | Object::Integer(_) | Object::Float(_) => {}
            Object::Array(_) => {
                validate_number_array(x, &o)
                    .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]")))?;
            }
            other => {
                return Err(Error::Validation(format!(
                    "{dict_name}.{entry_name}[{i}]: expected integer, float or number array, got {}",
                    crate::model::deref::type_name(other)
                )));
            }
        }
    }

    Ok(())
}

/// `validateCIDSystemInfoDict`.
fn validate_cid_system_info_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "CIDSystemInfoDict";

    // Registry, required, ASCII string
    validate_string_entry(x, d, dict_name, "Registry", REQUIRED, Version::V10, None)?;

    // Ordering, required, ASCII string
    validate_string_entry(x, d, dict_name, "Ordering", REQUIRED, Version::V10, None)?;

    // Supplement, required, integer
    validate_integer_entry(x, d, dict_name, "Supplement", REQUIRED, Version::V10, None)?;
    Ok(())
}

/// `validateCIDFontDict` (9.7.4).
fn validate_cid_font_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "CIDFontDict";

    // Type, required, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Font"),
    )?;

    // Subtype, required, name
    let font_type = validate_name_entry(
        x,
        d,
        dict_name,
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"CIDFontType0" || s == b"CIDFontType2"),
    )?
    .unwrap_or_default();
    let font_type = lossy(&font_type);

    // BaseFont, required, name
    validate_name_entry(x, d, dict_name, "BaseFont", REQUIRED, Version::V10, None)?;

    // CIDSystemInfo, required, dict
    if let Some(d1) = validate_dict_entry(
        x,
        d,
        dict_name,
        "CIDSystemInfo",
        REQUIRED,
        Version::V10,
        None,
    )? {
        validate_cid_system_info_dict(x, &d1)?;
    }

    // FontDescriptor, required, dict
    validate_font_descriptor(x, d, dict_name, &font_type, REQUIRED, Version::V10)?;

    // DW, optional, integer
    validate_integer_entry(x, d, dict_name, "DW", OPTIONAL, Version::V10, None)?;

    // W, optional, array
    validate_cid_font_glyph_widths(x, d, dict_name, "W", OPTIONAL, Version::V10)?;

    // DW2, optional, array of two numbers: the default metrics for vertical writing.
    validate_number_array_entry(
        x,
        d,
        dict_name,
        "DW2",
        OPTIONAL,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?;

    // W2, optional, array
    validate_cid_font_glyph_widths(x, d, dict_name, "W2", OPTIONAL, Version::V10)?;

    // CIDToGIDMap, stream or (name /Identity)
    if let Some(o) = d.find(b"CIDToGIDMap") {
        let o = o.clone();
        validate_cid_to_gid_map(x, &o).map_err(|e| e.context("CIDFontDict.CIDToGIDMap"))?;
    }
    Ok(())
}

/// `validateDescendantFonts`: a one-element array holding a CID font dictionary.
fn validate_descendant_fonts(
    x: &mut XRefTable,
    d: &Dict,
    font_dict_name: &str,
    required: bool,
) -> Result<(), Error> {
    let Some(a) = validate_array_entry(
        x,
        d,
        font_dict_name,
        "DescendantFonts",
        required,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 1),
    )
    .map_err(|e| e.context(format!("{font_dict_name}.DescendantFonts")))?
    else {
        return Ok(());
    };
    let Some(first) = a.first() else {
        return Ok(());
    };

    let context = object_context(&format!("{font_dict_name}.DescendantFonts[0]"), Some(first));
    let d1 = match x.dereference_dict(first) {
        Ok(Some(d1)) => d1.clone(),
        Ok(None) => {
            if required {
                return Err(Error::Validation(format!(
                    "{context}: missing required descendant font dict"
                )));
            }
            return Ok(());
        }
        Err(e) => return Err(e.context(format!("{context}: dereference dict"))),
    };

    validate_cid_font_dict(x, &d1).map_err(|e| e.context(context))
}

/// `cMapTraversal`: the CMap chain of a Type 0 font (the CMaps that `UseCMap` refers to).
#[derive(Debug, Default)]
struct CMapTraversal {
    ancestors: BTreeSet<i64>,
}

impl CMapTraversal {
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr <= 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::CMapCycle).context(format!("obj#{obj_nr}")));
        }
        Ok(())
    }

    fn leave(&mut self, obj_nr: i64) {
        if obj_nr > 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `validateEntry`: the entry is a name or a CMap stream.
    fn validate_entry(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        required: bool,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        v.check()?;

        let raw = match d.find(entry_name.as_bytes()) {
            Some(o) if !o.is_null() => o.clone(),
            _ => {
                validate_entry(v.x, d, dict_name, entry_name, required, since)?;
                return Ok(());
            }
        };
        v.x.check_recursion_depth("CMap chain", depth)?;

        let c_map_obj_nr = match &raw {
            Object::Ref(r) => r.num,
            _ => 0,
        };
        self.enter(c_map_obj_nr)?;
        let r = self.validate_resolved(v, d, dict_name, entry_name, required, since, depth);
        self.leave(c_map_obj_nr);
        r
    }

    fn validate_resolved(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        required: bool,
        since: Version,
        depth: u64,
    ) -> Result<(), Error> {
        let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, since)? else {
            return Ok(());
        };
        match o {
            Object::Name(_) => Ok(()),
            Object::Stream(sd) => self.validate_stream_dict(v, &sd, depth),
            _ => Err(Error::Validation(format!(
                "dict={dict_name} corrupt entry \"{entry_name}\""
            ))),
        }
    }

    /// `validateStreamDict` (table 120).
    fn validate_stream_dict(
        &mut self,
        v: &mut Validator<'_>,
        sd: &StreamDict,
        depth: u64,
    ) -> Result<(), Error> {
        let dict_name = "CMapStreamDict";

        // Type, optional, name
        validate_name_entry(
            v.x,
            &sd.dict,
            dict_name,
            "Type",
            OPTIONAL,
            Version::V10,
            Some(&|s: &[u8]| s == b"CMap"),
        )?;

        // CMapName, required, name
        validate_name_entry(
            v.x,
            &sd.dict,
            dict_name,
            "CMapName",
            REQUIRED,
            Version::V10,
            None,
        )?;

        // CIDFontType0SystemInfo, required, dict
        if let Some(d) = validate_dict_entry(
            v.x,
            &sd.dict,
            dict_name,
            "CIDSystemInfo",
            REQUIRED,
            Version::V10,
            None,
        )? {
            validate_cid_system_info_dict(v.x, &d)?;
        }

        // WMode, optional, integer, 0 or 1
        validate_integer_entry(
            v.x,
            &sd.dict,
            dict_name,
            "WMode",
            OPTIONAL,
            Version::V10,
            Some(&|i: i64| i == 0 || i == 1),
        )?;

        // UseCMap, name or cmap stream dict, optional. If present, the referencing CMap
        // shall specify only the character mappings that differ from the referenced CMap.
        self.validate_entry(
            v,
            &sd.dict,
            dict_name,
            "UseCMap",
            OPTIONAL,
            Version::V10,
            depth.saturating_add(1),
        )
    }
}

/// `validateType0FontEncoding`.
fn validate_type0_font_encoding(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    required: bool,
) -> Result<(), Error> {
    CMapTraversal::default().validate_entry(v, d, dict_name, "Encoding", required, Version::V10, 0)
}

/// `validateType0FontDict`: the font name. Relaxed validation turns any failure of the
/// descendant fonts into a missing font.
fn validate_type0_font_dict(v: &mut Validator<'_>, d: &Dict) -> FontResult<Vec<u8>> {
    let dict_name = "type0FontDict";

    // BaseFont, required, name
    let bf = validate_name_entry(v.x, d, dict_name, "BaseFont", REQUIRED, Version::V10, None)?;
    let font_name = bf.unwrap_or_default();

    // Encoding, required, name or CMap stream dict
    validate_type0_font_encoding(v, d, dict_name, REQUIRED)?;

    // DescendantFonts: one-element array specifying the CIDFont dictionary that is the
    // descendant of this Type 0 font, required.
    if validate_descendant_fonts(v.x, d, dict_name, REQUIRED).is_err() {
        return Err(FontErr {
            name: font_name,
            err: missing_font(),
        });
    }

    // ToUnicode, optional, CMap stream dict, or the name Identity-H
    if validate_stream_dict_entry(v.x, d, dict_name, "ToUnicode", OPTIONAL, Version::V11, None)
        .is_err()
    {
        validate_name_entry(
            v.x,
            d,
            dict_name,
            "ToUnicode",
            REQUIRED,
            Version::V11,
            Some(&|s: &[u8]| s == b"Identity-H"),
        )
        .map_err(|err| FontErr {
            name: font_name.clone(),
            err,
        })?;
    }

    Ok(font_name)
}

/// `missingType1FontMetrics` and `type1FontMetricsRequired`: relaxed validation never requires
/// the metrics, but the entries are still dereferenced.
fn check_type1_font_metrics(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    for key in ["FirstChar", "LastChar", "Widths", "FontDescriptor"] {
        let o = d.0.get(key.as_bytes()).cloned().unwrap_or(Object::Null);
        x.dereference(&o)?;
    }
    Ok(())
}

/// `validateType1FontDict` (9.6.2): the font name.
fn validate_type1_font_dict(x: &mut XRefTable, d: &Dict) -> Result<Vec<u8>, Error> {
    let dict_name = "type1FontDict";

    // Name, name, obsolet and should not be used.

    // BaseFont, required, name
    let bf = validate_name_entry(x, d, dict_name, "BaseFont", REQUIRED, Version::V10, None)?;
    let font_name = bf.unwrap_or_default();

    check_type1_font_metrics(x, d)?;
    let required = false;

    // FirstChar, required except for standard 14 fonts before PDF 2.0, integer
    let fc = validate_integer_entry(x, d, dict_name, "FirstChar", required, Version::V10, None)?;

    validate_simple_font_widths(x, d, dict_name, required, fc)?;

    // FontDescriptor, required since version 2.0; required unless standard font for
    // version <= 1.7, dict
    validate_font_descriptor(x, d, dict_name, "Type1", required, Version::V10)?;

    // Encoding, optional, name or dict
    validate_font_encoding(x, d, dict_name, OPTIONAL)?;

    // ToUnicode, optional, stream
    validate_stream_dict_entry(x, d, dict_name, "ToUnicode", OPTIONAL, Version::V10, None)?;

    Ok(font_name)
}

/// `validateCharProcsDict`: not required in relaxed validation. Go returns at once if the entry
/// does not validate (the dictionary it returns is nil then), so a failure is no error.
fn validate_char_procs_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let Ok(Some(d1)) =
        validate_dict_entry(x, d, "type3FontDict", "CharProcs", OPTIONAL, since, None)
    else {
        return Ok(());
    };

    for (key, value) in d1.iter() {
        x.dereference_stream_dict(value)
            .map_err(|e| e.context(format!("CharProcs entry {}", lossy(key))))?;
    }

    Ok(())
}

/// `validateType3FontDict` (9.6.5).
fn validate_type3_font_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "type3FontDict";

    // Name, name, obsolet and should not be used.

    // FontBBox, required, rectangle
    validate_rectangle_entry(v.x, d, dict_name, "FontBBox", REQUIRED, Version::V10, None)?;

    // FontMatrix, required, number array
    validate_number_array_entry(
        v.x,
        d,
        dict_name,
        "FontMatrix",
        REQUIRED,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 6),
    )?;

    // CharProcs, required, dict
    validate_char_procs_dict(v.x, d, Version::V10)?;

    // Encoding, required, name or dict
    validate_font_encoding(v.x, d, dict_name, REQUIRED)?;

    // FirstChar, required, integer
    let first_char =
        validate_integer_entry(v.x, d, dict_name, "FirstChar", REQUIRED, Version::V10, None)?;

    validate_simple_font_widths(v.x, d, dict_name, REQUIRED, first_char)?;

    // FontDescriptor, required since version 1.5 for tagged PDF documents, dict
    let tagged = v.x.tagged;
    validate_font_descriptor(v.x, d, dict_name, "Type3", tagged, Version::V13)?;

    // Resources, optional, dict, since V1.2
    let raw_resources = d.find(b"Resources").cloned();
    let d1 = validate_dict_entry(v.x, d, dict_name, "Resources", OPTIONAL, Version::V11, None)?;
    if d1.is_some() {
        validate_resource_dict(v, &raw_resources.unwrap_or(Object::Null))?;
    }

    // ToUnicode, optional, stream
    validate_stream_dict_entry(v.x, d, dict_name, "ToUnicode", OPTIONAL, Version::V12, None)?;
    Ok(())
}

/// `_validateFontDict`, after the repairs: the font name. `ind_ref` is the reference of the font
/// dictionary, `None` for a direct one.
fn validate_font_dict_inner(
    v: &mut Validator<'_>,
    d: &Dict,
    ind_ref: Option<Ref>,
) -> FontResult<Vec<u8>> {
    let (subtype, _) =
        v.x.dereference_name_entry(d, b"Subtype")
            .map_err(|e| e.context("font dict Subtype"))?;
    let Some(subtype) = subtype else {
        let msg = if ind_ref.is_some() {
            "font: missing Subtype"
        } else {
            "font dict: missing Subtype"
        };
        return Err(Error::Validation(msg.to_string()).into());
    };

    let r = match subtype.as_slice() {
        b"TrueType" => validate_true_type_font_dict(v.x, d).map_err(FontErr::from),
        b"Type0" => validate_type0_font_dict(v, d),
        b"Type1" | b"Type1C" => validate_type1_font_dict(v.x, d).map_err(FontErr::from),
        b"MMType1" => return validate_type1_font_dict(v.x, d).map_err(FontErr::from),
        b"Type3" => validate_type3_font_dict(v, d)
            .map(|()| Vec::new())
            .map_err(FontErr::from),
        _ => {
            return Err(Error::Validation(format!(
                "font dict: unknown Subtype {:?}",
                lossy(&subtype)
            ))
            .into());
        }
    };

    if let Some(r) = ind_ref {
        v.x.set_valid(r)
            .map_err(|e| e.context("font: mark valid"))?;
    }

    match r {
        Ok(name) => Ok(name),
        Err(FontErr { name, err }) => Err(FontErr {
            name,
            err: err.context(format!("font dict Subtype {}", lossy(&subtype))),
        }),
    }
}

/// `checkFontIndRefValidationState`: true if the font is valid or being validated already.
fn check_font_ind_ref_validation_state(x: &mut XRefTable, r: Ref) -> Result<bool, Error> {
    match x.is_valid(r) {
        Err(e) => {
            return Err(missing_font().context(format!("font: check valid: {e}")));
        }
        Ok(true) => return Ok(true),
        Ok(false) => {}
    }

    if x.is_being_validated(r)
        .map_err(|e| e.context("font: check being validated"))?
    {
        return Ok(true);
    }

    x.set_being_validated(r)
        .map_err(|e| e.context("font: mark being validated"))?;
    Ok(false)
}

/// `dereferenceFontDict`: a copy of the font dictionary `r` refers to. A font that does not
/// dereference is a missing font in relaxed validation.
fn dereference_font_dict(x: &mut XRefTable, r: Ref) -> Result<Dict, Error> {
    match x.dereference_dict_ref(r) {
        Err(e) => Err(missing_font().context(format!("font: dereference dict: {e}"))),
        Ok(None) => Err(missing_font().context("font: missing dict")),
        Ok(Some(d)) => Ok(d.clone()),
    }
}

/// `validateFontDict` for the font `r`.
fn validate_font_dict(v: &mut Validator<'_>, r: Ref) -> FontResult<Vec<u8>> {
    if check_font_ind_ref_validation_state(v.x, r)? {
        return Ok(Vec::new());
    }

    let mut d = dereference_font_dict(v.x, r)?;

    if d.is_empty() {
        return Ok(Vec::new());
    }
    if repair_string_type1_font_dict(&mut d) {
        store_dict(v.x, r, d.clone());
    }

    // Relaxed validation does not check the Type.
    v.x.dereference_name_entry(&d, b"Type")
        .map_err(|e| e.context("font Type"))?;

    if repair_self_referential_font_to_unicode(&mut d, Some(r)) {
        store_dict(v.x, r, d.clone());
    }

    validate_font_dict_inner(v, &d, Some(r))
}

/// `validateFontObject`: the font name, and the reference if the font is indirect. A direct font
/// dictionary is changed in place.
fn validate_font_object(
    v: &mut Validator<'_>,
    obj: &mut Object,
) -> FontResult<(Vec<u8>, Option<Ref>)> {
    if let Object::Ref(r) = obj {
        let r = *r;
        let name = validate_font_dict(v, r)?;
        return Ok((name, Some(r)));
    }

    let d =
        v.x.dereference_dict(obj)
            .map_err(|e| e.context("font resource: dereference direct font dict"))?
            .cloned();
    let Some(mut d) = d else {
        return Err(missing_font().into());
    };

    repair_string_type1_font_dict(&mut d);
    let r = validate_font_dict_inner(v, &d, None);
    *obj = Object::Dict(d);
    Ok((r?, None))
}

/// `fixFontObjNr`: a skipped font resource refers to the font of the same name that validated, or
/// to nothing.
fn fix_font_obj_nr(m1: &BTreeMap<Vec<u8>, Vec<u8>>, m2: &BTreeMap<Vec<u8>, Ref>, d: &mut Dict) {
    for (k, v) in m1 {
        if !v.is_empty()
            && let Some(r) = m2.get(v)
        {
            d.set(k, Object::Ref(*r));
            continue;
        }
        d.set(k, Object::Null);
    }
}

/// `isEncodingDict`.
fn is_encoding_dict(x: &mut XRefTable, o: &Object) -> bool {
    let Ok(Some(d)) = x.dereference_dict(o).map(|d| d.cloned()) else {
        return false;
    };
    matches!(
        x.dereference_name_entry(&d, b"Type"),
        Ok((Some(t), _)) if t == b"Encoding"
    )
}

/// `isMisplacedEncodingResourceDict`: an `Encoding` entry that holds only encoding dictionaries.
fn is_misplaced_encoding_resource_dict(x: &mut XRefTable, id: &[u8], o: &Object) -> bool {
    if id != b"Encoding" {
        return false;
    }
    let Ok(Some(d)) = x.dereference_dict(o).map(|d| d.cloned()) else {
        return false;
    };
    !d.is_empty() && d.0.values().all(|o| is_encoding_dict(x, o))
}

/// `validateFontResources`: the font resource dictionary `d` is changed in place.
fn validate_font_resources(v: &mut Validator<'_>, d: &mut Dict) -> Result<(), Error> {
    // fontid, fontname
    let mut m1: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    // fontname, ind ref
    let mut m2: BTreeMap<Vec<u8>, Ref> = BTreeMap::new();

    // Iterate over font resource dict
    let ids: Vec<Vec<u8>> = d.0.keys().cloned().collect();
    for id in ids {
        v.check()?;
        let Some(mut obj) = d.0.get(&id).cloned() else {
            continue;
        };
        if is_misplaced_encoding_resource_dict(v.x, &id, &obj) {
            d.delete(&id);
            continue;
        }

        // Process fontDict
        match validate_font_object(v, &mut obj) {
            Err(FontErr { name, err }) => {
                // Relaxed: a missing font is skipped.
                if err.is(Sentinel::MissingFont) {
                    m1.insert(id, name);
                    continue;
                }
                return Err(err.context(format!("Font resource {}", lossy(&id))));
            }
            Ok((fn_, ind_ref)) => {
                if matches!(obj, Object::Dict(_)) {
                    d.set(&id, obj);
                }
                if let Some(r) = ind_ref {
                    m2.insert(fn_, r);
                }
            }
        }
    }

    if !m1.is_empty() {
        fix_font_obj_nr(&m1, &m2, d);
    }

    Ok(())
}

/// `validateFontResourceDict`: the `Font` entry `o` of a resource dictionary. A font resource
/// dictionary that is a reference is stored back, and the other changes of a direct one are lost:
/// see [`validate_font_resource_dict_repaired`].
///
/// # Errors
/// As [`validate_font_resource_dict_repaired`].
pub fn validate_font_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    validate_font_resource_dict_repaired(v, o, since).map(|_| ())
}

/// [`validate_font_resource_dict`], and the repaired copy of the dictionary if `o` is a direct
/// dictionary (Go changes it in place): a font that is skipped is removed from it or replaced.
///
/// # Errors
/// [`Error::Validation`] for a missing dictionary, a version that is too low or an invalid font,
/// [`Error::Cancelled`] and the errors of the lookups.
pub fn validate_font_resource_dict_repaired(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<Option<Dict>, Error> {
    v.x.validate_version("fontResourceDict", since)
        .map_err(|e| e.context("Font resource dict: version"))?;

    let r = match o {
        Object::Ref(r) => Some(*r),
        _ => None,
    };
    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("Font resource dict: dereference dict"))?
            .cloned();
    let Some(mut d) = d else {
        return Err(Error::Validation(
            "Font resource dict: missing dict".to_string(),
        ));
    };
    let result = validate_font_resources(v, &mut d);
    match r {
        Some(r) => {
            store_dict(v.x, r, d);
            result.map(|()| None)
        }
        None => result.map(|()| Some(d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    /// `type1FontTestObjects`: a table with a font descriptor as object 1, and a Type 1 font
    /// dictionary that refers to it.
    fn type1_font_test_objects(version: Version, font_name: &str) -> (XRefTable, Dict) {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(version);
        let fd = dict(&[
            ("Type", name("FontDescriptor")),
            ("FontName", name(font_name)),
            ("Flags", Object::Integer(33)),
            (
                "FontBBox",
                Object::Array(vec![
                    Object::Integer(-23),
                    Object::Integer(-250),
                    Object::Integer(715),
                    Object::Integer(805),
                ]),
            ),
            ("ItalicAngle", Object::Integer(0)),
            ("Ascent", Object::Integer(629)),
            ("Descent", Object::Integer(-157)),
            ("CapHeight", Object::Integer(562)),
            ("StemV", Object::Integer(51)),
        ]);
        x.table.insert(1, Entry::gen0(Object::Dict(fd)));
        let d = dict(&[
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name(font_name)),
            ("FirstChar", Object::Integer(65)),
            ("LastChar", Object::Integer(65)),
            ("Widths", Object::Array(vec![Object::Integer(600)])),
            (
                "FontDescriptor",
                Object::Ref(Ref {
                    num: 1,
                    generation: 0,
                }),
            ),
        ]);
        (x, d)
    }

    fn check_type1_font_validation(x: &mut XRefTable, d: &Dict) {
        let font_name = validate_type1_font_dict(x, d).unwrap();
        assert_eq!(Some(font_name.as_slice()), d.name_entry(b"BaseFont"));
    }

    const METRICS: [&str; 4] = ["FirstChar", "LastChar", "Widths", "FontDescriptor"];

    /// `TestType1FontMetricsVersions` in relaxed validation: every combination of the metrics is
    /// accepted across the PDF 2.0 boundary.
    #[test]
    fn type1_font_metrics_versions() {
        for version in [Version::V17, Version::V20] {
            for font_name in ["Courier", "CustomFont"] {
                for mask in 0..16 {
                    let (mut x, mut d) = type1_font_test_objects(version, font_name);
                    for (i, key) in METRICS.iter().enumerate() {
                        if mask & (1 << i) == 0 {
                            d.0.remove(key.as_bytes());
                        }
                    }
                    check_type1_font_validation(&mut x, &d);
                }
            }
        }
    }

    /// `TestType1FontMetricsRelaxed` and the relaxed cases of
    /// `TestType1FontMetricsRelaxedDiagnostics`.
    #[test]
    fn type1_font_metrics_relaxed() {
        for version in [Version::V17, Version::V20] {
            let (mut x, mut d) = type1_font_test_objects(version, "Courier");
            for key in METRICS {
                d.0.remove(key.as_bytes());
            }
            check_type1_font_validation(&mut x, &d);
        }
        let cases: [(Version, &str, &[&str]); 5] = [
            (Version::V17, "Courier", &METRICS),
            (Version::V17, "Courier", &["FirstChar"]),
            (Version::V20, "Courier", &METRICS),
            (Version::V17, "CustomFont", &["Widths"]),
            (Version::V20, "Courier", &[]),
        ];
        for (version, font_name, remove) in cases {
            let (mut x, mut d) = type1_font_test_objects(version, font_name);
            for key in remove {
                d.0.remove(key.as_bytes());
            }
            check_type1_font_validation(&mut x, &d);
        }
    }

    /// `TestType1FontMetricsNull`: direct and indirect null entries count as absent.
    #[test]
    fn type1_font_metrics_null() {
        for indirect in [false, true] {
            let (mut x, mut d) = type1_font_test_objects(Version::V17, "Courier");
            x.table.insert(2, Entry::gen0(Object::Null));
            let null = if indirect {
                Object::Ref(Ref {
                    num: 2,
                    generation: 0,
                })
            } else {
                Object::Null
            };
            for key in METRICS {
                d.set(key.as_bytes(), null.clone());
            }
            check_type1_font_validation(&mut x, &d);
            d.set(b"LastChar", Object::Integer(65));
            check_type1_font_validation(&mut x, &d);
        }
    }

    #[test]
    fn simple_font_widths() {
        assert_eq!(simple_font_width_count(65, 65), Some(1));
        assert_eq!(simple_font_width_count(66, 65), None);
        assert_eq!(simple_font_width_count(-1, i64::MAX), None);
        assert_eq!(simple_font_width_count(0, i64::MAX), None);
        assert!(relaxed_simple_font_width_count(2, 3));
        assert!(relaxed_simple_font_width_count(4, 3));
        assert!(!relaxed_simple_font_width_count(0, 1));
        assert!(!relaxed_simple_font_width_count(1, 3));

        let (mut x, mut d) = type1_font_test_objects(Version::V17, "Courier");
        d.set(
            b"Widths",
            Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
        );
        // One width too many is accepted in relaxed validation.
        check_type1_font_validation(&mut x, &d);
        d.set(b"LastChar", Object::Integer(64));
        assert!(validate_type1_font_dict(&mut x, &d).is_err());
        d.set(b"LastChar", Object::Integer(67));
        d.set(b"Widths", Object::Array(Vec::new()));
        assert!(validate_type1_font_dict(&mut x, &d).is_err());
        // The empty range that some producers write.
        d.set(b"FirstChar", Object::Integer(65535));
        d.set(b"LastChar", Object::Integer(0));
        check_type1_font_validation(&mut x, &d);
    }

    #[test]
    fn string_entries_of_a_standard_type1_font_become_names() {
        let mut d = dict(&[
            ("Type", Object::string("Font")),
            ("Subtype", Object::string("Type1")),
            ("BaseFont", Object::string("Helvetica")),
        ]);
        assert!(repair_string_type1_font_dict(&mut d));
        assert_eq!(d.name_entry(b"BaseFont"), Some(&b"Helvetica"[..]));
        assert!(!repair_string_type1_font_dict(&mut d));

        let mut other = dict(&[
            ("Type", Object::string("Font")),
            ("Subtype", Object::string("Type1")),
            ("BaseFont", Object::string("Custom")),
        ]);
        assert!(!repair_string_type1_font_dict(&mut other));
    }

    #[test]
    fn a_self_referential_to_unicode_is_removed() {
        let r = Ref {
            num: 7,
            generation: 0,
        };
        let mut d = dict(&[("ToUnicode", Object::Ref(r))]);
        assert!(!repair_self_referential_font_to_unicode(&mut d, None));
        assert!(repair_self_referential_font_to_unicode(&mut d, Some(r)));
        assert!(d.find(b"ToUnicode").is_none());
    }

    #[test]
    fn a_missing_font_is_skipped_and_mapped() {
        let cancel = AtomicBool::new(false);
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        // Object 1 is a valid Type 1 font, object 2 does not exist.
        let font = dict(&[
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name("Courier")),
        ]);
        x.table.insert(1, Entry::gen0(Object::Dict(font)));
        let r1 = Ref {
            num: 1,
            generation: 0,
        };
        let r2 = Ref {
            num: 2,
            generation: 0,
        };
        let res = Object::Dict(dict(&[
            ("F1", Object::Ref(r1)),
            ("F2", Object::Ref(r2)),
            ("F3", Object::Null),
        ]));
        let mut v = Validator::new(&mut x, &cancel);
        let d = validate_font_resource_dict_repaired(&mut v, &res, Version::V10)
            .unwrap()
            .unwrap();
        assert!(matches!(d.find(b"F1"), Some(Object::Ref(r)) if *r == r1));
        assert!(matches!(d.find(b"F2"), Some(Object::Null)));
        assert!(matches!(d.find(b"F3"), Some(Object::Null)));
        assert!(v.x.table.get(&1).is_some_and(|e| e.valid));
    }

    #[test]
    fn a_misplaced_encoding_dict_is_removed() {
        let cancel = AtomicBool::new(false);
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        let enc = dict(&[("Type", name("Encoding"))]);
        let res = Object::Dict(dict(&[(
            "Encoding",
            Object::Dict(dict(&[("E", Object::Dict(enc))])),
        )]));
        let mut v = Validator::new(&mut x, &cancel);
        let d = validate_font_resource_dict_repaired(&mut v, &res, Version::V10)
            .unwrap()
            .unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn a_cmap_chain_that_loops_is_rejected() {
        let cancel = AtomicBool::new(false);
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        let r = Ref {
            num: 1,
            generation: 0,
        };
        // A CMap stream whose UseCMap is itself.
        let sd = StreamDict::new(
            dict(&[
                ("Type", name("CMap")),
                ("CMapName", name("X")),
                (
                    "CIDSystemInfo",
                    Object::Dict(dict(&[
                        ("Registry", Object::string("Adobe")),
                        ("Ordering", Object::string("Identity")),
                        ("Supplement", Object::Integer(0)),
                    ])),
                ),
                ("UseCMap", Object::Ref(r)),
            ]),
            0,
            None,
            None,
            Vec::new(),
        );
        x.table.insert(1, Entry::gen0(Object::Stream(Box::new(sd))));
        let font = dict(&[("Encoding", Object::Ref(r))]);
        let mut v = Validator::new(&mut x, &cancel);
        let e = validate_type0_font_encoding(&mut v, &font, "type0FontDict", REQUIRED).unwrap_err();
        assert!(e.is(Sentinel::CMapCycle));
    }

    /// A font file's `Metadata` entry that refers to nothing is deleted from the stored stream,
    /// as Go deletes it through the map of the stored object.
    #[test]
    fn a_dangling_font_file_metadata_is_deleted_from_the_stored_stream() {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        let missing = Object::Ref(Ref {
            num: 7,
            generation: 0,
        });
        let sd = StreamDict::new(
            dict(&[("Length1", Object::Integer(0)), ("Metadata", missing)]),
            0,
            None,
            None,
            Vec::new(),
        );
        x.table.insert(1, Entry::gen0(Object::Stream(Box::new(sd))));
        let r = Ref {
            num: 1,
            generation: 0,
        };
        let fd = dict(&[("FontFile2", Object::Ref(r))]);
        validate_font_file(
            &mut x,
            &fd,
            "fontDescriptor",
            "FontFile2",
            "TrueType",
            OPTIONAL,
            Version::V12,
        )
        .unwrap();
        let Some(Entry {
            object: Some(EntryObject::Obj(Object::Stream(stored))),
            ..
        }) = x.table.get(&1)
        else {
            panic!("object 1 is not a stream");
        };
        assert!(stored.dict.find(b"Metadata").is_none());
    }
}
