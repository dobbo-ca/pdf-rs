//! The part of Go 1.27.4's `image/png` encoder (writer.go) that pdf-go reaches, byte for byte:
//! `png.Encode` with the default compression level, on an `image.Gray` (8 bits, colour type 0)
//! or an `image.NRGBA` or opaque `image.RGBA` (8 bits per channel, colour type 2 when every
//! alpha is 0xff, else 6). An `image.RGBA` that `renderCMYKToPng` fills is always opaque, so it
//! writes the same bytes as the equal `image.NRGBA`; one pixel format covers both.
//!
//! The row filter is chosen as Go does (`filter`: the five filters tried in the order Up, Paeth,
//! None, Sub, Average, with the early exits), the rows go through Go's zlib writer
//! ([`ZlibWriter`], level 6) into a 32 KiB `bufio.Writer`, and every `Write` that reaches the
//! encoder becomes one IDAT chunk, so the chunk boundaries equal Go's.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::{OutBuf, check};
use crate::error::Error;
use crate::filter::goflate::ZlibWriter;
use crate::ledger::Mem;
use std::sync::atomic::AtomicBool;

const PNG_HEADER: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// `bufio.NewWriterSize(e, 1<<15)`.
const BUFIO_SIZE: usize = 1 << 15;

const CT_GRAYSCALE: u8 = 0;
const CT_TRUE_COLOR: u8 = 2;
const CT_TRUE_COLOR_ALPHA: u8 = 6;

/// The pixels of an image, rows of `w` pixels one after the other.
pub(crate) enum Pixels<'a> {
    /// `image.Gray`: one byte per pixel.
    Gray(&'a [u8]),
    /// `image.NRGBA`: four bytes per pixel.
    Nrgba(&'a [u8]),
}

/// Go's `cbG8`, `cbTC8` and `cbTCA8`.
enum Cb {
    G8,
    Tc8,
    Tca8,
}

const fn make_crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut j = 1;
        while j < 8 {
            let prev = t[j - 1][i];
            t[j][i] = t[0][(prev & 0xff) as usize] ^ (prev >> 8);
            j += 1;
        }
        i += 1;
    }
    t
}

/// The tables of `crc32.IEEE` (slicing by 8).
static CRC_TABLES: [[u32; 256]; 8] = make_crc_tables();

/// `crc32.Update` with the IEEE polynomial.
pub(crate) fn crc32_update(crc: u32, mut p: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let mut c = !crc;
    while let Some((chunk, rest)) = p.split_first_chunk::<8>() {
        let a = c ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let b = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        c = t[7][(a & 0xff) as usize]
            ^ t[6][((a >> 8) & 0xff) as usize]
            ^ t[5][((a >> 16) & 0xff) as usize]
            ^ t[4][(a >> 24) as usize]
            ^ t[3][(b & 0xff) as usize]
            ^ t[2][((b >> 8) & 0xff) as usize]
            ^ t[1][((b >> 16) & 0xff) as usize]
            ^ t[0][(b >> 24) as usize];
        p = rest;
    }
    for &x in p {
        c = t[0][((c ^ u32::from(x)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// `encoder.writeChunk`.
fn write_chunk(out: &mut OutBuf, name: &[u8; 4], b: &[u8]) -> Result<(), Error> {
    let n = u32::try_from(b.len()).map_err(|_| {
        Error::Corrupt(format!(
            "png: {} chunk is too large: {}",
            String::from_utf8_lossy(name),
            b.len()
        ))
    })?;
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&n.to_be_bytes());
    header[4..].copy_from_slice(name);
    let crc = crc32_update(crc32_update(0, name), b);
    out.extend(&header)?;
    out.extend(b)?;
    out.extend(&crc.to_be_bytes())
}

/// The `bufio.Writer` of `writeIDATs`: a 32 KiB buffer in front of the encoder, which turns
/// each `Write` it gets into an IDAT chunk.
struct Bufio {
    buf: Vec<u8>,
}

impl Bufio {
    fn new() -> Result<Bufio, Error> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(BUFIO_SIZE)?;
        Ok(Bufio { buf })
    }

    /// `bufio.Writer.Write`: a write larger than the room left goes straight through when the
    /// buffer is empty, else it fills the buffer, which is flushed.
    fn write(&mut self, mut p: &[u8], out: &mut OutBuf) -> Result<(), Error> {
        while p.len() > BUFIO_SIZE - self.buf.len() {
            let n = if self.buf.is_empty() {
                // Large write, empty buffer.
                write_chunk(out, b"IDAT", p)?;
                p.len()
            } else {
                let n = BUFIO_SIZE - self.buf.len();
                self.buf.extend_from_slice(&p[..n]);
                self.flush(out)?;
                n
            };
            p = &p[n..];
        }
        self.buf.extend_from_slice(p);
        Ok(())
    }

    fn flush(&mut self, out: &mut OutBuf) -> Result<(), Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        write_chunk(out, b"IDAT", &self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

/// `abs8`: the absolute value of a byte interpreted as a signed int8.
fn abs8(d: u8) -> i64 {
    if d < 128 {
        i64::from(d)
    } else {
        256 - i64::from(d)
    }
}

/// `paeth` (image/png/paeth.go).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let pc = i64::from(c);
    let pa = i64::from(b) - pc;
    let pb = i64::from(a) - pc;
    let pc = (pa + pb).abs();
    let pa = pa.abs();
    let pb = pb.abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

const FT_NONE: usize = 0;
const FT_SUB: usize = 1;
const FT_UP: usize = 2;
const FT_AVERAGE: usize = 3;
const FT_PAETH: usize = 4;

/// `filter`: chooses the filter for the current row, which is `rows[0]`, and applies it. The
/// return value is the index of the filter and also of the row that has had it applied. Every
/// row is one filter-type byte and the data; `pr` is the previous row.
fn filter(rows: &mut [Vec<u8>; 5], pr: &[u8], bpp: usize) -> usize {
    let [r0, r1, r2, r3, r4] = rows;
    let cdat0 = &r0[1..];
    let cdat1 = &mut r1[1..];
    let cdat2 = &mut r2[1..];
    let cdat3 = &mut r3[1..];
    let cdat4 = &mut r4[1..];
    let pdat = &pr[1..];
    let n = cdat0.len();

    // The up filter.
    let mut sum = 0i64;
    for i in 0..n {
        cdat2[i] = cdat0[i].wrapping_sub(pdat[i]);
        sum += abs8(cdat2[i]);
    }
    let mut best = sum;
    let mut filter = FT_UP;

    // The Paeth filter.
    sum = 0;
    for i in 0..bpp {
        cdat4[i] = cdat0[i].wrapping_sub(pdat[i]);
        sum += abs8(cdat4[i]);
    }
    for i in bpp..n {
        cdat4[i] = cdat0[i].wrapping_sub(paeth(cdat0[i - bpp], pdat[i], pdat[i - bpp]));
        sum += abs8(cdat4[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_PAETH;
    }

    // The none filter.
    sum = 0;
    for &d in cdat0.iter() {
        sum += abs8(d);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_NONE;
    }

    // The sub filter.
    sum = 0;
    for i in 0..bpp {
        cdat1[i] = cdat0[i];
        sum += abs8(cdat1[i]);
    }
    for i in bpp..n {
        cdat1[i] = cdat0[i].wrapping_sub(cdat0[i - bpp]);
        sum += abs8(cdat1[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_SUB;
    }

    // The average filter.
    sum = 0;
    for i in 0..bpp {
        cdat3[i] = cdat0[i].wrapping_sub(pdat[i] / 2);
        sum += abs8(cdat3[i]);
    }
    for i in bpp..n {
        let avg = ((usize::from(cdat0[i - bpp]) + usize::from(pdat[i])) / 2) as u8;
        cdat3[i] = cdat0[i].wrapping_sub(avg);
        sum += abs8(cdat3[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        filter = FT_AVERAGE;
    }

    filter
}

/// `png.Encode`: the PNG of a `w` by `h` image at the default compression level.
///
/// The memory of the rows, the zlib writer and the output is charged to `mem`; the cancel flag
/// is checked once per row.
///
/// # Errors
/// [`Error::Corrupt`] for a size Go refuses (a side that is 0 or over 2^32 - 1) or pixels that
/// are not `w * h` pixels long, [`Error::Alloc`] and [`Error::OutOfMemory`], and
/// [`Error::Cancelled`].
pub(crate) fn encode(
    mem: &Mem,
    w: usize,
    h: usize,
    pix: Pixels<'_>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    if w == 0 || h == 0 || w as u64 >= 1 << 32 || h as u64 >= 1 << 32 {
        return Err(Error::Corrupt(format!("png: invalid image size: {w}x{h}")));
    }
    let (src, src_bpp) = match pix {
        Pixels::Gray(p) => (p, 1),
        Pixels::Nrgba(p) => (p, 4),
    };
    let row_len = w.checked_mul(src_bpp).ok_or(Error::OutOfMemory)?;
    if src.len() != row_len.checked_mul(h).ok_or(Error::OutOfMemory)? {
        return Err(Error::Corrupt("png: invalid image pixel buffer".into()));
    }
    let (cb, bits_per_pixel, color_type) = match pix {
        Pixels::Gray(_) => (Cb::G8, 8usize, CT_GRAYSCALE),
        Pixels::Nrgba(p) => {
            // `NRGBA.Opaque`.
            if p.iter().skip(3).step_by(4).all(|&a| a == 0xff) {
                (Cb::Tc8, 24, CT_TRUE_COLOR)
            } else {
                (Cb::Tca8, 32, CT_TRUE_COLOR_ALPHA)
            }
        }
    };

    let mut out = OutBuf::new(mem)?;
    out.extend(PNG_HEADER)?;
    // writeIHDR.
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&(w as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(h as u32).to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = color_type;
    write_chunk(&mut out, b"IHDR", &ihdr)?;

    // writeIDATs and writeImage. cr[0] is unfiltered; cr[1..] are the buffers for the other
    // filters; the first byte of cr[i] is the filter type i.
    let sz = 1 + (bits_per_pixel * w).div_ceil(8);
    let _held = mem.charge((sz as u64).saturating_mul(6))?;
    let mut rows: [Vec<u8>; 5] = Default::default();
    for (i, r) in rows.iter_mut().enumerate() {
        r.try_reserve_exact(sz)?;
        r.resize(sz, 0);
        r[0] = i as u8;
    }
    let mut pr: Vec<u8> = Vec::new();
    pr.try_reserve_exact(sz)?;
    pr.resize(sz, 0);

    let mut bw = Bufio::new()?;
    let mut zw = ZlibWriter::new(mem)?;
    {
        let mut sink = |p: &[u8]| bw.write(p, &mut out);
        for y in 0..h {
            check(cancel)?;
            let srow = &src[y * row_len..(y + 1) * row_len];
            let dst = &mut rows[0][1..];
            match cb {
                Cb::G8 | Cb::Tca8 => dst.copy_from_slice(srow),
                Cb::Tc8 => {
                    for (d, s) in dst.chunks_exact_mut(3).zip(srow.chunks_exact(4)) {
                        d.copy_from_slice(&s[..3]);
                    }
                }
            }
            let f = filter(&mut rows, &pr, bits_per_pixel / 8);
            zw.write(&rows[f], &mut sink, cancel)?;
            // The current row for y is the previous row for y+1.
            std::mem::swap(&mut pr, &mut rows[0]);
        }
        zw.close(&mut sink)?;
    }
    bw.flush(&mut out)?;
    write_chunk(&mut out, b"IEND", &[])?;
    Ok(out.into_vec())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What a PNG says: the header fields, the number of IDAT chunks, and the unfiltered rows
    /// (without the filter bytes).
    pub(crate) struct Decoded {
        pub(crate) w: usize,
        pub(crate) h: usize,
        pub(crate) color_type: u8,
        pub(crate) idats: usize,
        pub(crate) pixels: Vec<u8>,
    }

    /// `crc32.ChecksumIEEE` by the bit, to check the table.
    fn crc_slow(p: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in p {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    /// A decoder for 8-bit gray, RGB and RGBA PNGs: checks the signature and every CRC, inflates
    /// the IDAT data and undoes the filters.
    pub(crate) fn decode(png: &[u8]) -> Decoded {
        assert_eq!(&png[..8], PNG_HEADER);
        let mut pos = 8;
        let (mut w, mut h, mut ct) = (0, 0, 0);
        let mut idat = Vec::new();
        let mut idats = 0;
        let mut ended = false;
        while pos < png.len() {
            let n = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let name = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + n];
            let crc = u32::from_be_bytes(png[pos + 8 + n..pos + 12 + n].try_into().unwrap());
            assert_eq!(crc, crc_slow(&png[pos + 4..pos + 8 + n]), "crc of {name:?}");
            match name {
                b"IHDR" => {
                    w = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                    h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                    assert_eq!(data[8], 8);
                    ct = data[9];
                    assert_eq!(&data[10..], &[0, 0, 0]);
                }
                b"IDAT" => {
                    idat.extend_from_slice(data);
                    idats += 1;
                }
                b"IEND" => {
                    assert_eq!(n, 0);
                    ended = true;
                }
                _ => panic!("unexpected chunk {name:?}"),
            }
            pos += 12 + n;
        }
        assert!(ended);
        let bpp = match ct {
            0 => 1,
            2 => 3,
            6 => 4,
            _ => panic!("color type {ct}"),
        };
        let raw =
            deflate::zlib::decode(&idat, &deflate::Limits::default(), &AtomicBool::new(false))
                .unwrap();
        let stride = w * bpp;
        assert_eq!(raw.len(), h * (stride + 1));
        let mut pixels = vec![0u8; h * stride];
        for y in 0..h {
            let ft = raw[y * (stride + 1)];
            let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
            for i in 0..stride {
                let a = if i >= bpp {
                    pixels[y * stride + i - bpp]
                } else {
                    0
                };
                let b = if y > 0 {
                    pixels[(y - 1) * stride + i]
                } else {
                    0
                };
                let c = if y > 0 && i >= bpp {
                    pixels[(y - 1) * stride + i - bpp]
                } else {
                    0
                };
                let pred = match ft {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                    4 => paeth(a, b, c),
                    _ => panic!("filter {ft}"),
                };
                pixels[y * stride + i] = line[i].wrapping_add(pred);
            }
        }
        Decoded {
            w,
            h,
            color_type: ct,
            idats,
            pixels,
        }
    }

    fn mem() -> Mem {
        Mem::new(&crate::config::Limits::default())
    }

    fn encode_ok(w: usize, h: usize, pix: Pixels<'_>) -> Vec<u8> {
        encode(&mem(), w, h, pix, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn crc_table_is_the_ieee_crc() {
        for p in [
            &b""[..],
            b"123456789",
            b"IHDR",
            &[0u8; 100],
            &(0..=255u8).collect::<Vec<_>>(),
        ] {
            assert_eq!(crc32_update(0, p), crc_slow(p));
        }
        assert_eq!(crc32_update(0, b"123456789"), 0xCBF4_3926);
        // The state is chained as `crc.Write` does.
        let all = (0..=255u8).collect::<Vec<_>>();
        assert_eq!(
            crc32_update(crc32_update(0, &all[..77]), &all[77..]),
            crc32_update(0, &all)
        );
    }

    #[test]
    fn gray_round_trip() {
        let (w, h) = (7, 5);
        let pix: Vec<u8> = (0..w * h).map(|i| (i * 37 % 256) as u8).collect();
        let png = encode_ok(w, h, Pixels::Gray(&pix));
        let d = decode(&png);
        assert_eq!((d.w, d.h, d.color_type), (w, h, CT_GRAYSCALE));
        assert_eq!(d.pixels, pix);
    }

    #[test]
    fn opaque_nrgba_is_written_as_rgb() {
        let (w, h) = (3, 2);
        let mut pix = Vec::new();
        for i in 0..w * h {
            pix.extend_from_slice(&[i as u8, (i * 2) as u8, (i * 3) as u8, 255]);
        }
        let png = encode_ok(w, h, Pixels::Nrgba(&pix));
        let d = decode(&png);
        assert_eq!(d.color_type, CT_TRUE_COLOR);
        let want: Vec<u8> = pix.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
        assert_eq!(d.pixels, want);
    }

    #[test]
    fn translucent_nrgba_keeps_its_alpha() {
        let (w, h) = (3, 2);
        let mut pix = Vec::new();
        for i in 0..w * h {
            pix.extend_from_slice(&[i as u8, 9, 200, if i == 4 { 0x80 } else { 255 }]);
        }
        let png = encode_ok(w, h, Pixels::Nrgba(&pix));
        let d = decode(&png);
        assert_eq!(d.color_type, CT_TRUE_COLOR_ALPHA);
        assert_eq!(d.pixels, pix);
    }

    #[test]
    fn every_filter_round_trips_on_noise_and_gradients() {
        // 100 x 40 so that the filter search picks different filters on different rows.
        let (w, h) = (100, 40);
        let mut seed = 0x1234_5678u32;
        let mut pix = Vec::new();
        for y in 0..h {
            for x in 0..w {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 24) as u8;
                let v = match y % 4 {
                    0 => x as u8,
                    1 => (y * 3) as u8,
                    2 => noise,
                    _ => ((x * 2 + y) % 256) as u8,
                };
                pix.extend_from_slice(&[v, v.wrapping_add(7), noise, 255]);
            }
        }
        let png = encode_ok(w, h, Pixels::Nrgba(&pix));
        let d = decode(&png);
        let want: Vec<u8> = pix.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
        assert_eq!(d.pixels, want);
    }

    #[test]
    fn large_images_are_cut_into_idat_chunks_by_the_bufio_writer() {
        // Noise does not compress, so the output passes 32 KiB several times.
        let (w, h) = (300, 300);
        let mut seed = 99u32;
        let pix: Vec<u8> = (0..w * h)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect();
        let png = encode_ok(w, h, Pixels::Gray(&pix));
        let d = decode(&png);
        assert!(d.idats >= 2, "{} IDAT chunks", d.idats);
        assert_eq!(d.pixels, pix);
    }

    #[test]
    fn bufio_cuts_chunks_as_go() {
        let mem = mem();
        let mut out = OutBuf::new(&mem).unwrap();
        let mut bw = Bufio::new().unwrap();
        // Small writes are held; a big write into an empty buffer goes straight through; a
        // write that does not fit fills the buffer, which is flushed, and the rest is held.
        bw.write(&[1; 10], &mut out).unwrap();
        bw.write(&[2; BUFIO_SIZE], &mut out).unwrap();
        bw.write(&[3; 5], &mut out).unwrap();
        bw.flush(&mut out).unwrap();
        let v = out.into_vec();
        let mut chunks = Vec::new();
        let mut pos = 0;
        while pos < v.len() {
            let n = u32::from_be_bytes(v[pos..pos + 4].try_into().unwrap()) as usize;
            assert_eq!(&v[pos + 4..pos + 8], b"IDAT");
            chunks.push(n);
            pos += 12 + n;
        }
        assert_eq!(chunks, [BUFIO_SIZE, 15]);
    }

    #[test]
    fn rejects_a_bad_size_and_a_short_buffer() {
        let cancel = AtomicBool::new(false);
        assert!(encode(&mem(), 0, 1, Pixels::Gray(&[]), &cancel).is_err());
        assert!(encode(&mem(), 2, 2, Pixels::Gray(&[0; 3]), &cancel).is_err());
        assert!(encode(&mem(), 1, 1, Pixels::Nrgba(&[0; 3]), &cancel).is_err());
    }

    #[test]
    fn honours_cancel_and_the_ledger() {
        let pix = vec![0u8; 16];
        let e = encode(&mem(), 4, 4, Pixels::Gray(&pix), &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e, Error::Cancelled);
        let small = Mem::with_max(1000);
        let e = encode(&small, 4, 4, Pixels::Gray(&pix), &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e}");
    }
}
