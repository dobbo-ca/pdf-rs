#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::Mem;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

/// The CRC-32 of the PNG chunks (IEEE), bit by bit.
fn crc32(p: &[u8]) -> u32 {
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

/// A decoder for the 8-bit gray, RGB and RGBA PNGs the writer makes: checks the signature, every
/// CRC and the chunk order, inflates the IDAT data with deflate-rs and undoes the row filters.
/// Returns `(width, height, colour type, unfiltered pixels)`.
fn unpng(png: &[u8]) -> (usize, usize, u8, Vec<u8>) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "signature");
    let mut pos = 8;
    let (mut w, mut h, mut ct) = (0usize, 0usize, 0u8);
    let mut idat = Vec::new();
    let mut ended = false;
    while pos < png.len() {
        assert!(!ended, "a chunk after IEND");
        let n = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let name = &png[pos + 4..pos + 8];
        let data = &png[pos + 8..pos + 8 + n];
        let crc = u32::from_be_bytes(png[pos + 8 + n..pos + 12 + n].try_into().unwrap());
        assert_eq!(crc, crc32(&png[pos + 4..pos + 8 + n]), "crc of {name:?}");
        match name {
            b"IHDR" => {
                w = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                assert_eq!(data[8], 8, "bit depth");
                ct = data[9];
                assert_eq!(&data[10..], &[0, 0, 0], "compression, filter, interlace");
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => {
                assert_eq!(n, 0);
                ended = true;
            }
            _ => panic!("unexpected chunk {name:?}"),
        }
        pos += 12 + n;
    }
    assert!(ended, "no IEND");
    let bpp = match ct {
        0 => 1,
        2 => 3,
        6 => 4,
        _ => panic!("colour type {ct}"),
    };
    let raw = deflate::zlib::decode(&idat, &deflate::Limits::default(), &AtomicBool::new(false))
        .expect("the IDAT data inflates");
    let stride = w * bpp;
    assert_eq!(raw.len(), h * (stride + 1), "inflated size");
    let mut px = vec![0u8; h * stride];
    for y in 0..h {
        let ft = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for i in 0..stride {
            let a = if i >= bpp {
                px[y * stride + i - bpp]
            } else {
                0
            };
            let b = if y > 0 { px[(y - 1) * stride + i] } else { 0 };
            let c = if y > 0 && i >= bpp {
                px[(y - 1) * stride + i - bpp]
            } else {
                0
            };
            let pred = match ft {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => {
                    let (pa, pb, pc) = (
                        (i32::from(b) - i32::from(c)).abs(),
                        (i32::from(a) - i32::from(c)).abs(),
                        (i32::from(a) + i32::from(b) - 2 * i32::from(c)).abs(),
                    );
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => panic!("filter type {ft}"),
            };
            px[y * stride + i] = line[i].wrapping_add(pred);
        }
    }
    (w, h, ct, px)
}

// The PNG writer: the first three bytes choose the width (1 to 64), the height (1 to 64) and gray
// or NRGBA; the rest, repeated or cut to size, is the pixels. The PNG is read back with an
// independent decoder (deflate-rs inflate, CRCs, row unfiltering) and must give the pixels back,
// with the alpha channel dropped when every alpha is 0xff (colour type 2, as Go's encoder does).
fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let w = 1 + usize::from(data[0]) % 64;
    let h = 1 + usize::from(data[1]) % 64;
    let rgba = data[2] & 1 != 0;
    let bpp = if rgba { 4 } else { 1 };
    let src = &data[3..];
    let pixels: Vec<u8> = (0..w * h * bpp)
        .map(|i| {
            if src.is_empty() {
                0
            } else {
                src[i % src.len()]
            }
        })
        .collect();
    let mem = Mem::new(&common::TIGHT);
    let cancel = common::never();
    let png = match pdf::fuzz_api::png_encode(&mem, w, h, &pixels, rgba, &cancel) {
        Ok(png) => png,
        Err(e) => panic!("the writer refused a {w}x{h} image: {e}"),
    };
    let (rw, rh, ct, got) = unpng(&png);
    assert_eq!((rw, rh), (w, h), "size");
    let want: Vec<u8> = match ct {
        0 => {
            assert!(!rgba, "gray image written as colour");
            pixels.clone()
        }
        2 => {
            assert!(rgba, "colour image written for gray pixels");
            assert!(
                pixels.chunks_exact(4).all(|p| p[3] == 0xff),
                "alpha dropped"
            );
            pixels
                .chunks_exact(4)
                .flat_map(|p| p[..3].to_vec())
                .collect()
        }
        6 => {
            assert!(rgba);
            assert!(pixels.chunks_exact(4).any(|p| p[3] != 0xff), "alpha kept");
            pixels.clone()
        }
        _ => panic!("colour type {ct}"),
    };
    assert_eq!(got, want, "the pixels changed");
    assert_eq!(mem.used(), 0, "bytes still charged after the call");
});
