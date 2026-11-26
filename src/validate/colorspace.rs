//! Colour spaces (Go: validate/colorspace.go).
//!
//! Differences from Go, besides the ones in [`super::object`]:
//!
//! * [`ColorSpaceTraversal`] holds only the set of reference numbers on the path being validated
//!   (`ancestors`); the context and the table are passed to each method as the [`Validator`].
//!   `depth` is a `u64`, as in `XRefTable::check_recursion_depth`.
//! * Relaxed validation only: a value that is no name or array is skipped
//!   (`ShowSkipped` is not ported), a wrong `CalGray` value is accepted (the elements of the
//!   arrays are dereferenced by the entry validators already), and a wrong length of the lookup
//!   table of an `Indexed` space is accepted unless the pass was cancelled.
//! * `validateColorSpaceResourceDict` deletes a missing colour space from the resource
//!   dictionary. A resource dictionary that is a reference is changed in the table. For a direct
//!   one the function returns the repaired copy, which the caller stores.
//! * `ErrColorSpaceCycle` is [`Sentinel::ColorSpaceCycle`]; `errMissingColorSpaceObject` is an
//!   [`Error::Validation`] with a fixed message that [`is_missing_color_space`] recognises.
//! * The `ICCBased` stream gets its repair of a missing `Metadata` entry stored back to the
//!   table (see [`super::xobject::persist_stream_entries`]).

use super::Validator;
use super::function::validate_function;
use super::metadata::validate_metadata;
use super::object::{
    OPTIONAL, REQUIRED, object_context, validate_dict_entry, validate_entry,
    validate_float_for_object, validate_integer_entry, validate_integer_for_object,
    validate_name_array, validate_name_array_entry, validate_name_entry, validate_name_for_object,
    validate_number_array_entry, validate_number_entry, validate_stream_dict_for_object,
};
use super::xobject::{color_space_components, persist_stream_entries};
use crate::error::{Error, Sentinel};
use crate::filter::{DCT, JBIG2, JPX};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::string::{hex_decode, unescape};
use crate::object::{Dict, Object, StreamDict};
use std::collections::BTreeSet;

const MISSING_COLOR_SPACE: &str = "color space: missing object";

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `errMissingColorSpaceObject`.
fn missing_color_space() -> Error {
    Error::Validation(MISSING_COLOR_SPACE.to_string())
}

/// `errors.Is(err, errMissingColorSpaceObject)`.
pub fn is_missing_color_space(e: &Error) -> bool {
    matches!(e.root(), Error::Validation(s) if s == MISSING_COLOR_SPACE)
}

/// `colorSpaceRole`: what a colour space is used for, which decides the names it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpaceRole {
    /// `colorSpaceAny`.
    Any,
    /// `colorSpaceNoPattern`.
    NoPattern,
    /// `colorSpaceIndexedBase`.
    IndexedBase,
    /// `colorSpaceDeviceOrCIE`.
    DeviceOrCie,
    /// `colorSpaceSeparation`.
    Separation,
}

impl ColorSpaceRole {
    /// `colorSpaceRole.allows`.
    pub fn allows(self, name: &[u8]) -> bool {
        match self {
            ColorSpaceRole::Any => true,
            ColorSpaceRole::NoPattern => name != b"Pattern",
            ColorSpaceRole::IndexedBase => name != b"Pattern" && name != b"Indexed",
            ColorSpaceRole::DeviceOrCie => [
                &b"DeviceGray"[..],
                b"DeviceRGB",
                b"DeviceCMYK",
                b"CalGray",
                b"CalRGB",
                b"Lab",
                b"ICCBased",
            ]
            .contains(&name),
            ColorSpaceRole::Separation => name == b"Separation",
        }
    }

    /// `colorSpaceRole.description`.
    pub fn description(self) -> &'static str {
        match self {
            ColorSpaceRole::NoPattern => "non-Pattern color space",
            ColorSpaceRole::IndexedBase => "Indexed base color space",
            ColorSpaceRole::DeviceOrCie => "device or CIE-based color space",
            ColorSpaceRole::Separation => "Separation color space",
            ColorSpaceRole::Any => "color space",
        }
    }
}

fn is_device_name(s: &[u8]) -> bool {
    s == b"DeviceGray" || s == b"DeviceRGB" || s == b"DeviceCMYK"
}

/// `validateAllColorSpaceNamesExceptPattern`.
fn is_name_except_pattern(s: &[u8]) -> bool {
    is_device_name(s)
        || [
            &b"CalGray"[..],
            b"CalRGB",
            b"Lab",
            b"ICCBased",
            b"Indexed",
            b"Separation",
            b"DeviceN",
        ]
        .contains(&s)
}

/// `colorSpaceArrayName`: the name in the first element of the colour space array `a`.
///
/// # Errors
/// [`Error::Validation`] for an empty array or a first element that is no name, and the errors
/// of `XRefTable::dereference`.
pub fn color_space_array_name(x: &mut XRefTable, a: &[Object]) -> Result<Vec<u8>, Error> {
    let Some(first) = a.first() else {
        return Err(Error::Validation("color space array: empty".to_string()));
    };
    match x
        .dereference(first)
        .map_err(|e| e.context("color space array[0]"))?
    {
        Object::Name(n) => Ok(n.clone()),
        other => Err(Error::Validation(format!(
            "color space array[0]: expected name, got {}",
            type_name(other)
        ))),
    }
}

/// The dictionary of the parameters of a CIE-based colour space (`a[1]`).
fn cie_dict(x: &mut XRefTable, a: &[Object], dict_name: &str, label: &str) -> Result<Dict, Error> {
    x.validate_version(dict_name, Version::V11)?;
    if a.len() != 2 {
        return Err(Error::Validation(format!(
            "{label} color space: invalid array length {}, expected 2",
            a.len()
        )));
    }
    let d = x
        .dereference_dict(&a[1])
        .map_err(|e| e.context(format!("{label} color space parameters: dereference dict")))?
        .cloned();
    d.ok_or_else(|| Error::Validation(format!("{label} color space parameters: missing dict")))
}

/// `validateCalGrayColorSpace` (8.6.5.2, Table 62). Relaxed validation takes values that are out
/// of range (`validateCalGrayValues` only adds a notice), so the checks of the entries are all
/// that is left.
fn validate_cal_gray_color_space(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    let dict_name = "calGrayCSDict";
    let since = Version::V11;
    let d = cie_dict(x, a, dict_name, "CalGray")?;
    validate_number_array_entry(
        x,
        &d,
        dict_name,
        "WhitePoint",
        REQUIRED,
        since,
        Some(&|a: &[Object]| a.len() == 3),
    )?;
    validate_number_array_entry(
        x,
        &d,
        dict_name,
        "BlackPoint",
        OPTIONAL,
        since,
        Some(&|a: &[Object]| a.len() == 3),
    )?;
    validate_number_entry(x, &d, dict_name, "Gamma", OPTIONAL, since, None)?;
    Ok(())
}

/// `validateCalRGBColorSpace`.
fn validate_cal_rgb_color_space(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    let dict_name = "calRGBCSDict";
    let since = Version::V11;
    let d = cie_dict(x, a, dict_name, "CalRGB")?;
    for (entry, required, len) in [
        ("WhitePoint", REQUIRED, 3),
        ("BlackPoint", OPTIONAL, 3),
        ("Gamma", OPTIONAL, 3),
        ("Matrix", OPTIONAL, 9),
    ] {
        validate_number_array_entry(
            x,
            &d,
            dict_name,
            entry,
            required,
            since,
            Some(&|a: &[Object]| a.len() == len),
        )?;
    }
    Ok(())
}

/// `validateLabColorSpace`.
fn validate_lab_color_space(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    let dict_name = "labCSDict";
    let since = Version::V11;
    let d = cie_dict(x, a, dict_name, "Lab")?;
    for (entry, required, len) in [
        ("WhitePoint", REQUIRED, 3),
        ("BlackPoint", OPTIONAL, 3),
        ("Range", OPTIONAL, 4),
    ] {
        validate_number_array_entry(
            x,
            &d,
            dict_name,
            entry,
            required,
            since,
            Some(&|a: &[Object]| a.len() == len),
        )?;
    }
    Ok(())
}

/// `validateIndexedColorSpaceLookuptable`: a string, or a stream since `since`. A lookup that
/// refers to nothing is accepted.
fn validate_indexed_color_space_lookup_table(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    let target = x
        .dereference(o)
        .map_err(|e| e.context("Indexed color space lookup table: dereference"))?
        .clone();
    match target {
        Object::Null => Ok(()),
        Object::String(_) | Object::Hex(_) => {
            x.validate_version("IndexedColorSpaceLookuptable", Version::V12)
        }
        Object::Stream(_) => x.validate_version("IndexedColorSpaceLookuptable", since),
        other => Err(Error::Validation(format!(
            "Indexed color space lookup table: expected string, hex literal or stream dict, got {}",
            type_name(&other)
        ))),
    }
}

/// `indexedLookupStreamBytes`, as the length of the decoded bytes. The decode is charged to the
/// budget of the table, as in Go.
fn indexed_lookup_stream_len(v: &mut Validator<'_>, mut sd: StreamDict) -> Result<usize, Error> {
    for f in &sd.filter_pipeline {
        // These image filters may be preserved encoded by `decode_with_limit`.
        if f.name == DCT || f.name == JPX || f.name == JBIG2 {
            let name = lossy(&f.name);
            return Err(Error::UnsupportedFilter(name.clone())
                .context(format!("cannot check decoded lookup length for {name}")));
        }
    }
    v.x.decode_stream(&mut sd, v.cancel)
        .map_err(|e| e.context("decode lookup"))?;
    Ok(sd.content.as_ref().map_or(0, |c| c.len()))
}

/// `indexedLookupBytes`, as the length of the bytes.
fn indexed_lookup_len(v: &mut Validator<'_>, o: &Object) -> Result<usize, Error> {
    let target = v.x.dereference(o)?.clone();
    match target {
        Object::String(s) => Ok(unescape(&s).len()),
        Object::Hex(h) => Ok(hex_decode(&h)?.len()),
        Object::Stream(sd) => indexed_lookup_stream_len(v, *sd),
        _ => Err(Error::Validation("missing lookup data".to_string())),
    }
}

/// The checks of `validateIndexedLookupLength` that can fail.
fn check_indexed_lookup_length(
    v: &mut Validator<'_>,
    a: &[Object],
    hival: i64,
) -> Result<(), Error> {
    v.check()?;
    let n = color_space_components(v.x, &a[1])?;
    if n == 0 {
        return Err(Error::Validation(
            "cannot determine base colour-space component count".to_string(),
        ));
    }
    let len = indexed_lookup_len(v, &a[3]);
    v.check()?;
    let len = len?;
    let want = n.saturating_mul(usize::try_from(hival).unwrap_or(0).saturating_add(1));
    if len != want {
        return Err(Error::Validation(format!(
            "invalid length {len}, expected {want}"
        )));
    }
    Ok(())
}

/// `validateIndexedLookupLength` (8.6.6.3): relaxed validation accepts a lookup of the wrong
/// length, or one it cannot decode, unless the pass was cancelled.
fn validate_indexed_lookup_length(
    v: &mut Validator<'_>,
    a: &[Object],
    hival: i64,
) -> Result<(), Error> {
    match check_indexed_lookup_length(v, a, hival) {
        Err(_) if v.check().is_ok() => Ok(()),
        r => r,
    }
}

/// The colour spaces on the path being validated (Go: `colorSpaceTraversal`).
#[derive(Debug, Clone, Default)]
pub struct ColorSpaceTraversal {
    ancestors: BTreeSet<i64>,
}

impl ColorSpaceTraversal {
    /// `colorSpaceTraversal.enter`: the object number of `o` if it is a reference (0 if not).
    ///
    /// # Errors
    /// [`Sentinel::ColorSpaceCycle`] if the reference is on the path already.
    fn enter(&mut self, o: &Object) -> Result<i64, Error> {
        let Object::Ref(r) = o else {
            return Ok(0);
        };
        if r.num <= 0 {
            return Ok(0);
        }
        if !self.ancestors.insert(r.num) {
            return Err(
                Error::Sentinel(Sentinel::ColorSpaceCycle).context(format!("obj#{}", r.num))
            );
        }
        Ok(r.num)
    }

    /// `colorSpaceTraversal.leave`.
    fn leave(&mut self, obj_nr: i64) {
        if obj_nr != 0 {
            self.ancestors.remove(&obj_nr);
        }
    }

    /// `validateAlternateColorSpaceEntryForICC`.
    fn validate_alternate_for_icc(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        depth: u64,
    ) -> Result<(), Error> {
        let raw = d.find(entry_name.as_bytes()).cloned();
        let Some(o) = validate_entry(v.x, d, dict_name, entry_name, OPTIONAL, Version::V10)? else {
            return Ok(());
        };
        match o {
            Object::Name(n) => {
                if is_name_except_pattern(&n) {
                    Ok(())
                } else {
                    Err(Error::Validation(format!(
                        "{dict_name}.{entry_name}: invalid alternate color space name {:?}",
                        lossy(&n)
                    )))
                }
            }
            Object::Array(a) => {
                let target = raw.unwrap_or(Object::Array(a));
                self.validate_color_space_depth(v, &target, ColorSpaceRole::NoPattern, depth + 1)
                    .map_err(|e| e.context(format!("{dict_name}.{entry_name}")))
            }
            other => Err(Error::Validation(format!(
                "{dict_name}.{entry_name}: expected name or color space array, got {}",
                type_name(&other)
            ))),
        }
    }

    /// `validateICCBasedColorSpace` (8.6.5.5). Relaxed validation takes the version as 1.2.
    fn validate_icc_based_color_space(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        depth: u64,
    ) -> Result<(), Error> {
        let dict_name = "ICCBasedColorSpace";
        let since = Version::V12;
        v.x.validate_version(dict_name, since)?;
        if a.len() != 2 {
            return Err(Error::Validation(format!(
                "ICCBased color space: invalid array length {}, expected 2",
                a.len()
            )));
        }
        let Object::Ref(ir) = a[1] else {
            return Err(Error::Validation(format!(
                "ICCBased color space profile: expected indirect reference, got {}",
                type_name(&a[1])
            )));
        };
        let valid =
            v.x.is_valid(ir)
                .map_err(|e| e.context("ICCBased color space profile: check valid"))?;
        if valid {
            return Ok(());
        }

        let mut sd = validate_stream_dict_for_object(v.x, &a[1])
            .map_err(|e| e.context("ICCBased color space profile"))?;
        v.x.set_valid(ir)
            .map_err(|e| e.context("ICCBased color space profile: mark valid"))?;

        let n = validate_integer_entry(
            v.x,
            &sd.dict,
            dict_name,
            "N",
            REQUIRED,
            since,
            Some(&|i: i64| i == 1 || i == 3 || i == 4),
        )?
        .unwrap_or(0);
        self.validate_alternate_for_icc(v, &sd.dict, dict_name, "Alternate", depth)?;
        validate_number_array_entry(
            v.x,
            &sd.dict,
            dict_name,
            "Range",
            OPTIONAL,
            since,
            Some(&|a: &[Object]| i64::try_from(a.len()).is_ok_and(|l| l == 2 * n)),
        )?;

        // Metadata, stream, optional since V1.4
        let r = validate_metadata(v.x, &mut sd.dict, OPTIONAL, Version::V14);
        persist_stream_entries(v.x, ir, &sd.dict);
        r
    }

    /// `validateIndexedColorSpace` (8.6.6.3).
    fn validate_indexed_color_space(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        depth: u64,
    ) -> Result<(), Error> {
        let since = Version::V11;
        v.x.validate_version("IndexedColorSpace", since)?;
        if a.len() != 4 {
            return Err(Error::Validation(format!(
                "Indexed color space: invalid array length {}, expected 4",
                a.len()
            )));
        }

        // arr[1] base: base colorspace
        self.validate_color_space_depth(v, &a[1], ColorSpaceRole::IndexedBase, depth + 1)
            .map_err(|e| e.context("Indexed color space base"))?;

        // arr[2] hival: 0 <= int <= 255
        let hival = validate_integer_for_object(v.x, &a[2], Some(&|i: i64| (0..=255).contains(&i)))
            .map_err(|e| e.context("Indexed color space hival"))?;

        // arr[3] lookup: stream since V1.2 or byte string
        validate_indexed_color_space_lookup_table(v.x, &a[3], since)
            .map_err(|e| e.context("Indexed color space lookup"))?;
        validate_indexed_lookup_length(v, a, hival)
    }

    /// `validatePatternColorSpace`.
    fn validate_pattern_color_space(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        depth: u64,
    ) -> Result<(), Error> {
        v.x.validate_version("PatternColorSpace", Version::V12)?;
        if a.is_empty() || a.len() > 2 {
            return Err(Error::Validation(format!(
                "Pattern color space: invalid array length {}, expected 1 or 2",
                a.len()
            )));
        }

        // 8.7.3.3: arr[1]: name of underlying color space, any cs except PatternCS
        if a.len() == 2 {
            self.validate_color_space_depth(v, &a[1], ColorSpaceRole::NoPattern, depth + 1)
                .map_err(|e| e.context("Pattern color space underlying color space"))?;
        }
        Ok(())
    }

    /// `validateSeparationColorSpace` (8.6.6.4).
    fn validate_separation_color_space(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        depth: u64,
    ) -> Result<(), Error> {
        v.x.validate_version("SeparationColorSpace", Version::V12)?;
        if a.len() != 4 {
            return Err(Error::Validation(format!(
                "Separation color space: invalid array length {}, expected 4",
                a.len()
            )));
        }

        // arr[1]: colorant name, arbitrary
        validate_name_for_object(v.x, &a[1], None)
            .map_err(|e| e.context("Separation color space colorant name"))?;

        // arr[2]: alternate space
        self.validate_color_space_depth(v, &a[2], ColorSpaceRole::DeviceOrCie, depth + 1)
            .map_err(|e| e.context("Separation color space alternate color space"))?;

        // arr[3]: tintTransform, function
        validate_function(v, &a[3]).map_err(|e| e.context("Separation color space tint transform"))
    }

    /// `validateDeviceNColorSpaceColorantsDict`: the entries in key order.
    fn validate_device_n_colorants_dict(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        depth: u64,
    ) -> Result<(), Error> {
        for (name, obj) in d.iter() {
            v.check()?;
            self.validate_color_space_depth(v, obj, ColorSpaceRole::Separation, depth + 1)
                .map_err(|e| e.context(format!("DeviceN colorants {}", lossy(name))))?;
        }
        Ok(())
    }

    /// `validateDeviceNColorSpaceProcessDict`.
    fn validate_device_n_process_dict(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        depth: u64,
    ) -> Result<(), Error> {
        let dict_name = "DeviceNCSProcessDict";
        self.validate_color_space_entry_depth(
            v,
            d,
            dict_name,
            "ColorSpace",
            REQUIRED,
            ColorSpaceRole::DeviceOrCie,
            depth + 1,
        )?;
        validate_name_array_entry(
            v.x,
            d,
            dict_name,
            "Components",
            REQUIRED,
            Version::V10,
            None,
        )?;
        Ok(())
    }

    /// `validateDeviceNColorSpaceMixingHintsDict`.
    fn validate_device_n_mixing_hints_dict(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
    ) -> Result<(), Error> {
        let dict_name = "deviceNCSMixingHintsDict";

        if let Some(d1) = validate_dict_entry(
            v.x,
            d,
            dict_name,
            "Solidities",
            OPTIONAL,
            Version::V11,
            None,
        )? {
            // validateDeviceNColorSpaceSoliditiesDict
            for (name, obj) in d1.iter() {
                v.check()?;
                validate_float_for_object(v.x, obj, Some(&|f: f64| (0.0..=1.0).contains(&f)))
                    .map_err(|e| e.context(format!("DeviceN solidities {}", lossy(name))))?;
            }
        }

        validate_name_array_entry(
            v.x,
            d,
            dict_name,
            "PrintingOrder",
            REQUIRED,
            Version::V10,
            None,
        )?;

        if let Some(d1) =
            validate_dict_entry(v.x, d, dict_name, "DotGain", OPTIONAL, Version::V11, None)?
        {
            // validateDeviceNColorSpaceDotGainDict
            for (name, obj) in d1.iter() {
                v.check()?;
                validate_function(v, obj)
                    .map_err(|e| e.context(format!("DeviceN dot gain {}", lossy(name))))?;
            }
        }
        Ok(())
    }

    /// `validateDeviceNColorSpaceAttributesDict`. Relaxed validation takes `Subtype` and
    /// `Process` as 1.3.
    fn validate_device_n_attributes_dict(
        &mut self,
        v: &mut Validator<'_>,
        o: &Object,
        depth: u64,
    ) -> Result<(), Error> {
        let d =
            v.x.dereference_dict(o)
                .map_err(|e| e.context("DeviceN color space attributes: dereference dict"))?
                .cloned();
        let Some(d) = d else {
            return Err(Error::Validation(
                "DeviceN color space attributes: missing dict".to_string(),
            ));
        };

        let dict_name = "deviceNCSAttributesDict";
        let since = Version::V13;

        validate_name_entry(
            v.x,
            &d,
            dict_name,
            "Subtype",
            OPTIONAL,
            since,
            Some(&|s: &[u8]| s == b"DeviceN" || s == b"NChannel"),
        )?;

        if let Some(d1) = validate_dict_entry(
            v.x,
            &d,
            dict_name,
            "Colorants",
            OPTIONAL,
            Version::V11,
            None,
        )? {
            self.validate_device_n_colorants_dict(v, &d1, depth)?;
        }

        if let Some(d1) = validate_dict_entry(v.x, &d, dict_name, "Process", OPTIONAL, since, None)?
        {
            self.validate_device_n_process_dict(v, &d1, depth)?;
        }

        if let Some(d1) = validate_dict_entry(
            v.x,
            &d,
            dict_name,
            "MixingHints",
            OPTIONAL,
            Version::V16,
            None,
        )? {
            self.validate_device_n_mixing_hints_dict(v, &d1)?;
        }
        Ok(())
    }

    /// `validateDeviceNColorSpace` (8.6.6.5).
    fn validate_device_n_color_space(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        depth: u64,
    ) -> Result<(), Error> {
        v.x.validate_version("DeviceNColorSpace", Version::V13)?;
        if !(4..=5).contains(&a.len()) {
            return Err(Error::Validation(format!(
                "DeviceN color space: invalid array length {}, expected 4 or 5",
                a.len()
            )));
        }

        // arr[1]: array of names specifying the individual color components
        // length subject to implementation limit.
        validate_name_array(v.x, &a[1])
            .map_err(|e| e.context("DeviceN color space component names"))?;

        // arr[2]: alternate space
        self.validate_color_space_depth(v, &a[2], ColorSpaceRole::DeviceOrCie, depth + 1)
            .map_err(|e| e.context("DeviceN color space alternate color space"))?;

        // arr[3]: tintTransform, function
        validate_function(v, &a[3]).map_err(|e| e.context("DeviceN color space tint transform"))?;

        // arr[4]: color space attributes dict, optional
        if a.len() == 5 {
            self.validate_device_n_attributes_dict(v, &a[4], depth)
                .map_err(|e| e.context("DeviceN color space attributes"))?;
        }
        Ok(())
    }

    /// `validateCSArray`: the array `a` of the colour space `name` (not a device colour space).
    fn validate_cs_array(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        name: &[u8],
        depth: u64,
    ) -> Result<(), Error> {
        match name {
            // CIE-based
            b"CalGray" => validate_cal_gray_color_space(v.x, a),
            b"CalRGB" => validate_cal_rgb_color_space(v.x, a),
            b"Lab" => validate_lab_color_space(v.x, a),
            b"ICCBased" => self.validate_icc_based_color_space(v, a, depth),
            // Special
            b"Indexed" => self.validate_indexed_color_space(v, a, depth),
            b"Pattern" => self.validate_pattern_color_space(v, a, depth),
            b"Separation" => self.validate_separation_color_space(v, a, depth),
            b"DeviceN" => self.validate_device_n_color_space(v, a, depth),
            _ => Err(Error::Validation(format!(
                "color space array: undefined color space {:?}",
                lossy(name)
            ))),
        }
    }

    /// `validateColorSpaceArraySubset`.
    fn validate_color_space_array_subset(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        cs: &[&str],
    ) -> Result<(), Error> {
        let cs_name = color_space_array_name(v.x, a)?;
        if cs.iter().any(|s| s.as_bytes() == cs_name.as_slice()) {
            return self
                .validate_cs_array(v, a, &cs_name, 0)
                .map_err(|e| e.context(format!("color space {}", lossy(&cs_name))));
        }
        Err(Error::Validation(format!(
            "color space array: invalid color space {:?}",
            lossy(&cs_name)
        )))
    }

    /// `validateColorSpaceArray`.
    fn validate_color_space_array(
        &mut self,
        v: &mut Validator<'_>,
        a: &[Object],
        role: ColorSpaceRole,
        depth: u64,
    ) -> Result<(), Error> {
        let name = color_space_array_name(v.x, a)?;
        if !role.allows(&name) {
            return Err(Error::Validation(format!(
                "color space {}: not allowed as {}",
                lossy(&name),
                role.description()
            )));
        }

        let r = if is_device_name(&name) {
            // A device colour space should be a name; relaxed validation accepts the array.
            Ok(())
        } else {
            self.validate_cs_array(v, a, &name, depth)
        };
        r.map_err(|e| e.context(format!("color space {}", lossy(&name))))
    }

    /// `validateColorSpaceDepth`: the colour space `o` is or refers to, `depth` levels down.
    ///
    /// # Errors
    /// [`Error::Cancelled`], [`Sentinel::MaxRecursionDepth`], [`Sentinel::ColorSpaceCycle`],
    /// [`Error::Validation`] for an invalid colour space (see [`is_missing_color_space`]) and
    /// the errors of the dereference.
    pub fn validate_color_space_depth(
        &mut self,
        v: &mut Validator<'_>,
        o: &Object,
        role: ColorSpaceRole,
        depth: u64,
    ) -> Result<(), Error> {
        v.check()?;
        v.x.check_recursion_depth("colour-space graph", depth)?;
        let ancestor = self.enter(o)?;
        let r = self.validate_color_space_target(v, o, role, depth);
        self.leave(ancestor);
        r
    }

    fn validate_color_space_target(
        &mut self,
        v: &mut Validator<'_>,
        o: &Object,
        role: ColorSpaceRole,
        depth: u64,
    ) -> Result<(), Error> {
        let target =
            v.x.dereference(o)
                .map_err(|e| e.context("color space: dereference"))?
                .clone();
        match target {
            Object::Null => Err(missing_color_space()),
            Object::Name(n) => {
                if !role.allows(&n) {
                    return Err(Error::Validation(format!(
                        "color space {}: not allowed as {}",
                        lossy(&n),
                        role.description()
                    )));
                }
                if is_device_name(&n) || n == b"Pattern" {
                    Ok(())
                } else {
                    Err(Error::Validation(format!(
                        "color space name: invalid device color space name {:?}",
                        lossy(&n)
                    )))
                }
            }
            Object::Array(a) => self.validate_color_space_array(v, &a, role, depth),
            // Relaxed validation skips a value of another type.
            _ => Ok(()),
        }
    }

    /// `validateColorSpaceEntryDepth`.
    ///
    /// # Errors
    /// As [`ColorSpaceTraversal::validate_color_space_depth`].
    #[allow(clippy::too_many_arguments)]
    pub fn validate_color_space_entry_depth(
        &mut self,
        v: &mut Validator<'_>,
        d: &Dict,
        dict_name: &str,
        entry_name: &str,
        required: bool,
        role: ColorSpaceRole,
        depth: u64,
    ) -> Result<(), Error> {
        v.check()?;
        v.x.check_recursion_depth("colour-space graph", depth)?;
        let context = |e: Error| e.context(format!("{dict_name}.{entry_name}"));
        let raw = d.find(entry_name.as_bytes()).cloned();
        let Some(o) = validate_entry(v.x, d, dict_name, entry_name, required, Version::V10)
            .map_err(context)?
        else {
            return Ok(());
        };

        match o {
            Object::Name(n) => {
                if role.allows(&n) {
                    // A name that is no device colour space is skipped in relaxed validation.
                    Ok(())
                } else {
                    Err(context(Error::Validation(format!(
                        "color space {}: not allowed as {}",
                        lossy(&n),
                        role.description()
                    ))))
                }
            }
            Object::Array(a) => {
                let target = raw.unwrap_or(Object::Array(a));
                self.validate_color_space_depth(v, &target, role, depth)
                    .map_err(context)
            }
            other => Err(context(Error::Validation(format!(
                "expected name or color space array, got {}",
                type_name(&other)
            )))),
        }
    }
}

/// `validateColorSpaceArraySubset`: the colour space array `a`, which must be one of the spaces
/// named in `cs`.
///
/// # Errors
/// As [`ColorSpaceTraversal::validate_color_space_depth`].
pub fn validate_color_space_array_subset(
    v: &mut Validator<'_>,
    a: &[Object],
    cs: &[&str],
) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth("colour-space graph", 0)?;
    ColorSpaceTraversal::default().validate_color_space_array_subset(v, a, cs)
}

/// `validateColorSpaceEntry`: the colour space entry `entry_name` of `d`, a name or an array.
///
/// # Errors
/// As [`ColorSpaceTraversal::validate_color_space_depth`].
pub fn validate_color_space_entry(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    role: ColorSpaceRole,
) -> Result<(), Error> {
    ColorSpaceTraversal::default()
        .validate_color_space_entry_depth(v, d, dict_name, entry_name, required, role, 0)
}

/// The entries of a colour space resource dictionary, in key order. The names of the colour
/// spaces that refer to nothing go to `missing`.
fn validate_resource_entries(
    v: &mut Validator<'_>,
    d: &Dict,
    missing: &mut Vec<Vec<u8>>,
) -> Result<(), Error> {
    let mut t = ColorSpaceTraversal::default();
    for (name, entry) in d.iter() {
        v.check()?;
        match t.validate_color_space_depth(v, entry, ColorSpaceRole::Any, 0) {
            Ok(()) => {}
            Err(e) if is_missing_color_space(&e) => missing.push(name.clone()),
            Err(e) => {
                return Err(e.context(object_context(
                    &format!("ColorSpace resource {}", lossy(name)),
                    Some(entry),
                )));
            }
        }
    }
    Ok(())
}

/// `validateColorSpaceResourceDict` (8.6): the `ColorSpace` resource dictionary `o` is or refers
/// to. A colour space that refers to nothing is removed from it. The result is the repaired
/// copy if `o` is a direct dictionary that lost an entry (for the caller to store); a
/// dictionary that is a reference is changed in the table.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`] for a version that is too low or an invalid
/// colour space, and the errors of the dereference.
pub fn validate_color_space_resource_dict(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<Option<Dict>, Error> {
    v.x.validate_version("ColorSpaceResourceDict", since)
        .map_err(|e| e.context("ColorSpace resource dict: version"))?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("ColorSpace resource dict: dereference dict"))?
            .cloned();
    // Relaxed validation accepts a resource dictionary that refers to nothing.
    let Some(mut d) = d else {
        return Ok(None);
    };

    let mut missing = Vec::new();
    let r = validate_resource_entries(v, &d, &mut missing);
    let mut repaired = None;
    if !missing.is_empty() {
        match o {
            Object::Ref(ir) => {
                if let Some(stored) = v.x.dereference_dict_mut(*ir)? {
                    for name in &missing {
                        stored.0.remove(name);
                    }
                }
            }
            _ => {
                for name in &missing {
                    d.0.remove(name);
                }
                repaired = Some(d);
            }
        }
    }
    r?;
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::filter::FLATE;
    use crate::model::Entry;
    use crate::object::PdfFilter;
    use std::sync::atomic::AtomicBool;

    fn table(max_depth: u64, objects: Vec<(i64, Object)>) -> XRefTable {
        let mut x = XRefTable::new(Config {
            limits: Limits {
                max_recursion_depth: max_depth,
                ..Limits::default()
            },
            ..Config::default()
        });
        x.header_version = Some(Version::V17);
        for (n, o) in objects {
            x.table.insert(n, Entry::gen0(o));
        }
        x
    }

    fn r(num: i64) -> Object {
        Object::Ref(crate::object::Ref { num, generation: 0 })
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn function() -> Object {
        let mut d = Dict::default();
        d.set(b"FunctionType", Object::Integer(2));
        d.set(
            b"Domain",
            Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
        );
        d.set(b"C0", Object::Array(vec![Object::Integer(0); 3]));
        d.set(b"C1", Object::Array(vec![Object::Integer(1); 3]));
        d.set(b"N", Object::Integer(1));
        Object::Dict(d)
    }

    fn nested_color_space() -> Object {
        Object::Array(vec![
            name("Pattern"),
            Object::Array(vec![
                name("Indexed"),
                Object::Array(vec![
                    name("Separation"),
                    name("Spot"),
                    name("DeviceRGB"),
                    function(),
                ]),
                Object::Integer(1),
                Object::Hex(b"00ff".to_vec()),
            ]),
        ])
    }

    fn any(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(x, &cancel);
        ColorSpaceTraversal::default().validate_color_space_depth(&mut v, o, ColorSpaceRole::Any, 0)
    }

    #[test]
    fn indexed_lookup_decode_limit() {
        for (limit, fail) in [(0, false), (3, true), (4, false)] {
            let mut x = table(100, vec![]);
            x.conf.limits.max_decode_bytes = limit;
            let mut sd = StreamDict::new(
                Dict::default(),
                0,
                None,
                None,
                vec![PdfFilter {
                    name: FLATE.to_vec(),
                    decode_parms: None,
                }],
            );
            let cancel = AtomicBool::new(false);
            sd.content = Some(crate::Bytes::new(vec![0, 85, 170, 255], 4, None));
            sd.encode(x.mem(), &cancel).unwrap();
            sd.content = None;
            let mut v = Validator::new(&mut x, &cancel);
            let got = indexed_lookup_stream_len(&mut v, sd);
            if fail {
                assert!(got.unwrap_err().is_decode_limit_exceeded(), "limit {limit}");
            } else {
                assert_eq!(got.unwrap(), 4, "limit {limit}");
            }
        }
    }

    #[test]
    fn cancelled_resource_traversal_stops_before_the_dereference() {
        let mut x = table(100, vec![]);
        let cancel = AtomicBool::new(true);
        let mut d = Dict::default();
        d.set(b"A", name("DeviceRGB"));
        d.set(b"B", r(99));
        let mut v = Validator::new(&mut x, &cancel);
        let got = validate_color_space_resource_dict(&mut v, &Object::Dict(d), Version::V10);
        assert!(matches!(got, Err(Error::Cancelled)));
    }

    #[test]
    fn a_cycle_and_the_depth_are_rejected() {
        let cycle = Object::Array(vec![
            name("Indexed"),
            r(1),
            Object::Integer(1),
            Object::String(b"lookup".to_vec()),
        ]);
        let mut x = table(100, vec![(1, cycle)]);
        let e = any(&mut x, &r(1)).unwrap_err();
        assert!(e.is(Sentinel::ColorSpaceCycle), "{e}");

        let mut x = table(2, vec![]);
        let e = any(&mut x, &nested_color_space()).unwrap_err();
        assert!(e.is(Sentinel::MaxRecursionDepth), "{e}");
    }

    #[test]
    fn roles_are_enforced_before_the_descent() {
        let indexed = Object::Array(vec![
            name("Indexed"),
            Object::Array(vec![
                name("Indexed"),
                name("DeviceRGB"),
                Object::Integer(1),
                Object::String(b"x".to_vec()),
            ]),
            Object::Integer(1),
            Object::String(b"lookup".to_vec()),
        ]);
        let separation = Object::Array(vec![
            name("Separation"),
            name("Spot"),
            Object::Array(vec![
                name("Separation"),
                name("Alt"),
                name("DeviceRGB"),
                function(),
            ]),
            function(),
        ]);
        let mut x = table(100, vec![]);
        let e = any(&mut x, &indexed).unwrap_err();
        assert!(
            e.to_string()
                .contains("not allowed as Indexed base color space"),
            "{e}"
        );
        let e = any(&mut x, &separation).unwrap_err();
        assert!(
            e.to_string()
                .contains("not allowed as device or CIE-based color space"),
            "{e}"
        );
    }

    #[test]
    fn conforming_nesting_and_sharing_are_valid() {
        let shared = Object::Array(vec![
            name("Indexed"),
            name("DeviceRGB"),
            Object::Integer(1),
            Object::Hex(b"000000ffffff".to_vec()),
        ]);
        let mut x = table(100, vec![(1, shared)]);
        any(&mut x, &nested_color_space()).unwrap();
        let mut d = Dict::default();
        d.set(b"A", r(1));
        d.set(b"B", r(1));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let got = validate_color_space_resource_dict(&mut v, &Object::Dict(d), Version::V10);
        assert!(got.unwrap().is_none());
    }

    #[test]
    fn a_missing_color_space_is_removed_from_the_resources() {
        let mut x = table(100, vec![]);
        let mut d = Dict::default();
        d.set(b"A", name("DeviceRGB"));
        d.set(b"B", r(99));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let got = validate_color_space_resource_dict(&mut v, &Object::Dict(d), Version::V10);
        let repaired = got.unwrap().unwrap();
        assert!(repaired.find(b"A").is_some());
        assert!(repaired.find(b"B").is_none());
    }
}
