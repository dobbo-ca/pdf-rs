//! The image renderers (Go: pkg/pdfcpu/writeImage.go:63-1186, without `WriteImage`): an image
//! stream whose content is decoded becomes a PNG or TIFF file, or a DCT, JPX or JBIG2 stream is
//! passed through.
//!
//! Go hands each result over as an `io.Reader` and a file type; here it is the bytes and the
//! type. Where Go indexes past a slice and panics (a soft mask shorter than the image, a lookup
//! table shorter than an index) the result is [`Error::Panic`]. `thumb` is not carried:
//! `ExtractImage` passes `false` and nothing reads it.

use super::extract::{
    color_space_components, dereference_required_stream_dict, image_boolean_entry,
    integer_entry_value, prepare_image_decode, required_integer_entry,
};
use super::png::Pixels;
use super::tiff::Kind;
use super::{
    CAL_RGB, DEVICE_CMYK, DEVICE_GRAY, DEVICE_N, DEVICE_RGB, ICC_BASED, INDEXED, SEPARATION, check,
    copy_charged, out_of_range, png, tiff,
};
use crate::config::Limits;
use crate::error::Error;
use crate::filter::{ASCII85, CCITT_FAX, DCT, FLATE, JBIG2, JPX, LZW, RUN_LENGTH, gob};
use crate::ledger::{Bytes, Held, Mem};
use crate::model::XRefTable;
use crate::model::deref::type_name;
use crate::object::string::{hex_decode, unescape};
use crate::object::write::array_go_string;
use crate::object::{Array, Object, StreamDict};
use std::sync::atomic::AtomicBool;

/// A rendered image: its bytes and its file type.
pub(crate) type Rendered = (Vec<u8>, &'static str);

/// What every renderer needs besides the image: the ledger and the cancel flag.
#[derive(Clone, Copy)]
struct Env<'a> {
    mem: &'a Mem,
    cancel: &'a AtomicBool,
}

/// `colValRange`: a numeric range for colour space component values that may be inverted.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColRange {
    min: f64,
    max: f64,
}

/// `PDFImage`: an image XObject, its decoded content and what the renderers need of its
/// dictionary.
struct PdfImage<'a> {
    obj_nr: i64,
    /// The decoded content (Go's `sd.Content`).
    content: &'a [u8],
    comp: i64,
    bpc: i64,
    w: i64,
    h: i64,
    soft_mask: Option<Bytes>,
    decode: Vec<ColRange>,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn array_text(a: &[Object]) -> String {
    lossy(&array_go_string(a))
}

fn overflow() -> Error {
    Error::Corrupt("integer overflow".into())
}

fn to_us(v: i64) -> Result<usize, Error> {
    usize::try_from(v).map_err(|_| overflow())
}

fn len_i64(b: &[u8]) -> i64 {
    i64::try_from(b.len()).unwrap_or(i64::MAX)
}

/// `safemath.MultiplyInt64`: no negative operand and no overflow.
fn multiply_i64(a: i64, b: i64) -> Result<i64, Error> {
    if a < 0 || b < 0 || (a != 0 && b > i64::MAX / a) {
        return Err(overflow());
    }
    Ok(a * b)
}

/// Go's `uint8(f)` of a `float64` on amd64: the value is converted to an `int32` (0x80000000 for
/// NaN and for what does not fit) and cut to 8 bits. Rust's `as` would saturate.
fn go_u8(f: f64) -> u8 {
    let i = if f.is_nan() || f >= 2_147_483_648.0 || f <= -2_147_483_649.0 {
        i32::MIN
    } else {
        f as i32
    };
    i as u8
}

/// `checkedImageRowBytes`.
fn checked_image_row_bytes(w: i64, comp: i64, bpc: i64) -> Result<i64, Error> {
    let bits = multiply_i64(w, comp)?;
    let bits = multiply_i64(bits, bpc)?;
    if bits > i64::MAX - 7 {
        return Err(Error::Corrupt("image dimension overflow".into()));
    }
    Ok((bits + 7) / 8)
}

/// `checkedImageBytes`.
fn checked_image_bytes(w: i64, h: i64, comp: i64, bpc: i64) -> Result<i64, Error> {
    let row_bytes = checked_image_row_bytes(w, comp, bpc)?;
    multiply_i64(row_bytes, h)
}

/// `validatePDFImageDimensions`: positive sizes, within `max_image_pixels` and `max_image_bytes`
/// (the raw bytes and 4 bytes per pixel for the render).
fn validate_pdf_image_dimensions(
    limits: &Limits,
    w: i64,
    h: i64,
    comp: i64,
    bpc: i64,
    obj_nr: i64,
) -> Result<(), Error> {
    if w <= 0 || h <= 0 {
        return Err(Error::Corrupt(format!(
            "image obj#{obj_nr} has invalid dimensions {w}x{h}"
        )));
    }
    if comp <= 0 || bpc <= 0 {
        return Err(Error::Corrupt(format!(
            "image obj#{obj_nr} has invalid components/bpc {comp}/{bpc}"
        )));
    }

    let max_pixels = i64::try_from(limits.max_image_pixels).unwrap_or(i64::MAX);
    let max_bytes = i64::try_from(limits.max_image_bytes).unwrap_or(i64::MAX);
    let pixels = multiply_i64(w, h)?;
    if pixels > max_pixels {
        return Err(Error::ImageLimit { pixels: true }.context(format!(
            "image obj#{obj_nr} pixel count {pixels} exceeds limit {max_pixels}"
        )));
    }

    let raw_bytes = checked_image_bytes(w, h, comp, bpc)?;
    let render_bytes = multiply_i64(pixels, 4)?;
    if raw_bytes > max_bytes || render_bytes > max_bytes {
        return Err(Error::ImageLimit { pixels: false }.context(format!(
            "image obj#{obj_nr} byte size exceeds limit {max_bytes}"
        )));
    }
    Ok(())
}

/// `decodeArr`: the pairs of the `Decode` array; a last value without a partner is dropped.
fn decode_arr(
    xref: &mut XRefTable,
    o: Option<&Object>,
    obj_nr: i64,
) -> Result<Vec<ColRange>, Error> {
    let null = Object::Null;
    let a: Array = match xref.dereference_array(o.unwrap_or(&null)) {
        Err(e) => return Err(e.context(format!("image obj#{obj_nr} Decode"))),
        Ok(None) => return Ok(Vec::new()),
        Ok(Some(a)) => a.clone(),
    };

    let mut decode = Vec::new();
    let mut min = 0.0;
    for (i, o) in a.iter().enumerate() {
        let f = xref
            .dereference_number(o)
            .map_err(|e| e.context(format!("image obj#{obj_nr} Decode[{i}]")))?;
        if i.is_multiple_of(2) {
            min = f;
            continue;
        }
        decode.push(ColRange { min, max: f });
    }
    Ok(decode)
}

/// `pdfImage`: reads what the renderers need of the image dictionary and checks it against the
/// limits.
fn pdf_image<'a>(
    xref: &mut XRefTable,
    sd: &'a StreamDict,
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<PdfImage<'a>, Error> {
    let mut comp = color_space_components(xref, sd)?;

    let context = format!("image obj#{obj_nr}");
    let img_mask = image_boolean_entry(xref, sd, b"ImageMask", obj_nr)?;
    let mut bpc = integer_entry_value(xref, &sd.dict, b"BitsPerComponent", &context, !img_mask)?;
    if img_mask {
        comp = 1;
        if bpc.is_none() {
            bpc = Some(1);
        }
    }
    let w = required_integer_entry(xref, &sd.dict, b"Width", &context)?;
    let h = required_integer_entry(xref, &sd.dict, b"Height", &context)?;
    let bpc =
        bpc.ok_or_else(|| Error::Corrupt(format!("{context}: missing entry=BitsPerComponent")))?;

    validate_pdf_image_dimensions(&xref.conf.limits, w, h, comp, bpc, obj_nr)?;

    let decode = decode_arr(xref, sd.dict.find(b"Decode"), obj_nr)?;
    let soft_mask = soft_mask(xref, sd, w, h, obj_nr, cancel)?;

    Ok(PdfImage {
        obj_nr,
        content: sd.content.as_deref().unwrap_or(&[]),
        comp,
        bpc,
        w,
        h,
        soft_mask,
        decode,
    })
}

/// `streamBytes`: the decoded content of `sd`, `None` for a filter this does not decode. `sd` is
/// a clone: the decode does not persist in the document.
fn stream_bytes(
    xref: &mut XRefTable,
    sd: &mut StreamDict,
    context: &str,
    cancel: &AtomicBool,
) -> Result<Option<Bytes>, Error> {
    prepare_image_decode(xref, sd, context)?;

    let decode = |xref: &mut XRefTable, sd: &mut StreamDict| {
        xref.decode_image_stream(sd, cancel)
            .map_err(|e| e.context(format!("{context} decode")))
    };

    let last = sd.filter_pipeline.last().map(|f| f.name.clone());
    let Some(last) = last else {
        decode(xref, sd)?;
        return Ok(sd.content.clone());
    };

    // If the colour space is CMYK then write .tif else write .png.
    match last.as_slice() {
        DCT | FLATE | CCITT_FAX | ASCII85 | RUN_LENGTH | JPX | JBIG2 => decode(xref, sd)?,
        // Skip the image: this filter is not supported.
        _ => return Ok(None),
    }
    Ok(sd.content.clone())
}

/// `softMask`: the soft mask of the image, `None` if it has none or this cannot use it (not 8
/// bits, not `w * h` bytes).
fn soft_mask(
    xref: &mut XRefTable,
    d: &StreamDict,
    w: i64,
    h: i64,
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<Option<Bytes>, Error> {
    // TODO Process optional "Matte".
    let Some(o) = d.dict.find(b"SMask").filter(|o| !o.is_null()) else {
        // No soft mask available.
        return Ok(None);
    };

    // Soft mask present.
    let (sm_sd, _) = xref.dereference_stream_dict(o)?;
    // Go goes on with a nil stream dictionary and fails on its first use.
    let Some(mut sm_sd) = sm_sd else {
        return Err(Error::Panic("nil pointer dereference"));
    };

    let context = format!("image obj#{obj_nr} soft mask");
    let sm = stream_bytes(xref, &mut sm_sd, &context, cancel)?;

    let Some(bpc) = integer_entry_value(xref, &sm_sd.dict, b"BitsPerComponent", &context, false)?
    else {
        // Ignoring a soft mask without bpc.
        return Ok(None);
    };

    // TODO support soft masks with bpc != 8.
    if bpc != 8 {
        return Ok(None);
    }

    if let Some(b) = &sm {
        let want = multiply_i64(w, h)
            .and_then(|px| multiply_i64(px, bpc))
            .map(|bits| bits.saturating_add(7) / 8);
        // Ignoring a corrupt soft mask.
        if want.ok() != Some(len_i64(b)) {
            return Ok(None);
        }
    }

    Ok(sm)
}

/// `colorLookupTable`: the colour lookup table of an Indexed colour space, `None` for anything
/// that is not a string or a stream.
fn color_lookup_table(
    xref: &mut XRefTable,
    o: &Object,
    cancel: &AtomicBool,
) -> Result<Option<Bytes>, Error> {
    // Go drops the error and goes on with nil.
    let o = xref.dereference(o).cloned().unwrap_or(Object::Null);
    let table = |v: Vec<u8>| Bytes::new(v, 0, None);
    match o {
        Object::String(s) => Ok(Some(table(unescape(&s)))),
        Object::Hex(h) => Ok(Some(table(hex_decode(&h)?))),
        Object::Stream(mut sd) => stream_bytes(xref, &mut sd, "image color lookup stream", cancel),
        _ => Ok(None),
    }
}

fn max_val_for_bits(bpc: usize) -> u32 {
    1u32.checked_shl(bpc as u32).map_or(0, |v| v - 1)
}

/// `decodePixelValue`: decodes `v` into the `bpc` deep colour image space using the applicable
/// Decode array component.
fn decode_pixel_value(v: u8, bpc: usize, r: ColRange) -> u8 {
    let q = f64::from(max_val_for_bits(bpc));
    let f = r.min + (f64::from(v) * (r.max - r.min) / q);
    go_u8(f * q)
}

fn valid_bpc(bpc: i64) -> bool {
    matches!(bpc, 1 | 2 | 4 | 8 | 16)
}

/// `packedImageSample`: sample number `sample` of the row that starts at `row_offset`.
fn packed_image_sample(
    b: &[u8],
    row_offset: usize,
    sample: usize,
    bpc: usize,
) -> Result<u16, Error> {
    let bit_offset = sample.checked_mul(bpc).ok_or_else(out_of_range)?;
    let byte_offset = row_offset
        .checked_add(bit_offset / 8)
        .ok_or_else(out_of_range)?;
    if bpc == 16 {
        let hi = *b.get(byte_offset).ok_or_else(out_of_range)?;
        let lo = *b.get(byte_offset + 1).ok_or_else(out_of_range)?;
        return Ok((u16::from(hi) << 8) | u16::from(lo));
    }
    let shift = 8usize
        .checked_sub(bpc + bit_offset % 8)
        .ok_or_else(out_of_range)?;
    let v = *b.get(byte_offset).ok_or_else(out_of_range)?;
    let shifted = u16::from(v.checked_shr(shift as u32).unwrap_or(0));
    Ok(shifted & max_val_for_bits(bpc) as u16)
}

/// `decodedImageSample`: the sample as 8 bits, through the Decode array.
fn decoded_image_sample(
    im: &PdfImage<'_>,
    row_offset: usize,
    sample: usize,
    component: usize,
) -> Result<u8, Error> {
    let bpc = to_us(im.bpc)?;
    let v = packed_image_sample(im.content, row_offset, sample, bpc)?;
    let r = im
        .decode
        .get(component)
        .copied()
        .unwrap_or(ColRange { min: 0.0, max: 1.0 });
    let f = r.min + f64::from(v) * (r.max - r.min) / f64::from(max_val_for_bits(bpc));
    if f <= 0.0 {
        return Ok(0);
    }
    if f >= 1.0 {
        return Ok(255);
    }
    Ok(go_u8(f * 255.0))
}

/// `cmykSample`: the sample as 8 bits.
fn cmyk_sample(im: &PdfImage<'_>, row_offset: usize, sample: usize) -> Result<u8, Error> {
    let bpc = to_us(im.bpc)?;
    let v = packed_image_sample(im.content, row_offset, sample, bpc)?;
    Ok(match bpc {
        8 => v as u8,
        16 => (v >> 8) as u8,
        _ => (u32::from(v) * 255 / max_val_for_bits(bpc).max(1)) as u8,
    })
}

/// `color.CMYKToRGB`.
fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> (u8, u8, u8) {
    let w = 0xffff - u32::from(k) * 0x101;
    let r = (0xffff - u32::from(c) * 0x101) * w / 0xffff;
    let g = (0xffff - u32::from(m) * 0x101) * w / 0xffff;
    let b = (0xffff - u32::from(y) * 0x101) * w / 0xffff;
    ((r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8)
}

/// The alpha of pixel `i` of a soft-masked image: 255 without a mask.
fn alpha_at(im: &PdfImage<'_>, i: usize) -> Result<u8, Error> {
    match &im.soft_mask {
        Some(sm) => sm.get(i).copied().ok_or_else(out_of_range),
        None => Ok(255),
    }
}

/// The width and height as `usize`, which the pixel loops need to be at least 1 (Go's
/// `image.NewNRGBA` of an empty rectangle and `png.Encode` refuse it).
fn dims(im: &PdfImage<'_>) -> Result<(usize, usize), Error> {
    let (w, h) = (to_us(im.w)?, to_us(im.h)?);
    if w == 0 || h == 0 {
        return Err(Error::Corrupt(format!("invalid image size: {w}x{h}")));
    }
    Ok((w, h))
}

/// A zeroed pixel buffer of `w * h` pixels of `bpp` bytes, charged.
fn new_pix(env: Env<'_>, w: usize, h: usize, bpp: usize) -> Result<(Vec<u8>, Held), Error> {
    let n = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(bpp))
        .ok_or_else(overflow)?;
    env.mem.alloc_zeroed(n)
}

/// The check of the sample depth that the packed renderers start with.
fn check_bpc(im: &PdfImage<'_>, who: &str) -> Result<(), Error> {
    if valid_bpc(im.bpc) {
        Ok(())
    } else {
        Err(Error::Corrupt(format!(
            "{who}: unsupported bits per component {}",
            im.bpc
        )))
    }
}

/// The checks that the packed renderers share: the sample depth, the row size, and that the
/// content holds `h` rows of `w` pixels of `comp` samples. Returns `w`, `h` and the row size.
fn packed_dims(im: &PdfImage<'_>, comp: i64, who: &str) -> Result<(usize, usize, usize), Error> {
    check_bpc(im, who)?;
    let row_bytes = checked_image_row_bytes(im.w, comp, im.bpc).map_err(|e| e.context(who))?;
    let image_bytes = multiply_i64(row_bytes, im.h).map_err(|e| e.context(who))?;
    // For streams not using compression there is a trailing 0x0A in addition to the image bytes.
    if len_i64(im.content) < image_bytes {
        return Err(Error::Corrupt(format!(
            "{who}: corrupt image object: need {image_bytes} bytes, have {}",
            im.content.len()
        )));
    }
    Ok((to_us(im.w)?, to_us(im.h)?, to_us(row_bytes)?))
}

fn put_pixel(pix: &mut [u8], i: usize, px: [u8; 4]) {
    if let Some(d) = pix.get_mut(i * 4..i * 4 + 4) {
        d.copy_from_slice(&px);
    }
}

/// `renderDeviceCMYKToTIFF`.
fn render_device_cmyk_to_tiff(env: Env<'_>, im: &PdfImage<'_>) -> Result<Rendered, Error> {
    let who = format!("image obj#{} CMYK", im.obj_nr);
    check_bpc(im, &who)?;
    if im.comp < 4 {
        return Err(Error::Corrupt(format!(
            "{who}: need at least 4 components, have {}",
            im.comp
        )));
    }
    let (w, h, row_bytes) = packed_dims(im, im.comp, &who)?;
    let comp = to_us(im.comp)?;
    let masked = im.soft_mask.is_some();
    if w == 0 || h == 0 {
        return Err(Error::Corrupt(format!("invalid image size: {w}x{h}")));
    }

    // Preserve the CMYK colour model for print applications; with a soft mask the image is
    // NRGBA. TODO support decode.
    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    for y in 0..h {
        check(env.cancel)?;
        let row_offset = y * row_bytes;
        for x in 0..w {
            let sample = x * comp;
            let c = cmyk_sample(im, row_offset, sample)?;
            let m = cmyk_sample(im, row_offset, sample + 1)?;
            let yy = cmyk_sample(im, row_offset, sample + 2)?;
            let k = cmyk_sample(im, row_offset, sample + 3)?;
            let px = if masked {
                let (r, g, b) = cmyk_to_rgb(c, m, yy, k);
                [r, g, b, alpha_at(im, y * w + x)?]
            } else {
                [c, m, yy, k]
            };
            put_pixel(&mut pix, y * w + x, px);
        }
    }

    let kind = if masked { Kind::Nrgba } else { Kind::Cmyk };
    Ok((tiff::encode(env.mem, w, h, kind, &pix)?, "tif"))
}

/// `renderDeviceGrayToPNG`.
fn render_device_gray_to_png(env: Env<'_>, im: &PdfImage<'_>) -> Result<Rendered, Error> {
    let who = format!("renderDeviceGrayToPNG: objNr={}", im.obj_nr);
    let (w, h, row_bytes) = packed_dims(im, 1, &who)?;
    dims(im)?;

    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    for y in 0..h {
        check(env.cancel)?;
        let row_offset = y * row_bytes;
        for x in 0..w {
            let v = decoded_image_sample(im, row_offset, x, 0)?;
            let alpha = alpha_at(im, y * w + x)?;
            put_pixel(&mut pix, y * w + x, [v, v, v, alpha]);
        }
    }
    Ok((
        png::encode(env.mem, w, h, Pixels::Nrgba(&pix), env.cancel)?,
        "png",
    ))
}

/// `renderRGBToPNG`.
fn render_rgb_to_png(env: Env<'_>, im: &PdfImage<'_>, context: &str) -> Result<Rendered, Error> {
    let who = format!("{context}: objNr={}", im.obj_nr);
    let (w, h, row_bytes) = packed_dims(im, 3, &who)?;
    dims(im)?;

    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    for y in 0..h {
        check(env.cancel)?;
        let row_offset = y * row_bytes;
        for x in 0..w {
            let sample = x * 3;
            let alpha = alpha_at(im, y * w + x)?;
            let px = [
                decoded_image_sample(im, row_offset, sample, 0)?,
                decoded_image_sample(im, row_offset, sample + 1, 1)?,
                decoded_image_sample(im, row_offset, sample + 2, 2)?,
                alpha,
            ];
            put_pixel(&mut pix, y * w + x, px);
        }
    }
    Ok((
        png::encode(env.mem, w, h, Pixels::Nrgba(&pix), env.cancel)?,
        "png",
    ))
}

/// `unsupportedImageRender`.
fn unsupported(obj_nr: i64, detail: String) -> Error {
    Error::UnsupportedResource.context(format!("image obj#{obj_nr} render {detail}"))
}

/// `renderICCBased`: an ICC profile is not applied; the image is rendered as gray, RGB or CMYK
/// by the profile's `N`.
fn render_icc_based(
    xref: &mut XRefTable,
    env: Env<'_>,
    im: &PdfImage<'_>,
    cs: &[Object],
) -> Result<Rendered, Error> {
    // Any ICC profile >= ICC.1:2004:10 is sufficient for any PDF version <= 1.7. If the embedded
    // ICC profile version is newer than the one used by the Reader, substitute with the
    // Alternate colour space.
    let profile = cs.get(1).ok_or_else(out_of_range)?;
    let profile = dereference_required_stream_dict(xref, profile, "colorspace ICCBased profile")?;

    // 1, 3 or 4 colour components.
    let n = required_integer_entry(
        xref,
        &profile.dict,
        b"N",
        &format!("image obj#{} ICC profile", im.obj_nr),
    )?;
    if !matches!(n, 1 | 3 | 4) {
        return Err(Error::Corrupt(format!(
            "renderICCBasedToPNGFile: objNr={}, N must be 1,3 or 4, got:{n}",
            im.obj_nr
        )));
    }

    // TODO: Transform linear XYZ to RGB according to the ICC profile. For now we fall back to
    // the appropriate colour spaces for n regardless of a specified alternate colour space.

    // Validate the length. Sometimes there is a trailing 0x0A in addition to the image bytes.
    let need = multiply_i64(n, im.bpc)
        .and_then(|v| multiply_i64(v, im.w))
        .and_then(|v| multiply_i64(v, im.h))
        .map(|bits| bits.saturating_add(7) / 8);
    if need.is_ok_and(|need| len_i64(im.content) >= need) {
        match n {
            1 => render_device_gray_to_png(env, im),
            3 => render_rgb_to_png(env, im, "renderDeviceRGBToPNG"),
            _ => render_device_cmyk_to_tiff(env, im),
        }
    } else {
        Err(Error::Corrupt(format!(
            "renderICCBased: objNr={} corrupt image object",
            im.obj_nr
        )))
    }
}

/// `indexedImageIndex`: decodes a sample before rounding and clamping the palette index (8.9.5.2
/// and 8.6.6.3).
fn indexed_image_index(im: &PdfImage<'_>, sample: u16, max_ind: i64) -> usize {
    let mut f = f64::from(sample);
    if let Some(r) = im.decode.first() {
        let t = f / f64::from(max_val_for_bits(usize::try_from(im.bpc).unwrap_or(0)));
        f = (1.0 - t) * r.min + t * r.max;
    }
    if f <= 0.0 {
        return 0;
    }
    if f >= max_ind as f64 {
        return usize::try_from(max_ind).unwrap_or(0);
    }
    (f + 0.5) as usize
}

/// The loop of the Indexed renderers over the packed samples of 1, 2, 4 or 8 bits, row after
/// row, a row padded to the next byte: calls `f(pixel number, palette index)` for each pixel.
fn for_each_indexed(
    env: Env<'_>,
    im: &PdfImage<'_>,
    max_ind: i64,
    mut f: impl FnMut(usize, usize) -> Result<(), Error>,
) -> Result<(), Error> {
    let (w, h) = (to_us(im.w)?, to_us(im.h)?);
    let bpc = to_us(im.bpc)?;
    if !(1..=8).contains(&bpc) {
        return Err(out_of_range());
    }
    let per_byte = 8 / bpc;
    let mut i = 0usize;
    for y in 0..h {
        check(env.cancel)?;
        let mut x = 0usize;
        while x < w {
            let mut p = *im.content.get(i).ok_or_else(out_of_range)?;
            let mut j = 0usize;
            while j < per_byte && x < w {
                let ind = indexed_image_index(im, u16::from(p >> (8 - bpc)), max_ind);
                f(y * w + x, ind)?;
                p = p.checked_shl(bpc as u32).unwrap_or(0);
                x += 1;
                j += 1;
            }
            i += 1;
        }
    }
    Ok(())
}

/// `lookup[i]`.
fn lookup_at(lookup: &[u8], i: usize) -> Result<u8, Error> {
    lookup.get(i).copied().ok_or_else(out_of_range)
}

/// `renderIndexedGrayToPNG`.
fn render_indexed_gray_to_png(
    env: Env<'_>,
    im: &PdfImage<'_>,
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    // Validate the length. For streams not using compression there is a trailing 0x0A in
    // addition to the image bytes.
    let need = multiply_i64(im.bpc, im.w)
        .and_then(|v| multiply_i64(v, im.h))
        .map(|bits| bits.saturating_add(7) / 8);
    if !need.is_ok_and(|need| len_i64(im.content) >= need) {
        return Err(Error::Corrupt(format!(
            "renderIndexedGrayToPNG: objNr={} corrupt image object",
            im.obj_nr
        )));
    }
    let (w, h) = dims(im)?;

    // TODO support the soft mask.
    let (mut pix, _held) = new_pix(env, w, h, 1)?;
    for_each_indexed(env, im, max_ind, |i, ind| {
        if let Some(d) = pix.get_mut(i) {
            *d = lookup_at(lookup, ind)?;
        }
        Ok(())
    })?;
    Ok((
        png::encode(env.mem, w, h, Pixels::Gray(&pix), env.cancel)?,
        "png",
    ))
}

/// `renderIndexedRGBToPNG`.
fn render_indexed_rgb_to_png(
    env: Env<'_>,
    im: &PdfImage<'_>,
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    let (w, h) = dims(im)?;
    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    // TODO: For (some) Runlength encoded images the line sequence is reversed.
    for_each_indexed(env, im, max_ind, |i, ind| {
        let alpha = alpha_at(im, i)?;
        let l = 3 * ind;
        let px = [
            lookup_at(lookup, l)?,
            lookup_at(lookup, l + 1)?,
            lookup_at(lookup, l + 2)?,
            alpha,
        ];
        put_pixel(&mut pix, i, px);
        Ok(())
    })?;
    Ok((
        png::encode(env.mem, w, h, Pixels::Nrgba(&pix), env.cancel)?,
        "png",
    ))
}

/// `renderIndexedCMYKToTIFF` (with `imageForIndexedCMYKWithoutSoftMask` and
/// `imageForIndexedCMYKWithSoftMask`).
fn render_indexed_cmyk_to_tiff(
    env: Env<'_>,
    im: &PdfImage<'_>,
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    let (w, h) = dims(im)?;
    let masked = im.soft_mask.is_some();
    // Preserve the CMYK colour model for print applications.
    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    for_each_indexed(env, im, max_ind, |i, ind| {
        let l = 4 * ind;
        let (c, m, y, k) = (
            lookup_at(lookup, l)?,
            lookup_at(lookup, l + 1)?,
            lookup_at(lookup, l + 2)?,
            lookup_at(lookup, l + 3)?,
        );
        let px = if masked {
            let (r, g, b) = cmyk_to_rgb(c, m, y, k);
            [r, g, b, alpha_at(im, i)?]
        } else {
            [c, m, y, k]
        };
        put_pixel(&mut pix, i, px);
        Ok(())
    })?;
    let kind = if masked { Kind::Nrgba } else { Kind::Cmyk };
    Ok((tiff::encode(env.mem, w, h, kind, &pix)?, "tif"))
}

/// `max_ind + 1` as a size.
fn palette_len(max_ind: i64) -> usize {
    usize::try_from(max_ind.saturating_add(1)).unwrap_or(usize::MAX)
}

/// `renderIndexedNameCS`: an Indexed image over a device colour space.
fn render_indexed_name_cs(
    env: Env<'_>,
    im: &PdfImage<'_>,
    cs: &[u8],
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    let n = palette_len(max_ind);
    let short = |name: &str| {
        Error::Corrupt(format!(
            "renderIndexedNameCS: objNr={}, corrupt {name} lookup table",
            im.obj_nr
        ))
    };
    match cs {
        DEVICE_GRAY => {
            if lookup.len() < n {
                return Err(short("DeviceGray"));
            }
            render_indexed_gray_to_png(env, im, max_ind, lookup)
        }
        DEVICE_RGB => {
            if lookup.len() < n.saturating_mul(3) {
                return Err(short("DeviceRGB"));
            }
            render_indexed_rgb_to_png(env, im, max_ind, lookup)
        }
        DEVICE_CMYK => {
            if lookup.len() < n.saturating_mul(4) {
                return Err(short("DeviceCMYK"));
            }
            render_indexed_cmyk_to_tiff(env, im, max_ind, lookup)
        }
        _ => Err(unsupported(
            im.obj_nr,
            format!("indexed base colorspace {}", lossy(cs)),
        )),
    }
}

/// `imageColorSpaceName`: the name `o` is or refers to.
fn image_color_space_name(
    xref: &mut XRefTable,
    o: Option<&Object>,
    context: &str,
) -> Result<Vec<u8>, Error> {
    let o = o.ok_or_else(out_of_range)?;
    let o = xref.dereference(o).map_err(|e| e.context(context))?;
    match o {
        Object::Name(n) => Ok(n.clone()),
        other => Err(Error::Corrupt(format!(
            "{context}: expected name, got {}",
            type_name(other)
        ))),
    }
}

/// `unsupportedIndexedArrayCS`.
fn unsupported_indexed_array_cs(im: &PdfImage<'_>, csa: &[Object]) -> Error {
    unsupported(
        im.obj_nr,
        format!("indexed base colorspace {}", array_text(csa)),
    )
}

/// `renderIndexedICCGrayToPNG`.
fn render_indexed_icc_gray_to_png(
    env: Env<'_>,
    im: &PdfImage<'_>,
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    // TODO handle the soft mask.
    let (w, h) = dims(im)?;
    let row_bytes = checked_image_row_bytes(im.w, 1, im.bpc)
        .map_err(|e| e.context(format!("renderIndexedArrayCS: objNr={}", im.obj_nr)))?;
    let row_bytes = to_us(row_bytes)?;
    let bpc = to_us(im.bpc)?;
    let (mut pix, _held) = new_pix(env, w, h, 1)?;
    for y in 0..h {
        check(env.cancel)?;
        let row_offset = y * row_bytes;
        for x in 0..w {
            let sample = packed_image_sample(im.content, row_offset, x, bpc)?;
            let ind = indexed_image_index(im, sample, max_ind);
            if let Some(d) = pix.get_mut(y * w + x) {
                *d = lookup_at(lookup, ind)?;
            }
        }
    }
    Ok((
        png::encode(env.mem, w, h, Pixels::Gray(&pix), env.cancel)?,
        "png",
    ))
}

/// `renderIndexedICCBasedCS`.
fn render_indexed_icc_based_cs(
    xref: &mut XRefTable,
    env: Env<'_>,
    im: &PdfImage<'_>,
    csa: &[Object],
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    let profile = csa.get(1).ok_or_else(out_of_range)?;
    let profile = dereference_required_stream_dict(xref, profile, "indexed ICCBased profile")?;

    // 1, 3 or 4 colour components.
    let n = required_integer_entry(
        xref,
        &profile.dict,
        b"N",
        &format!("image obj#{} indexed ICC profile", im.obj_nr),
    )?;
    if !matches!(n, 1 | 3 | 4) {
        return Err(Error::Corrupt(format!(
            "renderIndexedArrayCS: objNr={}, N must be 1,3 or 4, got:{n}",
            im.obj_nr
        )));
    }
    let components = to_us(n)?;
    if lookup.len() < components.saturating_mul(palette_len(max_ind)) {
        return Err(Error::Corrupt(format!(
            "renderIndexedArrayCS: objNr={}, corrupt ICCBased lookup table",
            im.obj_nr
        )));
    }

    // TODO: Transform linear XYZ to RGB according to the ICC profile. For now we fall back to
    // the appropriate colour spaces for n regardless of a specified alternate colour space.
    match n {
        1 => render_indexed_icc_gray_to_png(env, im, max_ind, lookup),
        3 => render_indexed_rgb_to_png(env, im, max_ind, lookup),
        4 => render_indexed_cmyk_to_tiff(env, im, max_ind, lookup),
        _ => Err(unsupported_indexed_array_cs(im, csa)),
    }
}

/// `renderIndexedArrayCS`: an Indexed image over a CalRGB or ICCBased colour space.
fn render_indexed_array_cs(
    xref: &mut XRefTable,
    env: Env<'_>,
    im: &PdfImage<'_>,
    csa: &[Object],
    max_ind: i64,
    lookup: &[u8],
) -> Result<Rendered, Error> {
    let cs = image_color_space_name(xref, csa.first(), "indexed base colorspace[0]")?;
    match cs.as_slice() {
        CAL_RGB => {
            if lookup.len() < palette_len(max_ind).saturating_mul(3) {
                return Err(Error::Corrupt(format!(
                    "renderIndexedArrayCS: objNr={}, corrupt CalRGB lookup table",
                    im.obj_nr
                )));
            }
            render_indexed_rgb_to_png(env, im, max_ind, lookup)
        }
        ICC_BASED => render_indexed_icc_based_cs(xref, env, im, csa, max_ind, lookup),
        _ => Err(unsupported_indexed_array_cs(im, csa)),
    }
}

/// `validateIndexedImageSamples` (8.9.5.2).
fn validate_indexed_image_samples(im: &PdfImage<'_>, max_ind: i64) -> Result<(), Error> {
    let who = format!("renderIndexed: objNr={}", im.obj_nr);
    if !matches!(im.bpc, 1 | 2 | 4 | 8) {
        return Err(Error::Corrupt(format!(
            "{who}: unsupported bits per component {}",
            im.bpc
        )));
    }
    if !(0..=255).contains(&max_ind) {
        return Err(Error::Corrupt(format!("{who}: invalid HiVal {max_ind}")));
    }
    let row_bytes = checked_image_row_bytes(im.w, 1, im.bpc).map_err(|e| e.context(who.clone()))?;
    let image_bytes = multiply_i64(row_bytes, im.h).map_err(|e| e.context(who.clone()))?;
    if len_i64(im.content) < image_bytes {
        return Err(Error::Corrupt(format!(
            "{who}: corrupt image object: need {image_bytes} bytes, have {}",
            im.content.len()
        )));
    }
    for r in &im.decode {
        if !r.min.is_finite() || !r.max.is_finite() {
            return Err(Error::Corrupt(format!("{who}: invalid Decode range")));
        }
    }
    Ok(())
}

/// `renderIndexed` (8.6.6.3).
fn render_indexed(
    xref: &mut XRefTable,
    env: Env<'_>,
    im: &PdfImage<'_>,
    cs: &[Object],
) -> Result<Rendered, Error> {
    let nr = im.obj_nr;
    if cs.len() < 4 {
        return Err(Error::Corrupt(format!(
            "renderIndexed: objNr={nr} Indexed colorspace needs 4 entries, got {}",
            cs.len()
        )));
    }

    // Identify the base colour space; Go drops a dereference error.
    let base_cs = xref.dereference(&cs[1]).cloned().unwrap_or(Object::Null);

    // Identify the max index into the colour lookup table.
    let max_ind = match xref.dereference_integer(&cs[2]) {
        Err(e) => return Err(e.context(format!("renderIndexed: objNr={nr} HiVal"))),
        Ok(None) => {
            return Err(Error::Corrupt(format!(
                "renderIndexed: objNr={nr} missing HiVal"
            )));
        }
        Ok(Some(v)) => v,
    };
    validate_indexed_image_samples(im, max_ind)?;

    // Identify the colour lookup table.
    let Some(lookup) = color_lookup_table(xref, &cs[3], env.cancel)? else {
        return Err(Error::Corrupt(format!(
            "renderIndexed: objNr={nr} IndexedCS with corrupt lookup table {}",
            array_text(cs)
        )));
    };

    match &base_cs {
        Object::Name(n) => render_indexed_name_cs(env, im, n, max_ind, &lookup),
        Object::Array(a) => render_indexed_array_cs(xref, env, im, a, max_ind, &lookup),
        other => Err(unsupported(
            nr,
            format!("indexed base colorspace type {}", type_name(other)),
        )),
    }
}

/// `renderDeviceN`: Separation and DeviceN images render by their component count, then by their
/// alternate colour space.
fn render_device_n(
    xref: &mut XRefTable,
    env: Env<'_>,
    im: &PdfImage<'_>,
    cs: &[Object],
) -> Result<Rendered, Error> {
    match im.comp {
        1 => return render_device_gray_to_png(env, im),
        3 => return render_rgb_to_png(env, im, "renderDeviceRGBToPNG"),
        4 => return render_device_cmyk_to_tiff(env, im),
        _ => {}
    }

    let alternate = cs.get(2).ok_or_else(out_of_range)?;
    let o = xref
        .dereference(alternate)
        .map_err(|e| e.context("DeviceN alternate colorspace"))?;
    let Object::Name(alt) = o else {
        return Err(unsupported(
            im.obj_nr,
            format!("DeviceN alternate colorspace type {}", type_name(o)),
        ));
    };
    match alt.as_slice() {
        DEVICE_GRAY => render_device_gray_to_png(env, im),
        DEVICE_RGB => render_rgb_to_png(env, im, "renderDeviceRGBToPNG"),
        DEVICE_CMYK => render_device_cmyk_to_tiff(env, im),
        other => Err(unsupported(
            im.obj_nr,
            format!("DeviceN alternate colorspace {}", lossy(other)),
        )),
    }
}

/// `renderImage`: renders by the image's colour space. If the colour space is CMYK the file is
/// a TIFF, else a PNG.
fn render_by_color_space(
    xref: &mut XRefTable,
    env: Env<'_>,
    sd: &StreamDict,
    obj_nr: i64,
) -> Result<Rendered, Error> {
    let im = pdf_image(xref, sd, obj_nr, env.cancel)?;

    let o = xref
        .dereference_dict_entry(&sd.dict, b"ColorSpace")?
        .clone();

    match &o {
        Object::Name(cs) => match cs.as_slice() {
            DEVICE_GRAY => render_device_gray_to_png(env, &im),
            DEVICE_RGB => render_rgb_to_png(env, &im, "renderDeviceRGBToPNG"),
            DEVICE_CMYK => render_device_cmyk_to_tiff(env, &im),
            other => Err(unsupported(obj_nr, format!("colorspace {}", lossy(other)))),
        },
        Object::Array(cs) => {
            let csn = image_color_space_name(
                xref,
                cs.first(),
                &format!("image obj#{obj_nr} colorspace[0]"),
            )?;
            match csn.as_slice() {
                CAL_RGB => render_rgb_to_png(env, &im, "renderCalRGBToPNG"),
                DEVICE_N | SEPARATION => render_device_n(xref, env, &im, cs),
                ICC_BASED => render_icc_based(xref, env, &im, cs),
                INDEXED => render_indexed(xref, env, &im, cs),
                _ => Err(unsupported(
                    obj_nr,
                    format!("colorspace {}", array_text(cs)),
                )),
            }
        }
        other => Err(unsupported(
            obj_nr,
            format!("colorspace type {}", type_name(other)),
        )),
    }
}

/// `decodeCMYK`.
fn decode_cmyk(c: u8, m: u8, y: u8, k: u8, decode: &[ColRange]) -> Result<(u8, u8, u8, u8), Error> {
    if decode.is_empty() {
        return Ok((c, m, y, k));
    }
    let r = |i: usize| decode.get(i).copied().ok_or_else(out_of_range);
    Ok((
        decode_pixel_value(c, 8, r(0)?),
        decode_pixel_value(m, 8, r(1)?),
        decode_pixel_value(y, 8, r(2)?),
        decode_pixel_value(k, 8, r(3)?),
    ))
}

/// `img.At(x, y)` of an `image.CMYK` decoded from gob: the zero colour outside its rectangle;
/// Go's slice expression panics when `Pix` is shorter than the rectangle says.
fn cmyk_at(img: &gob::CmykImage<'_>, x: i64, y: i64) -> Result<[u8; 4], Error> {
    let r = img.rect;
    if !((r.min.x..r.max.x).contains(&x) && (r.min.y..r.max.y).contains(&y)) {
        return Ok([0; 4]);
    }
    let off = (y - r.min.y)
        .checked_mul(img.stride)
        .and_then(|v| v.checked_add((x - r.min.x).checked_mul(4)?))
        .ok_or_else(out_of_range)?;
    let off = usize::try_from(off).map_err(|_| out_of_range())?;
    match img.pix.get(off..off.saturating_add(4)) {
        Some(&[c, m, yy, k]) => Ok([c, m, yy, k]),
        _ => Err(out_of_range()),
    }
}

/// `renderCMYKToPng`: the content is the gob of an `image.CMYK` (what the DCT filter writes for
/// a CMYK JPEG); the JPEG's inverted values are put right and the pixels converted to RGB.
fn render_cmyk_to_png(env: Env<'_>, im: &PdfImage<'_>) -> Result<Rendered, Error> {
    let img = gob::decode_cmyk(im.content)?;
    let (w, h) = dims(im)?;

    let (mut pix, _held) = new_pix(env, w, h, 4)?;
    for y in 0..h {
        check(env.cancel)?;
        for x in 0..w {
            let a = cmyk_at(&img, x as i64, y as i64)?;
            let (cyan, mag, yel, blk) =
                decode_cmyk(255 - a[0], 255 - a[1], 255 - a[2], 255 - a[3], &im.decode)?;
            let (r, g, b) = cmyk_to_rgb(cyan, mag, yel, blk);
            // TODO Apply the Decode array.
            put_pixel(&mut pix, y * w + x, [r, g, b, 255]);
        }
    }
    // An opaque `image.RGBA`, which `png.Encode` writes as the equal `image.NRGBA`.
    Ok((
        png::encode(env.mem, w, h, Pixels::Nrgba(&pix), env.cancel)?,
        "png",
    ))
}

/// `renderDCTToPNG`.
fn render_dct_to_png(
    xref: &mut XRefTable,
    env: Env<'_>,
    sd: &StreamDict,
    obj_nr: i64,
) -> Result<Rendered, Error> {
    let im = pdf_image(xref, sd, obj_nr, env.cancel)?;
    render_cmyk_to_png(env, &im)
}

/// The content as it is: a DCT (not CMYK), JPX or JBIG2 stream.
fn pass_through(env: Env<'_>, sd: &StreamDict, file_type: &'static str) -> Result<Rendered, Error> {
    let content = sd.content.as_deref().unwrap_or(&[]);
    Ok((copy_charged(env.mem, content)?, file_type))
}

/// `RenderImage`: the bytes and file type of the decoded image stream `sd`. Image compression is
/// the last filter in the pipeline.
///
/// `sd.content` is the decoded content (for a stream without filters, its raw bytes) and for a
/// DCT stream `sd.cs_components` says whether it is CMYK. The memory of the pixels and of the
/// output is charged to the document's ledger while the call runs.
///
/// # Errors
/// [`Error::UnsupportedResource`] for a filter, colour space or Indexed base that is not
/// rendered, [`Error::ImageLimit`] over `max_image_pixels` or `max_image_bytes`,
/// [`Error::Corrupt`] for contents too short for the image or a sample depth that is not
/// supported, [`Error::Panic`] where Go indexes past a slice, the dereference errors of the
/// image entries, [`Error::Alloc`] and [`Error::Cancelled`].
pub(crate) fn render_image(
    xref: &mut XRefTable,
    sd: &StreamDict,
    obj_nr: i64,
    cancel: &AtomicBool,
) -> Result<Rendered, Error> {
    let mem = xref.mem().clone();
    let env = Env { mem: &mem, cancel };

    let Some(last) = sd.filter_pipeline.last() else {
        return render_by_color_space(xref, env, sd, obj_nr);
    };

    match last.name.as_slice() {
        FLATE | LZW | CCITT_FAX | RUN_LENGTH => render_by_color_space(xref, env, sd, obj_nr),
        DCT => {
            if sd.cs_components == 4 {
                render_dct_to_png(xref, env, sd, obj_nr)
            } else {
                pass_through(env, sd, "jpg")
            }
        }
        JPX => pass_through(env, sd, "jpx"),
        JBIG2 => pass_through(env, sd, "jbig2"),
        other => Err(unsupported(obj_nr, format!("filter {}", lossy(other)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::filter::gojpeg::GoImage;
    use crate::image::{png, tiff};
    use crate::model::Entry;
    use crate::object::{Dict, PdfFilter, Ref};

    fn xref() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn int(i: i64) -> Object {
        Object::Integer(i)
    }

    fn hex(b: &[u8]) -> Object {
        Object::hex_from_bytes(b)
    }

    fn numbers(a: &[f64]) -> Object {
        Object::Array(a.iter().map(|&f| Object::Float(f)).collect())
    }

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v);
        }
        d
    }

    /// A stream dictionary with `Content` set and nothing else, as the Go tests make them.
    fn stream(d: Dict, content: &[u8]) -> StreamDict {
        StreamDict {
            dict: d,
            content: Some(Bytes::new(content.to_vec(), 0, None)),
            ..StreamDict::default()
        }
    }

    fn render(x: &mut XRefTable, sd: &StreamDict) -> Result<Rendered, Error> {
        render_image(x, sd, 7, &AtomicBool::new(false))
    }

    fn mem() -> Mem {
        Mem::new(&Limits::default())
    }

    fn put(x: &mut XRefTable, num: i64, o: Object) {
        x.table.insert(num, Entry::gen0(o));
    }

    /// The pixels of a rendered image as RGBA bytes: a PNG of any colour type, or a TIFF.
    fn rgba_pixels(r: &Rendered) -> Vec<[u8; 4]> {
        match r.1 {
            "png" => {
                let d = png::tests::decode(&r.0);
                let bpp = match d.color_type {
                    0 => 1,
                    2 => 3,
                    _ => 4,
                };
                d.pixels
                    .chunks(bpp)
                    .map(|p| match bpp {
                        1 => [p[0], p[0], p[0], 255],
                        3 => [p[0], p[1], p[2], 255],
                        _ => [p[0], p[1], p[2], p[3]],
                    })
                    .collect()
            }
            "tif" => {
                let d = tiff::tests::decode(&r.0);
                d.pixels
                    .chunks(4)
                    .map(|p| {
                        if d.extra_samples.is_some() {
                            [p[0], p[1], p[2], p[3]]
                        } else {
                            let (r, g, b) = cmyk_to_rgb(p[0], p[1], p[2], p[3]);
                            [r, g, b, 255]
                        }
                    })
                    .collect()
            }
            other => panic!("file type {other}"),
        }
    }

    fn cmyk_pixels(r: &Rendered) -> Vec<[u8; 4]> {
        let d = tiff::tests::decode(&r.0);
        d.pixels
            .chunks(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect()
    }

    // TestGHSA_6524_w46v_6399_RenderDeviceGray16BPC.
    #[test]
    fn device_gray_16_bits() {
        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(16)),
                ("ColorSpace", name("DeviceGray")),
                ("Height", int(1)),
                ("Width", int(2)),
            ]),
            &[0, 0, 0xff, 0xff],
        );
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!(r.1, "png");
        assert_eq!(rgba_pixels(&r), [[0, 0, 0, 255], [255, 255, 255, 255]]);
    }

    fn icc(n: i64) -> Object {
        Object::Array(vec![
            name("ICCBased"),
            Object::Stream(Box::new(StreamDict {
                dict: dict(vec![("N", int(n))]),
                ..StreamDict::default()
            })),
        ])
    }

    fn cal_rgb() -> Object {
        Object::Array(vec![name("CalRGB"), Object::Dict(Dict::default())])
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedPaletteBounds.
    #[test]
    fn indexed_palette_index_is_clamped() {
        let bases: Vec<(&str, Object, Object)> = vec![
            ("DeviceGray", name("DeviceGray"), hex(&[0])),
            ("DeviceRGB", name("DeviceRGB"), hex(&[0; 3])),
            ("DeviceCMYK", name("DeviceCMYK"), hex(&[0; 4])),
            ("CalRGB", cal_rgb(), hex(&[0; 3])),
        ];
        for (what, base, lookup) in bases {
            for pixel in [0u8, 0xff] {
                let sd = stream(
                    dict(vec![
                        ("BitsPerComponent", int(8)),
                        (
                            "ColorSpace",
                            Object::Array(vec![
                                name("Indexed"),
                                base.clone(),
                                int(0),
                                lookup.clone(),
                            ]),
                        ),
                        ("Height", int(1)),
                        ("Width", int(1)),
                    ]),
                    &[pixel],
                );
                render(&mut xref(), &sd)
                    .unwrap_or_else(|e| panic!("{what} pixel {pixel}: clamped palette index: {e}"));
            }
        }
    }

    fn indexed_test_stream(
        base: Object,
        lookup: &[u8],
        bpc: i64,
        w: i64,
        h: i64,
        samples: &[u8],
    ) -> StreamDict {
        stream(
            dict(vec![
                ("BitsPerComponent", int(bpc)),
                (
                    "ColorSpace",
                    Object::Array(vec![name("Indexed"), base, int(1), hex(lookup)]),
                ),
                ("Height", int(h)),
                ("Width", int(w)),
            ]),
            samples,
        )
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedDecode.
    #[test]
    fn indexed_decode_mapping_rounding_and_clamping() {
        let gray = [0x00, 0xff];
        let rgb = [0, 0, 0, 0xff, 0xff, 0xff];
        let cmyk = [0, 0, 0, 0xff, 0, 0, 0, 0];
        let bases: Vec<(&str, Object, &[u8], bool)> = vec![
            ("DeviceGray", name("DeviceGray"), &gray[..], false),
            ("DeviceRGB", name("DeviceRGB"), &rgb[..], false),
            ("DeviceCMYK", name("DeviceCMYK"), &cmyk[..], false),
            ("CalRGB", cal_rgb(), &rgb[..], false),
            ("ICCGray", icc(1), &gray[..], false),
            ("ICCRGB", icc(3), &rgb[..], false),
            ("ICCCMYK", icc(4), &cmyk[..], false),
            ("DeviceRGBMasked", name("DeviceRGB"), &rgb[..], true),
            ("DeviceCMYKMasked", name("DeviceCMYK"), &cmyk[..], true),
        ];
        let tests: Vec<(&str, Option<Vec<f64>>, Vec<u8>, Vec<bool>)> = vec![
            ("default", None, vec![0, 1, 255], vec![false, true, true]),
            (
                "scaled",
                Some(vec![0.0, 1.0]),
                vec![0, 127, 128, 255],
                vec![false, false, true, true],
            ),
            ("scaled_max", Some(vec![0.0, 1.0]), vec![255], vec![true]),
            (
                "inverted",
                Some(vec![1.0, 0.0]),
                vec![0, 127, 128, 255],
                vec![true, true, false, false],
            ),
            (
                "half_up",
                Some(vec![0.0, 1.5]),
                vec![0, 85, 170, 255],
                vec![false, true, true, true],
            ),
            (
                "below_half",
                Some(vec![0.25, 0.49]),
                vec![0, 255],
                vec![false, false],
            ),
            (
                "clipped",
                Some(vec![-1.0, 2.0]),
                vec![0, 85, 170, 255],
                vec![false, false, true, true],
            ),
        ];
        for (base_name, base, lookup, masked) in &bases {
            for (test_name, decode, samples, white) in &tests {
                let mut sd =
                    indexed_test_stream(base.clone(), lookup, 8, samples.len() as i64, 1, samples);
                if let Some(d) = decode {
                    sd.dict.set(b"Decode", numbers(d));
                }
                let alpha = if *masked { 128 } else { 255 };
                if *masked {
                    let mut mask = stream(
                        dict(vec![
                            ("BitsPerComponent", int(8)),
                            ("ColorSpace", name("DeviceGray")),
                            ("Height", int(1)),
                            ("Width", int(samples.len() as i64)),
                        ]),
                        &vec![128u8; samples.len()],
                    );
                    mask.dict.set(b"ColorSpace", name("DeviceGray"));
                    sd.dict.set(b"SMask", Object::Stream(Box::new(mask)));
                }
                let r = render(&mut xref(), &sd).unwrap();
                let px = rgba_pixels(&r);
                assert_eq!(px.len(), white.len());
                for (x, &w) in white.iter().enumerate() {
                    let v = if w { 255 } else { 0 };
                    assert_eq!(px[x], [v, v, v, alpha], "{base_name}/{test_name} pixel {x}");
                }
            }
        }
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedPackedDecode.
    #[test]
    fn indexed_packed_rows() {
        let cases: [(i64, &[u8]); 4] = [
            (1, &[0x5f, 0xbf]),
            (2, &[0x33, 0xcf]),
            (4, &[0x0f, 0x0f, 0xf0, 0xff]),
            (8, &[0, 255, 0, 255, 0, 255]),
        ];
        for (bpc, samples) in cases {
            let mut sd = indexed_test_stream(name("DeviceGray"), &[0x40, 0x80], bpc, 3, 2, samples);
            sd.dict.set(b"Decode", numbers(&[0.0, 1.0]));
            let r = render(&mut xref(), &sd).unwrap();
            let want = [0x40, 0x80, 0x40, 0x80, 0x40, 0x80];
            let got: Vec<u8> = rgba_pixels(&r).iter().map(|p| p[0]).collect();
            assert_eq!(got, want, "bpc {bpc}");
        }
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedRejectsWideSamples.
    #[test]
    fn indexed_rejects_wide_samples() {
        for bpc in [9, 16] {
            let mut sd = indexed_test_stream(name("DeviceRGB"), &[0; 3], bpc, 1, 1, &[0, 0]);
            sd.dict.set(
                b"ColorSpace",
                Object::Array(vec![
                    name("Indexed"),
                    name("DeviceRGB"),
                    int(0),
                    hex(&[0; 3]),
                ]),
            );
            let e = render(&mut xref(), &sd).unwrap_err();
            assert!(
                e.to_string().contains("unsupported bits per component"),
                "{e}"
            );
        }
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedICCPackedGrayRows.
    #[test]
    fn indexed_icc_gray_packed_rows() {
        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(1)),
                (
                    "ColorSpace",
                    Object::Array(vec![name("Indexed"), icc(1), int(1), hex(&[0x00, 0xff])]),
                ),
                ("Height", int(2)),
                ("Width", int(2)),
            ]),
            &[0x40, 0x80],
        );
        let r = render(&mut xref(), &sd).unwrap();
        let got: Vec<u8> = rgba_pixels(&r).iter().map(|p| p[0]).collect();
        assert_eq!(got, [0, 255, 255, 0]);
    }

    // TestGHSA_6524_w46v_6399_RenderIndexedCalRGBRejectsShortLookup.
    #[test]
    fn indexed_cal_rgb_rejects_a_short_lookup() {
        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                (
                    "ColorSpace",
                    Object::Array(vec![name("Indexed"), cal_rgb(), int(1), hex(&[0; 3])]),
                ),
                ("Height", int(1)),
                ("Width", int(1)),
            ]),
            &[0],
        );
        let e = render(&mut xref(), &sd).unwrap_err();
        assert!(e.to_string().contains("corrupt CalRGB lookup table"), "{e}");
    }

    // TestGHSA_prvw_v9vv_6966_ImageMaskDefaultsBitsPerComponent.
    #[test]
    fn image_mask_defaults_to_one_bit() {
        let sd = stream(
            dict(vec![
                ("ColorSpace", name("DeviceGray")),
                ("Height", int(1)),
                ("ImageMask", Object::Bool(true)),
                ("Length", int(1)),
                ("Width", int(1)),
            ]),
            &[],
        );
        let im = pdf_image(&mut xref(), &sd, 7, &AtomicBool::new(false)).unwrap();
        assert_eq!((im.bpc, im.comp), (1, 1));
    }

    // TestValidatePDFImageDimensionsRejectsPixelLimit and ...ByteLimit.
    #[test]
    fn dimensions_are_checked_against_the_limits() {
        let limits = Limits {
            max_image_pixels: 9,
            max_image_bytes: 1 << 20,
            ..Limits::default()
        };
        let e = validate_pdf_image_dimensions(&limits, 5, 2, 3, 8, 1).unwrap_err();
        assert!(e.to_string().contains("pixel count"), "{e}");
        assert_eq!(e.class(), "limit/image-pixels");

        let limits = Limits {
            max_image_pixels: 100,
            max_image_bytes: 8,
            ..Limits::default()
        };
        let e = validate_pdf_image_dimensions(&limits, 2, 2, 3, 8, 1).unwrap_err();
        assert!(e.to_string().contains("byte size"), "{e}");
        assert_eq!(e.class(), "limit/image-bytes");

        let limits = Limits::default();
        assert!(validate_pdf_image_dimensions(&limits, 0, 2, 3, 8, 1).is_err());
        assert!(validate_pdf_image_dimensions(&limits, 2, 2, 0, 8, 1).is_err());
        assert!(validate_pdf_image_dimensions(&limits, 2, 2, 3, 8, 1).is_ok());
        // 4 bytes a pixel for the render: 25 pixels need 100 bytes.
        let limits = Limits {
            max_image_bytes: 99,
            ..Limits::default()
        };
        assert!(validate_pdf_image_dimensions(&limits, 5, 5, 1, 8, 1).is_err());
        assert!(validate_pdf_image_dimensions(&Limits::default(), i64::MAX, 2, 1, 1, 1).is_err());
    }

    fn mask_image() -> StreamDict {
        stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceGray")),
                ("Height", int(1)),
                ("Length", int(1)),
                ("Width", int(1)),
            ]),
            &[0],
        )
    }

    // TestPDFImageResolvesIndirectImageMask.
    #[test]
    fn pdf_image_resolves_an_indirect_image_mask() {
        let mut x = xref();
        put(&mut x, 9, Object::Bool(true));
        for value in [
            Object::Bool(true),
            Object::Ref(Ref {
                num: 9,
                generation: 0,
            }),
        ] {
            let mut sd = mask_image();
            sd.dict.set(b"ImageMask", value);
            // An image mask is one component at one bit, whatever the dictionary says.
            let im = pdf_image(&mut x, &sd, 7, &AtomicBool::new(false)).unwrap();
            assert_eq!(im.comp, 1);
        }
    }

    // TestPDFImageResolvesIndirectIntegerEntries.
    #[test]
    fn pdf_image_resolves_indirect_integers() {
        let mut x = xref();
        put(&mut x, 10, int(1));
        put(&mut x, 11, int(8));
        let r = |num| Object::Ref(Ref { num, generation: 0 });
        let mut sd = mask_image();
        sd.dict.set(b"Width", r(10));
        sd.dict.set(b"Height", r(10));
        sd.dict.set(b"BitsPerComponent", r(11));
        let im = pdf_image(&mut x, &sd, 7, &AtomicBool::new(false)).unwrap();
        assert_eq!((im.w, im.h, im.bpc), (1, 1, 8));
    }

    #[test]
    fn decode_array_pairs_and_errors() {
        let mut x = xref();
        let a = Object::Array(vec![int(1), Object::Float(0.5), int(3)]);
        let d = decode_arr(&mut x, Some(&a), 7).unwrap();
        assert_eq!(d, [ColRange { min: 1.0, max: 0.5 }]);
        assert!(decode_arr(&mut x, None, 7).unwrap().is_empty());
        let bad = Object::Array(vec![int(0), name("x")]);
        let e = decode_arr(&mut x, Some(&bad), 7).unwrap_err();
        assert!(e.to_string().contains("image obj#7 Decode[1]"), "{e}");
        let e = decode_arr(&mut x, Some(&int(1)), 7).unwrap_err();
        assert!(e.to_string().contains("image obj#7 Decode"), "{e}");
    }

    // TestRenderImagePreservesJBIG2Stream.
    #[test]
    fn jbig2_is_preserved() {
        let content = [0x97, 0x4a, 0x42, 0x32];
        let mut sd = stream(Dict::default(), &content);
        sd.filter_pipeline = vec![PdfFilter {
            name: JBIG2.to_vec(),
            decode_parms: None,
        }];
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!((r.1, r.0.as_slice()), ("jbig2", &content[..]));
    }

    #[test]
    fn dct_that_is_not_cmyk_is_a_jpg() {
        let mut sd = stream(Dict::default(), &[0xff, 0xd8, 0xff]);
        sd.filter_pipeline = vec![PdfFilter {
            name: DCT.to_vec(),
            decode_parms: None,
        }];
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!((r.1, r.0.as_slice()), ("jpg", &[0xff, 0xd8, 0xff][..]));
    }

    fn cmyk_test_stream(bpc: i64, w: i64, h: i64, samples: &[u8]) -> StreamDict {
        stream(
            dict(vec![
                ("ColorSpace", name("DeviceCMYK")),
                ("BitsPerComponent", int(bpc)),
                ("Width", int(w)),
                ("Height", int(h)),
            ]),
            samples,
        )
    }

    // TestRenderDeviceCMYKRejectsShortSoftMaskedImage.
    #[test]
    fn cmyk_rejects_a_short_soft_masked_image() {
        let im = PdfImage {
            obj_nr: 7,
            content: &[0],
            comp: 4,
            bpc: 8,
            w: 1,
            h: 1,
            soft_mask: Some(Bytes::new(vec![255], 0, None)),
            decode: Vec::new(),
        };
        let m = mem();
        let cancel = AtomicBool::new(false);
        let e = render_device_cmyk_to_tiff(
            Env {
                mem: &m,
                cancel: &cancel,
            },
            &im,
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("image obj#7 CMYK: corrupt image object"),
            "{e}"
        );
    }

    // TestRenderCMYKSampleDepths.
    #[test]
    fn cmyk_sample_depths_with_and_without_a_mask() {
        type Case = (i64, &'static [u8], [[u8; 4]; 2]);
        let cases: [Case; 5] = [
            (1, &[0xcf, 0x3a], [[255, 255, 0, 0], [0, 0, 255, 255]]),
            (2, &[0xe4, 0x1b], [[255, 170, 85, 0], [0, 85, 170, 255]]),
            (
                4,
                &[0xf8, 0x40, 0x04, 0x8f],
                [[255, 136, 68, 0], [0, 68, 136, 255]],
            ),
            (
                8,
                &[255, 128, 64, 0, 0, 64, 128, 255],
                [[255, 128, 64, 0], [0, 64, 128, 255]],
            ),
            (
                16,
                &[255, 255, 128, 0, 64, 0, 0, 0, 0, 0, 64, 0, 128, 0, 255, 255],
                [[255, 128, 64, 0], [0, 64, 128, 255]],
            ),
        ];
        for (bpc, samples, pixels) in cases {
            for masked in [false, true] {
                let mut sd = cmyk_test_stream(bpc, 1, 2, samples);
                if masked {
                    let mask = cmyk_test_stream(8, 1, 2, &[255, 128]);
                    let mut mask = mask;
                    mask.dict.set(b"ColorSpace", name("DeviceGray"));
                    sd.dict.set(b"SMask", Object::Stream(Box::new(mask)));
                }
                let r = render(&mut xref(), &sd).unwrap();
                assert_eq!(r.1, "tif");
                let got = if masked {
                    rgba_pixels(&r)
                } else {
                    cmyk_pixels(&r)
                };
                for (y, want) in pixels.iter().enumerate() {
                    if masked {
                        let (cr, cg, cb) = cmyk_to_rgb(want[0], want[1], want[2], want[3]);
                        assert_eq!(got[y], [cr, cg, cb, [255, 128][y]], "bpc {bpc} row {y}");
                    } else {
                        assert_eq!(&got[y], want, "bpc {bpc} row {y}");
                    }
                }
            }
        }
    }

    // TestRenderCMYKRejectsInvalidSamples.
    #[test]
    fn cmyk_rejects_truncated_data_and_bad_depths() {
        for bpc in [1, 2, 4, 8, 16] {
            let row_bytes = (3 * 4 * bpc as usize).div_ceil(8);
            let sd = cmyk_test_stream(bpc, 3, 2, &vec![0; 2 * row_bytes - 1]);
            assert!(render(&mut xref(), &sd).is_err(), "short bpc {bpc}");
            let sd = cmyk_test_stream(bpc, 3, 2, &vec![0; 2 * row_bytes]);
            assert!(render(&mut xref(), &sd).is_ok(), "full bpc {bpc}");
        }
        for bpc in [0, 3, 32] {
            let sd = cmyk_test_stream(bpc, 1, 1, &[0; 16]);
            assert!(render(&mut xref(), &sd).is_err(), "unsupported bpc {bpc}");
        }
    }

    // TestRenderCMYKComponentStride.
    #[test]
    fn cmyk_component_stride() {
        let content = [255, 128, 64, 0, 99, 0, 64, 128, 0, 77];
        let m = mem();
        let cancel = AtomicBool::new(false);
        let env = Env {
            mem: &m,
            cancel: &cancel,
        };
        for masked in [false, true] {
            let mut im = PdfImage {
                obj_nr: 7,
                content: &content,
                comp: 5,
                bpc: 8,
                w: 2,
                h: 1,
                soft_mask: masked.then(|| Bytes::new(vec![255, 128], 0, None)),
                decode: Vec::new(),
            };
            let r = render_device_cmyk_to_tiff(env, &im).unwrap();
            let got = if masked {
                rgba_pixels(&r)
            } else {
                cmyk_pixels(&r)
            };
            let want = [[255, 128, 64, 0], [0, 64, 128, 0]];
            for (i, w) in want.iter().enumerate() {
                if masked {
                    let (cr, cg, cb) = cmyk_to_rgb(w[0], w[1], w[2], w[3]);
                    assert_eq!(got[i], [cr, cg, cb, [255, 128][i]]);
                } else {
                    assert_eq!(&got[i], w);
                }
            }
            im.comp = 3;
            assert!(render_device_cmyk_to_tiff(env, &im).is_err());
        }
    }

    // TestRenderIndexedRejectsMalformedColorSpaces.
    #[test]
    fn indexed_rejects_malformed_colour_spaces() {
        let rgb = || name("DeviceRGB");
        let cases: Vec<(&str, Vec<Object>)> = vec![
            ("empty", vec![]),
            ("missing base", vec![name("Indexed")]),
            ("missing hival and lookup", vec![name("Indexed"), rgb()]),
            ("missing lookup", vec![name("Indexed"), rgb(), int(0)]),
            (
                "empty base",
                vec![name("Indexed"), Object::Array(vec![]), int(0), hex(&[0; 3])],
            ),
            (
                "missing ICC profile",
                vec![
                    name("Indexed"),
                    Object::Array(vec![name("ICCBased")]),
                    int(0),
                    hex(&[0; 3]),
                ],
            ),
        ];
        for (what, cs) in cases {
            let sd = stream(
                dict(vec![
                    ("ColorSpace", Object::Array(cs)),
                    ("BitsPerComponent", int(8)),
                    ("Width", int(1)),
                    ("Height", int(1)),
                ]),
                &[0],
            );
            assert!(render(&mut xref(), &sd).is_err(), "{what}");
        }
    }

    fn im(content: &[u8], comp: i64) -> PdfImage<'_> {
        PdfImage {
            obj_nr: 7,
            content,
            comp,
            bpc: 8,
            w: 0,
            h: 0,
            soft_mask: None,
            decode: Vec::new(),
        }
    }

    fn assert_unsupported(e: &Error, want: &str) {
        assert_eq!(e.root(), &Error::UnsupportedResource, "{e}");
        let s = e.to_string();
        assert!(s.contains("image obj#7 render"), "{s}");
        assert!(s.contains(want), "{s} should contain {want}");
    }

    // TestUnsupportedImageRenderingReturnsSentinel.
    #[test]
    fn unsupported_rendering_returns_the_sentinel() {
        let mut x = xref();
        let m = mem();
        let cancel = AtomicBool::new(false);
        let env = Env {
            mem: &m,
            cancel: &cancel,
        };
        let image = im(&[], 5);

        let e = render_indexed_name_cs(env, &image, b"Lab", 0, &[]).unwrap_err();
        assert_unsupported(&e, "indexed base colorspace Lab");

        let e = render_indexed_array_cs(&mut x, env, &image, &[name("Lab")], 0, &[]).unwrap_err();
        assert_unsupported(&e, "indexed base colorspace [Lab]");

        let cs = [name("Indexed"), int(1), int(0), Object::string("x")];
        let e = render_indexed(&mut x, env, &image, &cs).unwrap_err();
        assert_unsupported(&e, "indexed base colorspace type types.Integer");

        let cs = [name("DeviceN"), Object::Array(vec![]), int(1)];
        let e = render_device_n(&mut x, env, &image, &cs).unwrap_err();
        assert_unsupported(&e, "DeviceN alternate colorspace type types.Integer");

        let cs = [name("DeviceN"), Object::Array(vec![]), name("Lab")];
        let e = render_device_n(&mut x, env, &image, &cs).unwrap_err();
        assert_unsupported(&e, "DeviceN alternate colorspace Lab");

        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                (
                    "ColorSpace",
                    Object::Array(vec![name("CalGray"), Object::Dict(Dict::default())]),
                ),
                ("Height", int(1)),
                ("Width", int(1)),
            ]),
            &[0],
        );
        let e = render(&mut x, &sd).unwrap_err();
        assert_unsupported(&e, "colorspace [CalGray");

        let sd = StreamDict {
            filter_pipeline: vec![PdfFilter {
                name: b"Unsupported".to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        };
        let e = render(&mut x, &sd).unwrap_err();
        assert_unsupported(&e, "filter Unsupported");
    }

    #[test]
    fn device_n_and_separation_render_by_component_count() {
        for (cs, comp_samples, want) in [
            (
                Object::Array(vec![name("Separation"), name("Spot"), name("DeviceGray")]),
                vec![0x80u8],
                [0x80, 0x80, 0x80, 255],
            ),
            (
                Object::Array(vec![
                    name("DeviceN"),
                    Object::Array(vec![name("A"), name("B"), name("C")]),
                    name("DeviceRGB"),
                ]),
                vec![1, 2, 3],
                [1, 2, 3, 255],
            ),
        ] {
            let sd = stream(
                dict(vec![
                    ("BitsPerComponent", int(8)),
                    ("ColorSpace", cs),
                    ("Height", int(1)),
                    ("Width", int(1)),
                ]),
                &comp_samples,
            );
            let r = render(&mut xref(), &sd).unwrap();
            assert_eq!(rgba_pixels(&r), [want]);
        }
    }

    #[test]
    fn icc_based_renders_by_n_and_checks_the_length() {
        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", icc(3)),
                ("Height", int(1)),
                ("Width", int(2)),
            ]),
            &[1, 2, 3, 4, 5, 6],
        );
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!(rgba_pixels(&r), [[1, 2, 3, 255], [4, 5, 6, 255]]);

        let sd = stream(sd.dict.clone(), &[1, 2, 3, 4, 5]);
        let e = render(&mut xref(), &sd).unwrap_err();
        assert!(e.to_string().contains("corrupt image object"), "{e}");

        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", icc(2)),
                ("Height", int(1)),
                ("Width", int(1)),
            ]),
            &[1, 2],
        );
        let e = render(&mut xref(), &sd).unwrap_err();
        assert!(e.to_string().contains("N must be 1,3 or 4"), "{e}");
    }

    #[test]
    fn rgb_applies_the_decode_array_and_a_soft_mask() {
        let mut sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceRGB")),
                ("Height", int(1)),
                ("Width", int(1)),
                ("Decode", numbers(&[1.0, 0.0, 0.0, 1.0, 0.0, 0.5])),
            ]),
            &[0, 255, 255],
        );
        let mask = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceGray")),
                ("Height", int(1)),
                ("Width", int(1)),
            ]),
            &[77],
        );
        sd.dict.set(b"SMask", Object::Stream(Box::new(mask)));
        let r = render(&mut xref(), &sd).unwrap();
        // R inverted: 1.0 -> 255; G as it is: 1.0 -> 255; B over 0..0.5: 0.5 -> 127.
        assert_eq!(rgba_pixels(&r), [[255, 255, 127, 77]]);
    }

    #[test]
    fn a_soft_mask_that_does_not_fit_is_ignored() {
        for (bpc, content) in [(8, vec![1u8, 2, 3]), (4, vec![1u8])] {
            let mut sd = stream(
                dict(vec![
                    ("BitsPerComponent", int(8)),
                    ("ColorSpace", name("DeviceGray")),
                    ("Height", int(1)),
                    ("Width", int(1)),
                ]),
                &[9],
            );
            let mask = stream(
                dict(vec![
                    ("BitsPerComponent", int(bpc)),
                    ("ColorSpace", name("DeviceGray")),
                    ("Height", int(1)),
                    ("Width", int(1)),
                ]),
                &content,
            );
            sd.dict.set(b"SMask", Object::Stream(Box::new(mask)));
            let r = render(&mut xref(), &sd).unwrap();
            assert_eq!(rgba_pixels(&r), [[9, 9, 9, 255]], "mask bpc {bpc}");
        }
    }

    #[test]
    fn go_float_to_byte_conversion() {
        assert_eq!(go_u8(0.0), 0);
        assert_eq!(go_u8(254.99), 254);
        assert_eq!(go_u8(255.0), 255);
        // Out of range: through an int32, cut to 8 bits.
        assert_eq!(go_u8(256.0), 0);
        assert_eq!(go_u8(300.5), 44);
        assert_eq!(go_u8(-1.0), 255);
        assert_eq!(go_u8(f64::NAN), 0);
        assert_eq!(go_u8(1e30), 0);
    }

    #[test]
    fn cmyk_to_rgb_matches_go() {
        assert_eq!(cmyk_to_rgb(0, 0, 0, 0), (255, 255, 255));
        assert_eq!(cmyk_to_rgb(0, 0, 0, 255), (0, 0, 0));
        assert_eq!(cmyk_to_rgb(255, 0, 0, 0), (0, 255, 255));
        assert_eq!(cmyk_to_rgb(128, 64, 32, 16), (119, 179, 209));
    }

    fn gob_content(img: &GoImage) -> Vec<u8> {
        crate::filter::gob::encode_image(img)
            .unwrap()
            .iter()
            .flat_map(|m| m.to_vec().unwrap())
            .collect()
    }

    // The CMYK DCT path: the gob of an image.CMYK is rendered as an RGB PNG.
    #[test]
    fn cmyk_dct_content_renders_as_png() {
        let img = GoImage::Cmyk(jpeg::Cmyk {
            // The JPEG's values are inverted: 255 - a gives 0 (no ink) and 255 (all ink).
            pix: vec![255, 255, 255, 255, 0, 0, 0, 0],
            stride: 8,
            rect: jpeg::Rect::new(0, 0, 2, 1),
        });
        let mut sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceCMYK")),
                ("Height", int(1)),
                ("Width", int(2)),
            ]),
            &gob_content(&img),
        );
        sd.filter_pipeline = vec![PdfFilter {
            name: DCT.to_vec(),
            decode_parms: None,
        }];
        sd.cs_components = 4;
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!(r.1, "png");
        assert_eq!(rgba_pixels(&r), [[255, 255, 255, 255], [0, 0, 0, 255]]);

        // A Decode array with fewer than four ranges panics in Go.
        sd.dict.set(b"Decode", numbers(&[0.0, 1.0]));
        let e = render(&mut xref(), &sd).unwrap_err();
        assert!(matches!(e.root(), Error::Panic(_)), "{e}");
    }

    #[test]
    fn cmyk_dct_content_outside_its_rectangle_is_zero_and_short_pix_panics() {
        // A 1x1 image rendered into 2x1: the second pixel is the zero colour, inverted to 255.
        let img = GoImage::Cmyk(jpeg::Cmyk {
            pix: vec![255, 255, 255, 255],
            stride: 4,
            rect: jpeg::Rect::new(0, 0, 1, 1),
        });
        let mut sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceCMYK")),
                ("Height", int(1)),
                ("Width", int(2)),
            ]),
            &gob_content(&img),
        );
        sd.filter_pipeline = vec![PdfFilter {
            name: DCT.to_vec(),
            decode_parms: None,
        }];
        sd.cs_components = 4;
        let r = render(&mut xref(), &sd).unwrap();
        assert_eq!(rgba_pixels(&r), [[255, 255, 255, 255], [0, 0, 0, 255]]);

        // A Gray image decodes into the CMYK with a Pix that is too short for its Rect.
        let gray = GoImage::Gray(jpeg::Gray {
            pix: vec![10, 20],
            stride: 2,
            rect: jpeg::Rect::new(0, 0, 2, 1),
        });
        sd.content = Some(Bytes::new(gob_content(&gray), 0, None));
        let e = render(&mut xref(), &sd).unwrap_err();
        assert!(matches!(e.root(), Error::Panic(_)), "{e}");
        // Content that is no gob.
        sd.content = Some(Bytes::new(vec![1, 2, 3], 0, None));
        assert!(render(&mut xref(), &sd).is_err());
    }

    #[test]
    fn rendering_is_charged_and_cancellable() {
        let sd = stream(
            dict(vec![
                ("BitsPerComponent", int(8)),
                ("ColorSpace", name("DeviceGray")),
                ("Height", int(100)),
                ("Width", int(100)),
            ]),
            &vec![7u8; 10_000],
        );
        let e = render_image(&mut xref(), &sd, 7, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e, Error::Cancelled);

        let mut x = XRefTable::new(Config {
            limits: Limits {
                max_alloc: 100_000,
                ..Limits::default()
            },
            ..Config::default()
        });
        let e = render_image(&mut x, &sd, 7, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e.root(), Error::Alloc { .. }), "{e}");
        assert_eq!(x.mem().used(), 0);

        let mut x = xref();
        render_image(&mut x, &sd, 7, &AtomicBool::new(false)).unwrap();
        assert_eq!(x.mem().used(), 0);
    }
}
