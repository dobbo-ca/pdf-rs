//! The part of `github.com/hhrutter/tiff` v1.0.7 (writer.go) that pdf-go reaches: `tiff.Encode`
//! with nil options (no compression, no predictor) on an `image.CMYK` or an `image.NRGBA`, both
//! 8 bits per channel, four bytes per pixel. The file is little-endian: the 8-byte header, the
//! pixels as one strip, then the image file directory and its pointer area, as the Go writer
//! lays them out.
// Copyright 2012 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use crate::error::Error;
use crate::ledger::Mem;

const LE_HEADER: &[u8; 4] = b"II\x2A\x00";
/// The length of one IFD entry in bytes.
const IFD_LEN: u64 = 12;

const DT_SHORT: u16 = 3;
const DT_LONG: u16 = 4;
const DT_RATIONAL: u16 = 5;

const T_IMAGE_WIDTH: u16 = 256;
const T_IMAGE_LENGTH: u16 = 257;
const T_BITS_PER_SAMPLE: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC_INTERPRETATION: u16 = 262;
const T_STRIP_OFFSETS: u16 = 273;
const T_SAMPLES_PER_PIXEL: u16 = 277;
const T_ROWS_PER_STRIP: u16 = 278;
const T_STRIP_BYTE_COUNTS: u16 = 279;
const T_X_RESOLUTION: u16 = 282;
const T_Y_RESOLUTION: u16 = 283;
const T_RESOLUTION_UNIT: u16 = 296;
const T_EXTRA_SAMPLES: u16 = 338;

const C_NONE: u32 = 1;
const P_RGB: u32 = 2;
const P_CMYK: u32 = 5;
const RES_PER_INCH: u32 = 2;

/// The pixel format: `image.CMYK` or `image.NRGBA`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Cmyk,
    Nrgba,
}

/// `ifdEntry`. A rational is two values.
struct IfdEntry {
    tag: u16,
    datatype: u16,
    data: Vec<u32>,
}

fn too_large() -> Error {
    Error::Corrupt("tiff: file too large for classic TIFF offsets".into())
}

/// `checkClassicTIFFOffset`.
fn check_offset(offset: u64) -> Result<(), Error> {
    if offset > u64::from(u32::MAX) {
        Err(too_large())
    } else {
        Ok(())
    }
}

impl IfdEntry {
    /// The size in bytes of one value of the data type (`lengths`).
    fn value_len(&self) -> u64 {
        match self.datatype {
            DT_SHORT => 2,
            DT_LONG => 4,
            _ => 8,
        }
    }

    /// `ifdEntrySize`: the count and the number of data bytes.
    fn size(&self) -> Result<(u32, u64), Error> {
        let mut count = self.data.len() as u64;
        if self.datatype == DT_RATIONAL {
            if !self.data.len().is_multiple_of(2) {
                return Err(Error::Corrupt("tiff: invalid rational IFD data".into()));
            }
            count /= 2;
        }
        let count32 =
            u32::try_from(count).map_err(|_| Error::Corrupt("tiff: too many IFD values".into()))?;
        Ok((count32, count * self.value_len()))
    }

    /// `putData`.
    fn put_data(&self, out: &mut Vec<u8>) {
        for &d in &self.data {
            match self.datatype {
                DT_SHORT => out.extend_from_slice(&(d as u16).to_le_bytes()),
                _ => out.extend_from_slice(&d.to_le_bytes()),
            }
        }
    }
}

/// `marshalIFD`: the entries (in ascending tag order, as the caller builds them), the offset of
/// the next IFD, and the pointer area.
fn marshal_ifd(ifd_offset: u32, next_ifd_offset: u32, d: &[IfdEntry]) -> Result<Vec<u8>, Error> {
    if d.len() > usize::from(u16::MAX) {
        return Err(Error::Corrupt("tiff: too many IFD entries".into()));
    }
    let pstart = u64::from(ifd_offset) + IFD_LEN * d.len() as u64 + 6;
    check_offset(pstart)?;

    let mut out: Vec<u8> = Vec::new();
    let mut parea: Vec<u8> = Vec::new();
    out.extend_from_slice(&(d.len() as u16).to_le_bytes());
    for ent in d {
        let (count, datalen) = ent.size()?;
        out.extend_from_slice(&ent.tag.to_le_bytes());
        out.extend_from_slice(&ent.datatype.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if datalen <= 4 {
            let start = out.len();
            ent.put_data(&mut out);
            out.resize(start + 4, 0);
            continue;
        }
        let pointer = pstart + parea.len() as u64;
        if check_offset(pointer).is_err() || datalen > u64::from(u32::MAX) - pointer {
            return Err(too_large());
        }
        ent.put_data(&mut parea);
        out.extend_from_slice(&(pointer as u32).to_le_bytes());
    }
    // The IFD ends with the offset of the next IFD, or zero if it is the last one.
    out.extend_from_slice(&next_ifd_offset.to_le_bytes());
    out.extend_from_slice(&parea);
    Ok(out)
}

/// `integerIFDEntry`.
fn integer_entry(tag: u16, value: u32) -> IfdEntry {
    IfdEntry {
        tag,
        datatype: if value <= u32::from(u16::MAX) {
            DT_SHORT
        } else {
            DT_LONG
        },
        data: vec![value],
    }
}

fn entry(tag: u16, datatype: u16, data: &[u32]) -> IfdEntry {
    IfdEntry {
        tag,
        datatype,
        data: data.to_vec(),
    }
}

/// `tiff.Encode(w, m, nil)`: the uncompressed TIFF of a `w` by `h` image whose rows (`w` pixels
/// of four bytes each, no padding) are in `pix`.
///
/// The output is charged to `mem` while it is built.
///
/// # Errors
/// [`Error::Corrupt`] for pixels that are not `w * h * 4` bytes, or for an image that does not
/// fit a classic TIFF (data over 4 GiB), [`Error::Alloc`] and [`Error::OutOfMemory`].
pub(crate) fn encode(
    mem: &Mem,
    w: usize,
    h: usize,
    kind: Kind,
    pix: &[u8],
) -> Result<Vec<u8>, Error> {
    // imageDimensions.
    let (wd, ht) = match (u32::try_from(w), u32::try_from(h)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Err(Error::Corrupt("tiff: image dimensions too large".into())),
    };
    let size = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| Error::Corrupt("tiff: image data too large".into()))?;
    // Uncompressed: the strip must fit a LONG.
    let size32 =
        u32::try_from(size).map_err(|_| Error::Corrupt("tiff: image data too large".into()))?;
    if pix.len() < size {
        return Err(Error::Corrupt("tiff: invalid image pixel buffer".into()));
    }

    // encodePage's IFD, in ascending tag order. Go sorts by tag; the tags are distinct.
    let (photometric, extra_samples) = match kind {
        Kind::Cmyk => (P_CMYK, 0),
        // Unassociated alpha.
        Kind::Nrgba => (P_RGB, 2),
    };
    let mut ifd = vec![
        integer_entry(T_IMAGE_WIDTH, wd),
        integer_entry(T_IMAGE_LENGTH, ht),
        entry(T_BITS_PER_SAMPLE, DT_SHORT, &[8, 8, 8, 8]),
        entry(T_COMPRESSION, DT_SHORT, &[C_NONE]),
        entry(T_PHOTOMETRIC_INTERPRETATION, DT_SHORT, &[photometric]),
        // `setStripOffset` puts the real offset here: the data follows the 8-byte header.
        entry(T_STRIP_OFFSETS, DT_LONG, &[8]),
        entry(T_SAMPLES_PER_PIXEL, DT_SHORT, &[4]),
        integer_entry(T_ROWS_PER_STRIP, ht),
        entry(T_STRIP_BYTE_COUNTS, DT_LONG, &[size32]),
        // There is no support for storing the image resolution: a bogus 72x72 dpi.
        entry(T_X_RESOLUTION, DT_RATIONAL, &[72, 1]),
        entry(T_Y_RESOLUTION, DT_RATIONAL, &[72, 1]),
        entry(T_RESOLUTION_UNIT, DT_SHORT, &[RES_PER_INCH]),
    ];
    if extra_samples > 0 {
        ifd.push(entry(T_EXTRA_SAMPLES, DT_SHORT, &[extra_samples]));
    }

    // pageOffsets: the data after the header, then the IFD.
    let data_offset = 8u64;
    check_offset(data_offset)?;
    let ifd_offset = data_offset + size as u64;
    check_offset(ifd_offset)?;
    let ifd_bytes = marshal_ifd(ifd_offset as u32, 0, &ifd)?;
    check_offset(ifd_offset + ifd_bytes.len() as u64)?;

    // writePages.
    let total = 8usize
        .checked_add(size)
        .and_then(|n| n.checked_add(ifd_bytes.len()))
        .ok_or(Error::OutOfMemory)?;
    let _held = mem.charge(total as u64)?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)?;
    out.extend_from_slice(LE_HEADER);
    out.extend_from_slice(&(ifd_offset as u32).to_le_bytes());
    out.extend_from_slice(&pix[..size]);
    out.extend_from_slice(&ifd_bytes);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What a TIFF written by [`encode`] says.
    pub(crate) struct Decoded {
        pub(crate) w: usize,
        pub(crate) h: usize,
        pub(crate) photometric: u32,
        pub(crate) extra_samples: Option<u32>,
        pub(crate) pixels: Vec<u8>,
    }

    fn u16_at(b: &[u8], i: usize) -> u32 {
        u32::from(u16::from_le_bytes([b[i], b[i + 1]]))
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    /// A reader of the one-strip, uncompressed, 8-bit, four-sample TIFFs of [`encode`].
    pub(crate) fn decode(t: &[u8]) -> Decoded {
        assert_eq!(&t[..4], LE_HEADER);
        let ifd = u32_at(t, 4) as usize;
        let n = u16_at(t, ifd) as usize;
        let mut d = Decoded {
            w: 0,
            h: 0,
            photometric: 99,
            extra_samples: None,
            pixels: Vec::new(),
        };
        let (mut offset, mut count) = (0usize, 0usize);
        let mut tags = Vec::new();
        for i in 0..n {
            let e = ifd + 2 + 12 * i;
            let (tag, dt) = (u16_at(t, e) as u16, u16_at(t, e + 2));
            // A SHORT value is in the first two bytes of the value field, a LONG in all four.
            let v = if dt == u32::from(DT_SHORT) {
                u16_at(t, e + 8)
            } else {
                u32_at(t, e + 8)
            };
            tags.push(tag);
            match tag {
                T_IMAGE_WIDTH => d.w = v as usize,
                T_IMAGE_LENGTH => d.h = v as usize,
                T_COMPRESSION => assert_eq!(v, C_NONE),
                T_PHOTOMETRIC_INTERPRETATION => d.photometric = v,
                T_STRIP_OFFSETS => offset = v as usize,
                T_STRIP_BYTE_COUNTS => count = v as usize,
                T_EXTRA_SAMPLES => d.extra_samples = Some(v),
                T_BITS_PER_SAMPLE => {
                    let p = u32_at(t, e + 8) as usize;
                    assert_eq!(
                        (0..4).map(|k| u16_at(t, p + 2 * k)).collect::<Vec<_>>(),
                        [8, 8, 8, 8]
                    );
                }
                T_X_RESOLUTION | T_Y_RESOLUTION => {
                    let p = u32_at(t, e + 8) as usize;
                    assert_eq!((u32_at(t, p), u32_at(t, p + 4)), (72, 1));
                }
                _ => {}
            }
        }
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted, "tags are in ascending order");
        assert_eq!(u32_at(t, ifd + 2 + 12 * n), 0, "next IFD");
        d.pixels = t[offset..offset + count].to_vec();
        d
    }

    fn mem() -> Mem {
        Mem::new(&crate::config::Limits::default())
    }

    #[test]
    fn cmyk_layout() {
        let pix = [1, 2, 3, 4, 5, 6, 7, 8];
        let t = encode(&mem(), 2, 1, Kind::Cmyk, &pix).unwrap();
        // Header, 8 pixel bytes, the IFD (2 + 12 * 12 + 4 bytes) and the pointer area of the
        // BitsPerSample (8 bytes) and the two resolutions (8 bytes each).
        assert_eq!(t.len(), 8 + 8 + 2 + 12 * 12 + 4 + 8 + 16);
        assert_eq!(&t[..8], b"II\x2A\x00\x10\x00\x00\x00");
        let d = decode(&t);
        assert_eq!(
            (d.w, d.h, d.photometric, d.extra_samples),
            (2, 1, P_CMYK, None)
        );
        assert_eq!(d.pixels, pix);
    }

    #[test]
    fn nrgba_has_unassociated_alpha() {
        let pix = [9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 255, 254];
        let t = encode(&mem(), 1, 3, Kind::Nrgba, &pix).unwrap();
        assert_eq!(t.len(), 8 + 12 + 2 + 13 * 12 + 4 + 8 + 16);
        let d = decode(&t);
        assert_eq!(
            (d.w, d.h, d.photometric, d.extra_samples),
            (1, 3, P_RGB, Some(2))
        );
        assert_eq!(d.pixels, pix);
    }

    #[test]
    fn a_side_over_65535_is_a_long() {
        let e = integer_entry(T_IMAGE_WIDTH, 65535);
        assert_eq!(e.datatype, DT_SHORT);
        let e = integer_entry(T_IMAGE_WIDTH, 65536);
        assert_eq!(e.datatype, DT_LONG);
    }

    #[test]
    fn rejects_short_pixels_and_oversize_images() {
        assert!(encode(&mem(), 2, 2, Kind::Cmyk, &[0; 15]).is_err());
        // 2^15 x 2^15 x 4 bytes is 4 GiB, which does not fit a LONG; refused before any
        // allocation.
        let e = encode(&mem(), 1 << 15, 1 << 15, Kind::Cmyk, &[]).unwrap_err();
        assert!(e.to_string().contains("too large"), "{e}");
    }

    #[test]
    fn output_is_charged() {
        let small = Mem::with_max(10);
        let e = encode(&small, 2, 1, Kind::Cmyk, &[0; 8]).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e}");
    }
}
