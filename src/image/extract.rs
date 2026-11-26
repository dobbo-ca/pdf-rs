//! `ExtractImage`, `ColorSpaceComponents` and the image entry helpers (Go:
//! pkg/pdfcpu/extract.go:128-545, image.go:34 and :302-361).
//!
//! Only what `ExtractImage(.., thumb=false, .., stub=false)` reaches is here: `imageStub`,
//! `ColorSpaceString`, `StreamLength` and the page and list helpers are not ported (docs/scope.md
//! section 5).

use super::render::render_image;
use super::{
    CAL_GRAY, CAL_RGB, DEVICE_CMYK, DEVICE_GRAY, DEVICE_N, DEVICE_RGB, ICC_BASED, INDEXED, LAB,
    SEPARATION, check,
};
use crate::error::Error;
use crate::filter::{CCITT_FAX, DCT, FLATE, JBIG2, JPX, LZW, RUN_LENGTH};
use crate::model::deref::type_name;
use crate::model::{Context, XRefTable};
use crate::object::{Dict, Object, StreamDict};
use std::sync::atomic::AtomicBool;

/// `model.Image` as `ExtractImage` returns it: the rendered bytes (Go's `Reader`) and the file
/// type: `"png"`, `"tif"`, `"jpg"`, `"jpx"` or `"jbig2"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub data: Vec<u8>,
    pub file_type: &'static str,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `ExtractImage` with `thumb` and `stub` false: decodes the image stream `sd` (the clone that
/// `dereference_stream_dict` returns, which the call changes: its content, the CCITT decode
/// parameters, the DCT component count) and renders it to a PNG or TIFF file, or passes a DCT
/// (not CMYK), JPX or JBIG2 stream through. `obj_nr` is the object number of the image, for
/// messages; `_resource_id` is the resource name, which the Go `Image` only carries along.
///
/// The decoded content is charged against the cumulative decode budget of the document (the
/// call changes it, so it is serial), and the pixel buffers and the output against the ledger.
/// The pixel and byte limits are `max_image_pixels` and `max_image_bytes`. `Some` always: Go
/// returns a nil image only together with an error.
///
/// # Errors
/// [`Error::UnsupportedResource`] for a filter or colour space that is not rendered,
/// [`Error::UnsupportedFilter`] for JPX or JBIG2 before the last filter, the errors of the stream
/// decode (the CMYK DCT path gives Go's `image/jpeg` errors as [`Error::Jpeg`]),
/// [`Error::ImageLimit`], the dereference and type errors of
/// the image entries, [`Error::Alloc`], and [`Error::Cancelled`].
pub fn extract_image(
    ctx: &mut Context,
    sd: &mut StreamDict,
    _resource_id: &[u8],
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<Option<Image>, Error> {
    check(cancel)?;
    let xref = &mut ctx.xref;

    // Go reads ImageMask here for imageStub; its errors count.
    image_boolean_entry(xref, sd, b"ImageMask", obj_nr)?;

    let (filters, last_filter) = prepare_extract_image(sd);
    let image = img(xref, sd, &filters, &last_filter, obj_nr, cancel)?;
    check(cancel)?;
    Ok(Some(image))
}

/// `prepareExtractImage` without the decode parameters (only `imageStub` reads them): the filter
/// names joined by commas, and the name of the last filter, empty for none.
fn prepare_extract_image(sd: &StreamDict) -> (String, Vec<u8>) {
    let names: Vec<String> = sd.filter_pipeline.iter().map(|f| lossy(&f.name)).collect();
    let last = sd
        .filter_pipeline
        .last()
        .map(|f| f.name.clone())
        .unwrap_or_default();
    (names.join(","), last)
}

/// `decodeImage`.
fn decode_image(
    xref: &mut XRefTable,
    sd: &mut StreamDict,
    filters: &str,
    last_filter: &[u8],
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    prepare_image_decode(xref, sd, &format!("image obj#{obj_nr}"))?;

    // CCITTDecoded images / (bit) masks don't have a ColorSpace attribute, but we render image
    // files.
    if last_filter == CCITT_FAX
        && xref
            .dereference_dict_entry(&sd.dict, b"ColorSpace")
            .is_err()
    {
        sd.dict.insert_name(b"ColorSpace", DEVICE_GRAY);
    }

    if last_filter == DCT {
        let comp = color_space_components(xref, sd)
            .map_err(|e| e.context(format!("image obj#{obj_nr} colorspace components")))?;
        sd.cs_components = comp;
    }

    match last_filter {
        DCT | JPX | JBIG2 | FLATE | LZW | CCITT_FAX | RUN_LENGTH => {
            match xref.decode_image_stream(sd, cancel) {
                // Go wraps both `ErrUnsupportedResource` and the filter error (`%w (%w)`); keep
                // the filter error as the root so the class stays `unsupported-filter`.
                Err(e) if matches!(e.root(), Error::UnsupportedFilter(_)) => Err(e.context(
                    format!("image obj#{obj_nr} filter {filters}: unsupported resource"),
                )),
                Err(e) => Err(e.context(format!("image obj#{obj_nr} decode"))),
                Ok(()) => Ok(()),
            }
        }
        _ => {
            Err(Error::UnsupportedResource.context(format!("image obj#{obj_nr} filter {filters}")))
        }
    }
}

/// `img`: the decoded stream, rendered.
fn img(
    xref: &mut XRefTable,
    sd: &mut StreamDict,
    filters: &str,
    last_filter: &[u8],
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<Image, Error> {
    if sd.filter_pipeline.is_empty() {
        sd.content = sd.raw.clone();
    } else {
        decode_image(xref, sd, filters, last_filter, obj_nr, cancel)?;
    }

    match render_image(xref, sd, obj_nr, cancel) {
        Ok((data, file_type)) => Ok(Image { data, file_type }),
        Err(e) if matches!(e.root(), Error::UnsupportedResource) => Err(e),
        Err(e) => Err(e.context(format!("image obj#{obj_nr} render"))),
    }
}

// ---------------------------------------------------------------------------------------------
// Colour space components

/// `colorSpaceDereferenceError`.
fn color_space_dereference_error(err: Error, o: &Object, topic: &str) -> Error {
    match o {
        Object::Ref(r) => err.context(format!("{topic} obj#{}: dereference", r.num)),
        _ => err.context(format!("{topic}: dereference")),
    }
}

/// `colorSpaceNameComponents`.
fn color_space_name_components(cs: &[u8]) -> i64 {
    match cs {
        DEVICE_GRAY => 1,
        DEVICE_RGB => 3,
        DEVICE_CMYK => 4,
        _ => 0,
    }
}

/// `colorSpaceArrayEntry`.
fn color_space_array_entry(cs: &[Object], index: usize) -> Result<&Object, Error> {
    cs.get(index)
        .ok_or_else(|| Error::Corrupt(format!("colorspace: missing array entry {index}")))
}

/// `colorSpaceArrayName`: the name in `cs[0]`.
pub(crate) fn color_space_array_name(
    xref: &mut XRefTable,
    cs: &[Object],
) -> Result<Vec<u8>, Error> {
    let first = cs
        .first()
        .ok_or_else(|| Error::Corrupt("colorspace: empty array".into()))?;
    let o = xref
        .dereference(first)
        .map_err(|e| e.context("colorspace[0]"))?;
    match o {
        Object::Name(n) => Ok(n.clone()),
        other => Err(Error::Corrupt(format!(
            "colorspace: expected name, got {}",
            type_name(other)
        ))),
    }
}

/// `dereferenceRequiredStreamDict`: the stream `o` is or refers to, as a clone.
pub(crate) fn dereference_required_stream_dict(
    xref: &mut XRefTable,
    o: &Object,
    topic: &str,
) -> Result<StreamDict, Error> {
    let (sd, _) = xref
        .dereference_stream_dict(o)
        .map_err(|e| e.context(topic))?;
    sd.ok_or_else(|| Error::Corrupt("missing stream dict".into()).context(topic))
}

/// `indexedColorSpaceComponents`.
fn indexed_color_space_components(
    xref: &mut XRefTable,
    cs: &[Object],
    depth: u64,
) -> Result<i64, Error> {
    let o = color_space_array_entry(cs, 1)?;
    let base = match xref.dereference(o) {
        Ok(b) => b.clone(),
        Err(e) => {
            return Err(color_space_dereference_error(
                e,
                o,
                "colorspace Indexed base",
            ));
        }
    };
    match &base {
        Object::Name(n) => Ok(color_space_name_components(n)),
        Object::Array(a) => color_space_array_components(xref, a, depth + 1),
        _ => Ok(0),
    }
}

/// `iccBasedColorSpaceComponents`: `/N` of the profile.
fn icc_based_color_space_components(xref: &mut XRefTable, cs: &[Object]) -> Result<i64, Error> {
    let o = color_space_array_entry(cs, 1)?;
    let profile = dereference_required_stream_dict(xref, o, "colorspace ICCBased profile")?;
    required_integer_entry(xref, &profile.dict, b"N", "colorspace ICCBased profile")
}

/// `deviceNColorSpaceComponents`: the number of colorants.
fn device_n_color_space_components(xref: &mut XRefTable, cs: &[Object]) -> Result<i64, Error> {
    let o = color_space_array_entry(cs, 1)?;
    let colorants = xref
        .dereference_array(o)
        .map_err(|e| e.context("colorspace: DeviceN colorants"))?;
    match colorants {
        Some(a) => Ok(i64::try_from(a.len()).unwrap_or(i64::MAX)),
        None => Err(Error::Corrupt(
            "colorspace: DeviceN colorants: expected array".into(),
        )),
    }
}

/// `colorSpaceArrayComponents`. `depth` is the number of Indexed bases above: Go recurses
/// without a bound (a base that refers back to its own array overflows its stack), here the
/// configured recursion limit applies.
fn color_space_array_components(
    xref: &mut XRefTable,
    cs: &[Object],
    depth: u64,
) -> Result<i64, Error> {
    xref.check_recursion_depth("colorspace", depth)?;
    let name = color_space_array_name(xref, cs)?;
    match name.as_slice() {
        CAL_GRAY => Ok(1),
        CAL_RGB | LAB => Ok(3),
        ICC_BASED => icc_based_color_space_components(xref, cs),
        SEPARATION => Ok(1),
        DEVICE_N => device_n_color_space_components(xref, cs),
        INDEXED => indexed_color_space_components(xref, cs, depth),
        _ => Ok(0),
    }
}

/// `ColorSpaceComponents`: the number of colour components of the colour space of image `sd`;
/// 0 if it has none or the colour space is not one that has a fixed count.
///
/// # Errors
/// The dereference errors of the colour space and its parts, naming what was being read, and
/// [`Error::Corrupt`] for an empty or malformed colour space array.
pub fn color_space_components(xref: &mut XRefTable, sd: &StreamDict) -> Result<i64, Error> {
    let Some(cs_object) = sd.dict.find(b"ColorSpace") else {
        return Ok(0);
    };
    let o = match xref.dereference(cs_object) {
        Ok(o) => o.clone(),
        Err(e) => return Err(color_space_dereference_error(e, cs_object, "colorspace")),
    };
    match &o {
        Object::Name(n) => Ok(color_space_name_components(n)),
        Object::Array(a) => color_space_array_components(xref, a, 0),
        _ => Ok(0),
    }
}

// ---------------------------------------------------------------------------------------------
// Entry helpers (image.go)

/// `imageBooleanEntry`: whether entry `key` of image `sd` is true; false if absent or null.
pub(crate) fn image_boolean_entry(
    xref: &mut XRefTable,
    sd: &StreamDict,
    key: &[u8],
    obj_nr: i64,
) -> Result<bool, Error> {
    match xref.dereference_boolean_entry(&sd.dict, key) {
        Ok((b, _)) => Ok(b == Some(true)),
        Err(e) => Err(e.context(format!("image obj#{obj_nr} entry \"{}\"", lossy(key)))),
    }
}

/// `integerEntryValue`: the integer entry `key` of `d`, resolved. `None` if it is absent (or
/// null) and not `required`.
pub(crate) fn integer_entry_value(
    xref: &mut XRefTable,
    d: &Dict,
    key: &[u8],
    context: &str,
    required: bool,
) -> Result<Option<i64>, Error> {
    let (i, _) = xref
        .dereference_integer_entry(d, key)
        .map_err(|e| e.context(context))?;
    if i.is_none() && required {
        return Err(Error::Corrupt(format!(
            "{context}: missing entry={}",
            lossy(key)
        )));
    }
    Ok(i)
}

/// [`integer_entry_value`] for a required entry, which is then there.
pub(crate) fn required_integer_entry(
    xref: &mut XRefTable,
    d: &Dict,
    key: &[u8],
    context: &str,
) -> Result<i64, Error> {
    integer_entry_value(xref, d, key, context, true)?
        .ok_or_else(|| Error::Corrupt(format!("{context}: missing entry={}", lossy(key))))
}

/// `resolvedCCITTDecodeParms`: a copy of the decode parameters in which `K`, `Columns` and
/// `Rows` are direct integers, and `Rows` is the image height if it was not given.
fn resolved_ccitt_decode_parms(
    xref: &mut XRefTable,
    sd_dict: &Dict,
    parms: Option<&Dict>,
    context: &str,
) -> Result<Dict, Error> {
    let mut resolved = parms.cloned().unwrap_or_default();
    let empty = Dict::default();
    let p = parms.unwrap_or(&empty);

    let keys: [&[u8]; 3] = [b"K", b"Columns", b"Rows"];
    let mut rows = None;
    for key in keys {
        let v = integer_entry_value(xref, p, key, &format!("{context} decode parameters"), false)?;
        if let Some(v) = v {
            resolved.set(key, Object::Integer(v));
        }
        if key == b"Rows" {
            rows = v;
        }
    }

    if rows.is_none() {
        let height = required_integer_entry(xref, sd_dict, b"Height", context)?;
        resolved.set(b"Rows", Object::Integer(height));
    }
    Ok(resolved)
}

/// `prepareImageDecode`: gives every CCITTFax filter of `sd` resolved decode parameters.
pub(crate) fn prepare_image_decode(
    xref: &mut XRefTable,
    sd: &mut StreamDict,
    context: &str,
) -> Result<(), Error> {
    for i in 0..sd.filter_pipeline.len() {
        if sd.filter_pipeline[i].name != CCITT_FAX {
            continue;
        }
        let parms = sd.filter_pipeline[i].decode_parms.clone();
        let resolved = resolved_ccitt_decode_parms(xref, &sd.dict, parms.as_ref(), context)?;
        sd.filter_pipeline[i].decode_parms = Some(resolved);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ledger::Bytes;
    use crate::model::Entry;
    use crate::object::{PdfFilter, Ref};

    fn xref() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn r(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v);
        }
        d
    }

    fn stream(d: Dict) -> StreamDict {
        StreamDict {
            dict: d,
            ..StreamDict::default()
        }
    }

    fn bytes(b: &[u8]) -> Bytes {
        Bytes::new(b.to_vec(), 0, None)
    }

    fn put(x: &mut XRefTable, num: i64, o: Object) {
        x.table.insert(num, Entry::gen0(o));
    }

    // TestICCBasedColorSpaceComponentsResolvesIndirectN.
    #[test]
    fn icc_based_components_resolve_an_indirect_n() {
        let mut x = xref();
        put(&mut x, 20, Object::Integer(3));
        put(
            &mut x,
            21,
            Object::Stream(Box::new(stream(dict(vec![("N", r(20))])))),
        );
        let sd = stream(dict(vec![(
            "ColorSpace",
            Object::Array(vec![name("ICCBased"), r(21)]),
        )]));
        assert_eq!(color_space_components(&mut x, &sd).unwrap(), 3);
    }

    #[test]
    fn components_of_the_other_colour_spaces() {
        let mut x = xref();
        let cases: Vec<(Object, i64)> = vec![
            (name("DeviceGray"), 1),
            (name("DeviceRGB"), 3),
            (name("DeviceCMYK"), 4),
            (name("Pattern"), 0),
            (
                Object::Array(vec![name("CalGray"), Object::Dict(Dict::default())]),
                1,
            ),
            (
                Object::Array(vec![name("CalRGB"), Object::Dict(Dict::default())]),
                3,
            ),
            (
                Object::Array(vec![name("Lab"), Object::Dict(Dict::default())]),
                3,
            ),
            (Object::Array(vec![name("Separation"), name("Spot")]), 1),
            (
                Object::Array(vec![
                    name("DeviceN"),
                    Object::Array(vec![name("A"), name("B")]),
                    name("DeviceRGB"),
                ]),
                2,
            ),
            (
                Object::Array(vec![
                    name("Indexed"),
                    name("DeviceCMYK"),
                    Object::Integer(1),
                    Object::Hex(b"00".to_vec()),
                ]),
                4,
            ),
            (Object::Array(vec![name("Unknown")]), 0),
            (Object::Integer(3), 0),
        ];
        for (cs, want) in cases {
            let sd = stream(dict(vec![("ColorSpace", cs.clone())]));
            assert_eq!(color_space_components(&mut x, &sd).unwrap(), want, "{cs:?}");
        }
        assert_eq!(
            color_space_components(&mut x, &stream(Dict::default())).unwrap(),
            0
        );
    }

    // TestColorSpaceComponentsRejectsMalformedArray.
    #[test]
    fn components_reject_a_malformed_array() {
        let cases: Vec<(&str, Vec<Object>, &str)> = vec![
            ("empty array", vec![], "empty array"),
            (
                "first entry is not a name",
                vec![Object::Integer(1)],
                "expected name",
            ),
            (
                "DeviceN missing colorants",
                vec![name("DeviceN")],
                "missing array entry 1",
            ),
            (
                "DeviceN colorants not array",
                vec![name("DeviceN"), name("Cyan")],
                "DeviceN colorants",
            ),
        ];
        for (what, cs, want) in cases {
            let sd = stream(dict(vec![("ColorSpace", Object::Array(cs))]));
            let e = color_space_components(&mut xref(), &sd).unwrap_err();
            assert!(e.to_string().contains(want), "{what}: {e}");
        }
    }

    #[test]
    fn a_base_that_refers_back_is_stopped() {
        let mut x = xref();
        // Object 5 is an Indexed space whose base is object 5: Go overflows its stack.
        put(
            &mut x,
            5,
            Object::Array(vec![
                name("Indexed"),
                r(5),
                Object::Integer(1),
                Object::Hex(b"00".to_vec()),
            ]),
        );
        let sd = stream(dict(vec![("ColorSpace", r(5))]));
        let e = color_space_components(&mut x, &sd).unwrap_err();
        assert!(e.is(crate::error::Sentinel::MaxRecursionDepth), "{e}");
    }

    #[test]
    fn colour_space_dereference_errors_name_the_object() {
        let mut x = xref();
        put(&mut x, 9, Object::Integer(1));
        // An ICC profile that is not a stream.
        let sd = stream(dict(vec![(
            "ColorSpace",
            Object::Array(vec![name("ICCBased"), r(9)]),
        )]));
        let e = color_space_components(&mut x, &sd).unwrap_err();
        assert!(e.to_string().contains("colorspace ICCBased profile"), "{e}");
    }

    // TestPrepareImageDecodeResolvesCCITTIntegers.
    #[test]
    fn prepare_image_decode_resolves_ccitt_integers() {
        for (rows, want_rows) in [(None, 12), (Some(r(33)), 9)] {
            let mut x = xref();
            put(&mut x, 30, Object::Integer(-1));
            put(&mut x, 31, Object::Integer(1728));
            put(&mut x, 32, Object::Integer(12));
            put(&mut x, 33, Object::Integer(9));
            let mut parms = dict(vec![("K", r(30)), ("Columns", r(31))]);
            if let Some(rows) = rows {
                parms.set(b"Rows", rows);
            }
            let mut sd = stream(dict(vec![("Height", r(32))]));
            sd.filter_pipeline = vec![PdfFilter {
                name: CCITT_FAX.to_vec(),
                decode_parms: Some(parms.clone()),
            }];
            prepare_image_decode(&mut x, &mut sd, "test image").unwrap();
            let got = sd.filter_pipeline[0].decode_parms.as_ref().unwrap();
            assert_eq!(got.int_entry(b"K"), Some(-1));
            assert_eq!(got.int_entry(b"Columns"), Some(1728));
            assert_eq!(got.int_entry(b"Rows"), Some(want_rows));
            // The raw decode parameters were not mutated.
            assert!(matches!(parms.find(b"K"), Some(Object::Ref(_))));
        }
    }

    #[test]
    fn prepare_image_decode_leaves_other_filters_alone() {
        let mut sd = stream(Dict::default());
        sd.filter_pipeline = vec![PdfFilter {
            name: FLATE.to_vec(),
            decode_parms: None,
        }];
        prepare_image_decode(&mut xref(), &mut sd, "x").unwrap();
        assert!(sd.filter_pipeline[0].decode_parms.is_none());
        // A CCITT filter without parameters needs the height.
        sd.filter_pipeline = vec![PdfFilter {
            name: CCITT_FAX.to_vec(),
            decode_parms: None,
        }];
        let e = prepare_image_decode(&mut xref(), &mut sd, "image obj#1").unwrap_err();
        assert!(e.to_string().contains("missing entry=Height"), "{e}");
    }

    // TestExtractImageBooleanErrorsIncludeContext (without the object attribution, which the
    // validation error of this port does not carry).
    #[test]
    fn boolean_errors_include_context() {
        for key in ["ImageMask", "Interpolate"] {
            let mut sd = stream(dict(vec![("Width", Object::Integer(1))]));
            sd.dict.set(key.as_bytes(), r(9));
            let e = image_boolean_entry(&mut xref(), &sd, key.as_bytes(), 7).unwrap_err();
            let s = e.to_string();
            assert!(s.contains("image obj#7") && s.contains(key), "{s}");
        }
        // An indirect true.
        let mut x = xref();
        put(&mut x, 9, Object::Bool(true));
        let sd = stream(dict(vec![("ImageMask", r(9))]));
        assert!(image_boolean_entry(&mut x, &sd, b"ImageMask", 7).unwrap());
        assert!(!image_boolean_entry(&mut x, &sd, b"Interpolate", 7).unwrap());
    }

    // TestDereferenceRequiredStreamDictPreservesMissingStreamDict.
    #[test]
    fn a_missing_stream_dict_keeps_its_context() {
        let e = dereference_required_stream_dict(&mut xref(), &r(1), "image").unwrap_err();
        assert!(e.to_string().contains("image"), "{e}");
        assert!(e.to_string().contains("missing stream dict"), "{e}");
    }

    fn gray_image(raw: &[u8]) -> StreamDict {
        let mut sd = stream(dict(vec![
            ("BitsPerComponent", Object::Integer(8)),
            ("ColorSpace", name("DeviceGray")),
            ("Height", Object::Integer(1)),
            ("Width", Object::Integer(2)),
        ]));
        sd.raw = Some(bytes(raw));
        sd
    }

    fn ctx() -> Context {
        Context::new(0, Config::default()).unwrap()
    }

    #[test]
    fn extracts_an_unfiltered_gray_image_as_png() {
        let mut c = ctx();
        let mut sd = gray_image(&[0x10, 0xf0]);
        let cancel = AtomicBool::new(false);
        let im = extract_image(&mut c, &mut sd, b"Im0", 7, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(im.file_type, "png");
        let d = crate::image::png::tests::decode(&im.data);
        assert_eq!((d.w, d.h, d.color_type), (2, 1, 2));
        assert_eq!(d.pixels, [0x10, 0x10, 0x10, 0xf0, 0xf0, 0xf0]);
        assert!(sd.content.is_some());
    }

    #[test]
    fn imagemask_errors_come_first_and_cancel_is_honoured() {
        let mut c = ctx();
        let mut sd = gray_image(&[0, 0]);
        sd.dict.set(b"ImageMask", r(9));
        let cancel = AtomicBool::new(false);
        let e = extract_image(&mut c, &mut sd, b"", 7, &cancel).unwrap_err();
        assert!(e.to_string().contains("image obj#7"), "{e}");

        let mut sd = gray_image(&[0, 0]);
        let e = extract_image(&mut c, &mut sd, b"", 7, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    #[test]
    fn jbig2_and_jpx_are_passed_through() {
        for (filter, ty) in [(JBIG2, "jbig2"), (JPX, "jpx")] {
            let mut c = ctx();
            let mut sd = stream(Dict::default());
            sd.raw = Some(bytes(&[0x97, 0x4a, 0x42, 0x32]));
            sd.filter_pipeline = vec![PdfFilter {
                name: filter.to_vec(),
                decode_parms: None,
            }];
            let im = extract_image(&mut c, &mut sd, b"", 3, &AtomicBool::new(false))
                .unwrap()
                .unwrap();
            assert_eq!(
                (im.file_type, im.data.as_slice()),
                (ty, &[0x97, 0x4a, 0x42, 0x32][..])
            );
        }
    }

    #[test]
    fn a_filter_that_is_not_rendered_is_an_unsupported_resource() {
        let mut c = ctx();
        let mut sd = gray_image(b"4142");
        sd.filter_pipeline = vec![PdfFilter {
            name: b"ASCIIHexDecode".to_vec(),
            decode_parms: None,
        }];
        let e = extract_image(&mut c, &mut sd, b"", 7, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(e.root(), &Error::UnsupportedResource);
        assert!(
            e.to_string().contains("image obj#7 filter ASCIIHexDecode"),
            "{e}"
        );
    }

    // pdf20__utf8test.fl1: `[/JBIG2Decode /FlateDecode]`. Go wraps ErrUnsupportedResource and
    // ErrUnsupportedFilter; the oracle classifies the filter first.
    #[test]
    fn an_unsupported_filter_mid_pipeline_keeps_its_class() {
        let mut c = ctx();
        let mut sd = gray_image(b"4142");
        sd.filter_pipeline = vec![
            PdfFilter {
                name: JBIG2.to_vec(),
                decode_parms: None,
            },
            PdfFilter {
                name: FLATE.to_vec(),
                decode_parms: None,
            },
        ];
        let e = extract_image(&mut c, &mut sd, b"", 7, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(e.class(), "unsupported-filter", "{e}");
        assert!(
            e.to_string()
                .contains("image obj#7 filter JBIG2Decode,FlateDecode: unsupported resource"),
            "{e}"
        );
    }

    #[test]
    fn a_flate_image_is_decoded_then_rendered() {
        // zlib of [0x10, 0xf0] (stored block).
        let z = [
            0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x10, 0xf0, 0x01, 0x12, 0x01, 0x01,
        ];
        let mut c = ctx();
        let mut sd = gray_image(&z);
        sd.filter_pipeline = vec![PdfFilter {
            name: FLATE.to_vec(),
            decode_parms: None,
        }];
        let im = extract_image(&mut c, &mut sd, b"", 7, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(im.file_type, "png");
        let d = crate::image::png::tests::decode(&im.data);
        assert_eq!(d.pixels, [0x10, 0x10, 0x10, 0xf0, 0xf0, 0xf0]);
    }
}
