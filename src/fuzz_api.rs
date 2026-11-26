//! Entry points that only the fuzz crate (`tests/fuzz`) calls. They are public so a separate crate
//! can reach crate-private items, they are hidden from the documentation, and they are not part
//! of the API: they can change or go without notice.

use crate::error::Error;
use crate::filter::gob::{CmykImage, Message};
use crate::filter::gojpeg::GoImage;
use crate::image::png::{Pixels, encode};
use crate::ledger::Mem;
use crate::model::resources::PageResourceNames;
use crate::object::Object;
use crate::parse::ParseCtx;
use std::sync::atomic::AtomicBool;

/// The PNG writer (`png.Encode` at the default level) on `w` by `h` pixels: 8-bit gray, or
/// NRGBA when `rgba` is set. `pixels` is `w * h` bytes or `w * h * 4` bytes.
///
/// # Errors
/// As the PNG writer: [`Error::Corrupt`] for a size or a buffer it refuses, [`Error::Alloc`],
/// [`Error::OutOfMemory`] and [`Error::Cancelled`].
#[doc(hidden)]
pub fn png_encode(
    mem: &Mem,
    w: usize,
    h: usize,
    pixels: &[u8],
    rgba: bool,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    let pix = if rgba {
        Pixels::Nrgba(pixels)
    } else {
        Pixels::Gray(pixels)
    };
    encode(mem, w, h, pix, cancel)
}

/// `ParseObject` (`parse::parse_object`), with no ledger: the caller bounds the input.
///
/// # Errors
/// As `parse::parse_object`.
#[doc(hidden)]
pub fn parse_object(
    cx: &mut ParseCtx<'_>,
    line: &mut &[u8],
    level: u64,
    max_depth: Option<u64>,
) -> Result<Object, Error> {
    crate::parse::parse_object(cx, line, level, max_depth)
}

/// `parseContent` (`parse::content::parse_content`), with no ledger.
///
/// # Errors
/// As `parse::content::parse_content`.
#[doc(hidden)]
pub fn parse_content(s: &[u8], relaxed: bool, pdf20: bool) -> Result<PageResourceNames, Error> {
    let mem = Mem::with_max(u64::MAX);
    crate::parse::content::parse_content(
        s,
        relaxed,
        pdf20,
        &mem,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map(|(prn, _)| prn)
}

/// The gob encoding of a decoded JPEG (`filter::gob::encode_image`).
///
/// # Errors
/// As `filter::gob::encode_image`.
#[doc(hidden)]
pub fn gob_encode_image(img: &GoImage) -> Result<Vec<Message<'_>>, Error> {
    crate::filter::gob::encode_image(img)
}

/// The gob decoding of an `image.CMYK` (`filter::gob::decode_cmyk`).
///
/// # Errors
/// As `filter::gob::decode_cmyk`.
#[doc(hidden)]
pub fn gob_decode_cmyk(data: &[u8]) -> Result<CmykImage<'_>, Error> {
    crate::filter::gob::decode_cmyk(data)
}
