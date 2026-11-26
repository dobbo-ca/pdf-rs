//! DCTDecode (Go: pkg/filter/dctDecode.go). pdf-go reaches it only for a CMYK image: it decodes
//! the JPEG with `image/jpeg` and gob-encodes the image into the stream's `Content`
//! (docs/design.md section 1, row 3). The decode is [`gojpeg`] (a port of Go's `image/jpeg`
//! decoder) and the encode is [`gob`]. The decode limit is checked from the header alone, before
//! any decode, as in Go.
//!
//! `DecodeLength` ignores `maxLen` except for the limit check: a JPEG cannot be decoded to a
//! prefix.

use super::{Decoded, Filter, Out, gob, gojpeg};
use crate::error::Error;
use crate::gocap::Buffer;
use crate::ledger::Mem;
use std::sync::atomic::AtomicBool;

fn overflow() -> Error {
    Error::Corrupt("integer overflow".into())
}

/// `dctDecode.DecodeLength`.
pub(super) fn decode_length(
    f: &Filter,
    input: &[u8],
    max_len: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Decoded, Error> {
    f.check_decode_length(max_len)?;
    let cfg = gojpeg::decode_config(input)?;
    // The size of the decoded image as 4 bytes a pixel, checked before the decode.
    let pixels = cfg.width.checked_mul(cfg.height).ok_or_else(overflow)?;
    let image_bytes = pixels.checked_mul(4).ok_or_else(overflow)?;
    let limit = f.decode_limit(-1);
    if limit >= 0 && !matches!(i64::try_from(image_bytes), Ok(b) if b <= limit) {
        return Err(Error::DecodeLimit);
    }

    let dec = gojpeg::decode(input, mem, cancel)?;

    // `gob.NewEncoder(w).Encode(im)`, `w` the buffer behind a `decodeLimitWriter` if there is a
    // limit. The encoder makes one `Write` per message; a message longer than what is left of
    // the limit fails, and the failed message writes nothing.
    let msgs = gob::encode_image(&dec.image)?;
    let mut out = Out::new(mem)?;
    let mut buf = Buffer::new();
    let mut remaining = limit;
    for m in &msgs {
        let n = m.len();
        let len = i64::try_from(n).unwrap_or(i64::MAX);
        if limit >= 0 {
            if len > remaining {
                return Err(Error::DecodeLimit);
            }
            remaining -= len;
        }
        for p in m.parts() {
            out.extend(p)?;
        }
        buf.write(n)?;
    }
    Ok(out.finish(buf.cap(), false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::gob::{Point, decode_cmyk};
    use crate::filter::tests::mem;
    use crate::filter::{DCT, Parms, new_filter};

    /// `jpeg.Encode` of a 2x2 RGBA image.
    fn rgba_jpeg() -> Vec<u8> {
        let img = jpeg::Rgba {
            pix: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 9, 9, 9, 255],
            stride: 8,
            rect: jpeg::Rect::new(0, 0, 2, 2),
        };
        jpeg::encode(
            jpeg::EncodeImage::Rgba(&img),
            &jpeg::EncodeOptions::default(),
            &jpeg::Limits::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn decode(data: &[u8], limit: Option<i64>, max_len: i64) -> Result<Decoded, Error> {
        let cancel = AtomicBool::new(false);
        let f = new_filter(DCT, Parms::new(), limit).unwrap();
        f.decode_length(data, max_len, &mem(), &cancel)
    }

    #[test]
    fn decode_length_uses_the_configured_limit() {
        // TestDCTDecodeLengthUsesConfiguredLimit: a prefix does not shorten the decode.
        let data = rgba_jpeg();
        let d = decode(&data, Some(1024), 1).unwrap();
        assert!(d.len() > 1, "{}", d.len());
        let e = decode(&data, Some(8), 1).unwrap_err();
        assert_eq!(e, Error::DecodeLimit);
    }

    #[test]
    fn bounds_the_serialized_output() {
        // TestDCTDecodeBoundsSerializedOutput: the limit counts the gob messages.
        let data = rgba_jpeg();
        let want = decode(&data, Some(-1), -1).unwrap();
        let want = want.as_slice().to_vec();
        for limit in [16, want.len() as i64 - 1, want.len() as i64] {
            let r = decode(&data, Some(limit), 1);
            if limit < want.len() as i64 {
                assert_eq!(r.unwrap_err(), Error::DecodeLimit, "limit {limit}");
            } else {
                assert_eq!(r.unwrap().as_slice(), &want[..], "limit {limit}");
            }
        }
    }

    #[test]
    fn output_is_a_gob_image_with_the_go_capacity() {
        let data = rgba_jpeg();
        let d = decode(&data, None, -1).unwrap();
        assert!(!d.is_nil());
        // One Write per message on the Write-path capacity model.
        let mut b = Buffer::new();
        let dec = gojpeg::decode(&data, &mem(), &AtomicBool::new(false)).unwrap();
        let msgs = gob::encode_image(&dec.image).unwrap();
        for m in &msgs {
            b.write(m.len()).unwrap();
        }
        assert_eq!((d.len(), d.go_cap()), (b.len(), b.cap()));
        // A 2x2 YCbCr image decoded into an image.CMYK keeps only its Rect.
        let img = decode_cmyk(d.as_slice()).unwrap();
        assert_eq!(img.rect.min, Point { x: 0, y: 0 });
        assert_eq!(img.rect.max, Point { x: 2, y: 2 });
        assert!(img.pix.is_empty());
    }

    #[test]
    fn gray_image_decodes_into_cmyk_with_its_pixels() {
        let img = jpeg::Gray {
            pix: vec![10, 20, 30, 40],
            stride: 2,
            rect: jpeg::Rect::new(0, 0, 2, 2),
        };
        let data = jpeg::encode(
            jpeg::EncodeImage::Gray(&img),
            &jpeg::EncodeOptions::default(),
            &jpeg::Limits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let d = decode(&data, None, -1).unwrap();
        let c = decode_cmyk(d.as_slice()).unwrap();
        assert_eq!(c.rect.max, Point { x: 2, y: 2 });
        assert!(!c.pix.is_empty());
    }

    #[test]
    fn not_a_jpeg_is_a_format_error() {
        let e = decode(b"not a jpeg", None, -1).unwrap_err();
        assert_eq!(e.class(), "jpeg/format");
    }

    #[test]
    fn cmyk_limit_is_checked_from_the_header() {
        // SOI, a 4-component 100x100 SOF0, and the first two bytes of an SOS: DecodeConfig stops
        // there, and 100 * 100 * 4 bytes is over a limit of 100 before any decode.
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x14, 8, 0, 100, 0, 100, 4];
        data.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0, 4, 0x22, 0]);
        data.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08]);
        assert_eq!(
            gojpeg::decode_config(&data).unwrap().model,
            gojpeg::ColorModel::Cmyk
        );
        assert_eq!(
            decode(&data, Some(100), -1).unwrap_err(),
            Error::DecodeLimit
        );
        // At the limit the decode starts, and stops at the header's end.
        let e = decode(&data, Some(40_000), -1).unwrap_err();
        assert_ne!(e, Error::DecodeLimit);
    }

    #[test]
    fn cancel_is_honoured() {
        let data = rgba_jpeg();
        let cancel = AtomicBool::new(true);
        let f = new_filter(DCT, Parms::new(), None).unwrap();
        let e = f.decode_length(&data, -1, &mem(), &cancel).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }
}
