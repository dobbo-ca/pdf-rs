//! Go's `image/jpeg` decoder (docs/design.md section 11, work package X2), ported from Go 1.27.4
//! `image/jpeg`: `reader.go` (the byte buffer with its `0xff00` unstuffing and unread, the marker
//! loop, `processSOF`, `processDQT`, `processDRI`, the APP0 and APP14 markers, `applyBlack`,
//! `isRGB`, `convertToRGB`, `DecodeConfig`), `huffman.go` (`processDHT`, `decodeHuffman`,
//! `decodeBit(s)`, `receiveExtend`, `ensureNBits`), `scan.go` (`makeImg`, `processSOS`, `refine`,
//! `refineNonZeroes`, `reconstructProgressiveImage`, `reconstructBlock`, `findRST`), `dct.go`
//! (`idct`) and `image/internal/imageutil` (`DrawYCbCr`). The entropy decode is Go's own, so
//! corrupt data decodes (or fails) as in Go, and Go's non-standard ("flex") sampling is accepted.
//!
//! [`decode_config`] is `jpeg.DecodeConfig`; [`decode`] is `jpeg.Decode`. [`decode`] returns the
//! four result types Go's decoder can return ([`GoImage`]), with the padded planes Go allocates,
//! because pdf-go gob-encodes the whole backing arrays (design section 3, row 3).

use crate::error::Error;
use crate::ledger::{Held, Mem};
use jpeg::{Cmyk, Gray, Rect, Rgba, Subsample, YCbCr};
use std::num::Wrapping as W;
use std::sync::atomic::{AtomicBool, Ordering};

/// The colour model `jpeg.DecodeConfig` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorModel {
    Gray,
    YCbCr,
    Rgba,
    Cmyk,
}

/// `image.Config` from `jpeg.DecodeConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegConfig {
    pub model: ColorModel,
    pub width: usize,
    pub height: usize,
}

/// What `jpeg.Decode` returns: `*image.Gray`, `*image.YCbCr`, `*image.RGBA` or `*image.CMYK`.
/// The pixel vectors are Go's whole backing arrays (block-padded for Gray and YCbCr), and `rect`
/// is the visible `(0, 0, width, height)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoImage {
    Gray(Gray),
    YCbCr(YCbCr),
    Rgba(Rgba),
    Cmyk(Cmyk),
}

/// A decoded image and its charge on the [`Mem`] ledger, released when it drops.
#[derive(Debug)]
pub struct Decoded {
    pub image: GoImage,
    pub held: Held,
}

/// Go's `FormatError`.
fn format_err(s: &'static str) -> Error {
    Error::Jpeg(jpeg::Error::Format(s))
}

/// Go's `UnsupportedError`.
fn unsupported(s: &'static str) -> Error {
    Error::Jpeg(jpeg::Error::UnsupportedFeature(s))
}

/// `errUnsupportedSubsamplingRatio`.
fn sub_ratio() -> Error {
    unsupported("luma/chroma subsampling ratio")
}

/// `errMissingFF00`.
fn missing_ff00() -> Error {
    format_err("missing 0xff00 sequence")
}

/// `errShortHuffmanData`.
fn short_huffman() -> Error {
    format_err("short Huffman data")
}

/// A site where Go would index out of range.
fn oob() -> Error {
    Error::Panic("jpeg: index out of range")
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn alloc(n: usize) -> Result<Vec<u8>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)?;
    v.resize(n, 0);
    Ok(v)
}

// ---------------------------------------------------------------------------------------------
// The IDCT (dct.go). Go's int32 arithmetic wraps, so every intermediate is a Wrapping<i32>.

const COS1: u64 = 1130768441178740757;
const SIN1: u64 = 224923827593068887;
const COS3: u64 = 958619196450722178;
const SIN3: u64 = 640528868967736374;
const SQRT2INV: u64 = 815238614083298888;
const SQRT2INV_COS6: u64 = 311978311033955632;
const SQRT2INV_SIN6: u64 = 753182269664427492;

/// Go's `c(x, bits)`: the 60-bit constant rounded to `bits` bits.
const fn c(x: u64, bits: u32) -> i32 {
    ((x + (1u64 << (59 - bits))) >> (60 - bits)) as i32
}

/// Go's `dctBox`: a 3-multiply rotation and scaling.
fn dct_box(x0: W<i32>, x1: W<i32>, kcos: i32, ksin: i32) -> (W<i32>, W<i32>) {
    let kcos = W(kcos);
    let ksin = W(ksin);
    let ksum = kcos * (x0 + x1);
    let y0 = ksum + (ksin - kcos) * x1;
    let y1 = ksum - (kcos + ksin) * x0;
    (y0, y1)
}

/// Go's `idctRows`.
fn idct_rows(b: &mut [i32; 64]) {
    for i in 0..8 {
        let r = 8 * i;
        let mut x0 = W(b[r]);
        let mut x7 = W(b[r + 1]);
        let mut x2 = W(b[r + 2]);
        let mut x5 = W(b[r + 3]);
        let mut x1 = W(b[r + 4]);
        let mut x6 = W(b[r + 5]);
        let mut x3 = W(b[r + 6]);
        let mut x4 = W(b[r + 7]);

        x0 <<= 17;
        x1 <<= 17;
        (x0, x1) = (x0 + x1, x0 - x1);

        (x2, x3) = dct_box(x2, x3, c(SQRT2INV_COS6, 18), -c(SQRT2INV_SIN6, 18));
        (x1, x2) = (x1 + x2, x1 - x2);
        (x0, x3) = (x0 + x3, x0 - x3);

        x4 <<= 7;
        x7 <<= 7;
        (x7, x4) = (x7 + x4, x7 - x4);

        x6 *= W(c(SQRT2INV, 8));
        x5 *= W(c(SQRT2INV, 8));

        (x7, x5) = (x7 + x5, x7 - x5);
        (x4, x6) = (x4 + x6, x4 - x6);

        (x4, x7) = dct_box(x4 >> 2, x7 >> 2, c(COS3, 12), -c(SIN3, 12));
        (x5, x6) = dct_box(x5 >> 2, x6 >> 2, c(COS1, 12), -c(SIN1, 12));

        (x0, x7) = (x0 + x7, x0 - x7);
        (x1, x6) = (x1 + x6, x1 - x6);
        (x2, x5) = (x2 + x5, x2 - x5);
        (x3, x4) = (x3 + x4, x3 - x4);

        b[r] = x0.0;
        b[r + 1] = x1.0;
        b[r + 2] = x2.0;
        b[r + 3] = x3.0;
        b[r + 4] = x4.0;
        b[r + 5] = x5.0;
        b[r + 6] = x6.0;
        b[r + 7] = x7.0;
    }
}

/// Go's `idctCols`.
fn idct_cols(b: &mut [i32; 64]) {
    for i in 0..8 {
        let mut x0 = W(b[i]);
        let mut x7 = W(b[8 + i]);
        let mut x2 = W(b[16 + i]);
        let mut x5 = W(b[24 + i]);
        let mut x1 = W(b[32 + i]);
        let mut x6 = W(b[40 + i]);
        let mut x3 = W(b[48 + i]);
        let mut x4 = W(b[56 + i]);

        x0 += W(1 << 19);

        (x0, x1) = ((x0 + x1) >> 2, (x0 - x1) >> 2);
        (x2, x3) = dct_box(
            x2 >> 13,
            x3 >> 13,
            c(SQRT2INV_COS6, 12),
            -c(SQRT2INV_SIN6, 12),
        );
        (x1, x2) = (x1 + x2, x1 - x2);
        (x0, x3) = (x0 + x3, x0 - x3);

        (x7, x4) = (x7 + x4, x7 - x4);

        x5 = (x5 >> 13) * W(c(SQRT2INV, 14));
        x6 = (x6 >> 13) * W(c(SQRT2INV, 14));

        (x7, x5) = (x7 + x5, x7 - x5);
        (x4, x6) = (x4 + x6, x4 - x6);

        (x4, x7) = dct_box(x4 >> 14, x7 >> 14, c(COS3, 12), -c(SIN3, 12));
        (x5, x6) = dct_box(x5 >> 14, x6 >> 14, c(COS1, 12), -c(SIN1, 12));

        (x0, x7) = (x0 + x7, x0 - x7);
        (x1, x6) = (x1 + x6, x1 - x6);
        (x2, x5) = (x2 + x5, x2 - x5);
        (x3, x4) = (x3 + x4, x3 - x4);

        b[i] = (x0 >> 18).0;
        b[8 + i] = (x1 >> 18).0;
        b[16 + i] = (x2 >> 18).0;
        b[24 + i] = (x3 >> 18).0;
        b[32 + i] = (x4 >> 18).0;
        b[40 + i] = (x5 >> 18).0;
        b[48 + i] = (x6 >> 18).0;
        b[56 + i] = (x7 >> 18).0;
    }
}

/// Go's `idct`: the inverse DCT of a dequantized block in natural order, in place. The output is
/// the sample before the level shift (add 128 and clamp to get the pixel).
fn idct(b: &mut [i32; 64]) {
    idct_rows(b);
    idct_cols(b);
}

// ---------------------------------------------------------------------------------------------
// The decoder state, the byte reader and the bit reader (reader.go, huffman.go).

/// `unzig`: the natural-order index of each zig-zag index.
const UNZIG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// `huffman`: a Huffman decoder. `lut` maps the next 8 bits to the value (high byte) and 1 plus
/// the code length (low byte), or 0 when the code is longer than 8 bits.
#[derive(Clone, Copy)]
struct Huffman {
    n_codes: i32,
    lut: [u16; 256],
    vals: [u8; 256],
    min_codes: [i32; 16],
    max_codes: [i32; 16],
    vals_indices: [i32; 16],
}

const NO_HUFFMAN: Huffman = Huffman {
    n_codes: 0,
    lut: [0; 256],
    vals: [0; 256],
    min_codes: [0; 16],
    max_codes: [0; 16],
    vals_indices: [0; 16],
};

/// `bits`: the `n` unread bits in the low bits of `a`, read MSB first; `m` is `1 << (n - 1)`, or
/// 0 when `n` is 0.
#[derive(Debug, Clone, Copy, Default)]
struct Bits {
    a: u32,
    m: u32,
    n: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: u8,
}

/// What Go's decoder keeps from the headers.
#[derive(Debug, Default)]
struct Meta {
    width: usize,
    height: usize,
    n_comp: usize,
    comps: [Comp; 4],
    max_h: usize,
    max_v: usize,
    progressive: bool,
    jfif: bool,
    /// The Adobe transform byte, when an APP14 "Adobe" segment was seen.
    adobe: Option<u8>,
}

/// The planes `makeImg` allocates: Y (or gray), Cb, Cr, black.
struct Img {
    lay: Layout,
    planes: Vec<Vec<u8>>,
}

/// Go's `decoder`. pdf-go reads from a `bytes.Reader`, so Go's 4096-byte buffer always holds
/// what follows and keeps the 2 bytes an unread can give back; reading `src` in place is the same
/// byte for byte (the fast and slow paths of `readByteStuffedByte` end at the same offset, and an
/// empty refill is the end of `src`).
struct Decoder<'a> {
    src: &'a [u8],
    /// `bytes.i`: the next unread byte of `src`.
    i: usize,
    /// `bytes.nUnreadable`.
    n_unreadable: usize,
    bits: Bits,
    m: Meta,
    ri: usize,
    baseline: bool,
    eob_run: u16,
    /// `progCoeffs`; `None` is Go's nil.
    prog: [Option<Vec<[i32; 64]>>; 4],
    huff: [[Huffman; 4]; 2],
    /// The quantization tables, in zig-zag order.
    quant: [[i32; 64]; 4],
    img: Option<Img>,
    /// The ledger charge for the planes and the progressive coefficients.
    held: Held,
    cancel: &'a AtomicBool,
}

impl<'a> Decoder<'a> {
    fn new(src: &'a [u8], held: Held, cancel: &'a AtomicBool) -> Self {
        Decoder {
            src,
            i: 0,
            n_unreadable: 0,
            bits: Bits::default(),
            m: Meta::default(),
            ri: 0,
            baseline: false,
            eob_run: 0,
            prog: Default::default(),
            huff: [[NO_HUFFMAN; 4]; 2],
            quant: [[0; 64]; 4],
            img: None,
            held,
            cancel,
        }
    }

    /// `unreadByteStuffedByte`.
    fn unread_byte_stuffed_byte(&mut self) {
        self.i = self.i.saturating_sub(self.n_unreadable);
        self.n_unreadable = 0;
        if self.bits.n >= 8 {
            self.bits.a >>= 8;
            self.bits.n -= 8;
            self.bits.m >>= 8;
        }
    }

    /// `readByte`: the end of `src` is `io.ErrUnexpectedEOF`.
    fn read_byte(&mut self) -> Result<u8, Error> {
        let x = *self.src.get(self.i).ok_or(Error::UnexpectedEof)?;
        self.i += 1;
        self.n_unreadable = 0;
        Ok(x)
    }

    /// `readByteStuffedByte`.
    fn read_byte_stuffed_byte(&mut self) -> Result<u8, Error> {
        self.n_unreadable = 0;
        let x = self.read_byte()?;
        self.n_unreadable = 1;
        if x != 0xff {
            return Ok(x);
        }
        let x = self.read_byte()?;
        self.n_unreadable = 2;
        if x != 0x00 {
            return Err(missing_ff00());
        }
        Ok(0xff)
    }

    /// The overshoot undo at the top of `readFull` and `ignore`.
    fn unread_overshoot(&mut self) {
        if self.n_unreadable != 0 {
            if self.bits.n >= 8 {
                self.unread_byte_stuffed_byte();
            }
            self.n_unreadable = 0;
        }
    }

    /// `readFull`.
    fn read_full(&mut self, dst: &mut [u8]) -> Result<(), Error> {
        self.unread_overshoot();
        let end = self.i.checked_add(dst.len()).ok_or(Error::UnexpectedEof)?;
        let Some(s) = self.src.get(self.i..end) else {
            self.i = self.src.len();
            return Err(Error::UnexpectedEof);
        };
        dst.copy_from_slice(s);
        self.i = end;
        Ok(())
    }

    /// `ignore`.
    fn ignore(&mut self, n: usize) -> Result<(), Error> {
        self.unread_overshoot();
        match self.i.checked_add(n) {
            Some(end) if end <= self.src.len() => {
                self.i = end;
                Ok(())
            }
            _ => {
                self.i = self.src.len();
                Err(Error::UnexpectedEof)
            }
        }
    }

    /// `ensureNBits`.
    fn ensure_n_bits(&mut self, n: i32) -> Result<(), Error> {
        loop {
            let c = match self.read_byte_stuffed_byte() {
                Ok(c) => c,
                Err(Error::UnexpectedEof) => return Err(short_huffman()),
                Err(e) => return Err(e),
            };
            self.bits.a = (self.bits.a << 8) | u32::from(c);
            self.bits.n += 8;
            if self.bits.m == 0 {
                self.bits.m = 1 << 7;
            } else {
                self.bits.m <<= 8;
            }
            if self.bits.n >= n {
                return Ok(());
            }
        }
    }

    /// `receiveExtend`: the signed value coded in `t` bits.
    fn receive_extend(&mut self, t: u8) -> Result<i32, Error> {
        let tn = i32::from(t);
        if self.bits.n < tn {
            self.ensure_n_bits(tn)?;
        }
        self.bits.n -= tn;
        self.bits.m >>= t;
        if t == 0 {
            // Go's branchless form gives 0 (its shift by t-1 wraps to 255).
            return Ok(0);
        }
        let s = 1i32 << t;
        let x = ((self.bits.a >> self.bits.n) as i32) & (s - 1);
        let sign = (x >> (t - 1)) - 1;
        Ok(x + (sign & ((-1i32 << t) + 1)))
    }

    /// `processDHT`.
    fn process_dht(&mut self, mut n: usize) -> Result<(), Error> {
        while n > 0 {
            if n < 17 {
                return Err(format_err("DHT has wrong length"));
            }
            let mut t = [0u8; 17];
            self.read_full(&mut t)?;
            let tc = t[0] >> 4;
            if tc > 1 {
                return Err(format_err("bad Tc value"));
            }
            let th = t[0] & 0x0f;
            if th > 3 || (self.baseline && th > 1) {
                return Err(format_err("bad Th value"));
            }
            let mut counts = [0i32; 16];
            let mut total = 0i32;
            for i in 0..16 {
                counts[i] = i32::from(t[i + 1]);
                total += counts[i];
            }
            if total == 0 {
                return Err(format_err("Huffman table has zero length"));
            }
            if total > 256 {
                return Err(format_err("Huffman table has excessive length"));
            }
            let nc = total as usize;
            n = match n.checked_sub(nc + 17) {
                Some(r) => r,
                None => return Err(format_err("DHT has wrong length")),
            };
            let mut vals = [0u8; 256];
            self.read_full(&mut vals[..nc])?;
            let h = &mut self.huff[usize::from(tc)][usize::from(th)];
            h.n_codes = total;
            h.vals[..nc].copy_from_slice(&vals[..nc]);

            // The look-up table.
            h.lut = [0; 256];
            let mut x = 0usize;
            let mut code = 0u32;
            for i in 0..8u32 {
                code <<= 1;
                for _ in 0..counts[i as usize] {
                    // Go's uint8 conversion drops the high bits of an over-full table's code.
                    let base = (code << (7 - i)) as u8;
                    let lut_value = (u16::from(h.vals[x]) << 8) | (2 + i) as u16;
                    for k in 0..(1u32 << (7 - i)) {
                        h.lut[usize::from(base | k as u8)] = lut_value;
                    }
                    code += 1;
                    x += 1;
                }
            }

            // minCodes, maxCodes and valsIndices.
            let mut c = 0i32;
            let mut index = 0i32;
            for i in 0..16 {
                let k = counts[i];
                if k == 0 {
                    h.min_codes[i] = -1;
                    h.max_codes[i] = -1;
                    h.vals_indices[i] = -1;
                } else {
                    h.min_codes[i] = c;
                    h.max_codes[i] = c + k - 1;
                    h.vals_indices[i] = index;
                    c += k;
                    index += k;
                }
                c <<= 1;
            }
        }
        Ok(())
    }

    /// `decodeHuffman` with table `huff[tc][th]`.
    fn decode_huffman(&mut self, tc: usize, th: usize) -> Result<u8, Error> {
        if self.huff[tc][th].n_codes == 0 {
            return Err(format_err("uninitialized Huffman table"));
        }
        let mut slow = false;
        if self.bits.n < 8
            && let Err(e) = self.ensure_n_bits(8)
        {
            if e != missing_ff00() && e != short_huffman() {
                return Err(e);
            }
            // No more bytes in this segment, but the bits already read may hold the next
            // symbol: undo the read that ensureNBits made.
            if self.n_unreadable != 0 {
                self.unread_byte_stuffed_byte();
            }
            slow = true;
        }
        if !slow {
            let v = self.huff[tc][th].lut[((self.bits.a >> (self.bits.n - 8)) & 0xff) as usize];
            if v != 0 {
                let n = (v & 0xff) - 1;
                self.bits.n -= i32::from(n);
                self.bits.m >>= n;
                return Ok((v >> 8) as u8);
            }
        }
        let mut code = 0i32;
        for i in 0..16 {
            if self.bits.n == 0 {
                self.ensure_n_bits(1)?;
            }
            if self.bits.a & self.bits.m != 0 {
                code |= 1;
            }
            self.bits.n -= 1;
            self.bits.m >>= 1;
            let h = &self.huff[tc][th];
            if code <= h.max_codes[i] {
                let k = h.vals_indices[i] + code - h.min_codes[i];
                let k = usize::try_from(k).map_err(|_| oob())?;
                return h.vals.get(k).copied().ok_or_else(oob);
            }
            code <<= 1;
        }
        Err(format_err("bad Huffman code"))
    }

    /// `decodeBit`.
    fn decode_bit(&mut self) -> Result<bool, Error> {
        if self.bits.n == 0 {
            self.ensure_n_bits(1)?;
        }
        let ret = self.bits.a & self.bits.m != 0;
        self.bits.n -= 1;
        self.bits.m >>= 1;
        Ok(ret)
    }

    /// `decodeBits`, for `n` up to 14.
    fn decode_bits(&mut self, n: i32) -> Result<u32, Error> {
        if self.bits.n < n {
            self.ensure_n_bits(n)?;
        }
        let ret = (self.bits.a >> (self.bits.n - n)) & ((1u32 << n) - 1);
        self.bits.n -= n;
        self.bits.m >>= n;
        Ok(ret)
    }

    // -----------------------------------------------------------------------------------------
    // The marker loop and the header segments (reader.go).

    /// `processSOF`. `n` is the segment length less its own two bytes.
    fn process_sof(&mut self, n: usize) -> Result<(), Error> {
        if self.m.n_comp != 0 {
            return Err(format_err("multiple SOF markers"));
        }
        let nc = match n {
            9 => 1,
            15 => 3,
            18 => 4,
            _ => return Err(unsupported("number of components")),
        };
        self.m.n_comp = nc;
        let mut t = [0u8; 18];
        self.read_full(&mut t[..n])?;
        let m = &mut self.m;
        if t[0] != 8 {
            return Err(unsupported("precision"));
        }
        m.height = (usize::from(t[1]) << 8) + usize::from(t[2]);
        m.width = (usize::from(t[3]) << 8) + usize::from(t[4]);
        if usize::from(t[5]) != nc {
            return Err(format_err("SOF has wrong length"));
        }
        for i in 0..nc {
            m.comps[i].id = t[6 + 3 * i];
            for j in 0..i {
                if m.comps[i].id == m.comps[j].id {
                    return Err(format_err("repeated component identifier"));
                }
            }
            m.comps[i].tq = t[8 + 3 * i];
            if m.comps[i].tq > 3 {
                return Err(format_err("bad Tq value"));
            }
            let hv = t[7 + 3 * i];
            let mut h = usize::from(hv >> 4);
            let mut v = usize::from(hv & 0x0f);
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return Err(format_err("luma/chroma subsampling ratio"));
            }
            if h == 3 || v == 3 {
                return Err(sub_ratio());
            }
            match nc {
                1 => {
                    h = 1;
                    v = 1;
                }
                3 => {}
                _ => match i {
                    0 => {
                        if hv != 0x11 && hv != 0x22 {
                            return Err(sub_ratio());
                        }
                    }
                    1 | 2 => {
                        if hv != 0x11 {
                            return Err(sub_ratio());
                        }
                    }
                    _ => {
                        if m.comps[0].h != h || m.comps[0].v != v {
                            return Err(sub_ratio());
                        }
                    }
                },
            }
            m.max_h = m.max_h.max(h);
            m.max_v = m.max_v.max(v);
            m.comps[i].h = h;
            m.comps[i].v = v;
        }
        if nc == 3 {
            for i in 0..3 {
                if !m.max_h.is_multiple_of(m.comps[i].h) || !m.max_v.is_multiple_of(m.comps[i].v) {
                    return Err(sub_ratio());
                }
            }
        }
        Ok(())
    }

    /// `processDQT`.
    fn process_dqt(&mut self, mut n: usize) -> Result<(), Error> {
        while n > 0 {
            n -= 1;
            let x = self.read_byte()?;
            let tq = usize::from(x & 0x0f);
            if tq > 3 {
                return Err(format_err("bad Tq value"));
            }
            match x >> 4 {
                0 => {
                    if n < 64 {
                        break;
                    }
                    n -= 64;
                    let mut t = [0u8; 64];
                    self.read_full(&mut t)?;
                    for i in 0..64 {
                        self.quant[tq][i] = i32::from(t[i]);
                    }
                }
                1 => {
                    if n < 128 {
                        break;
                    }
                    n -= 128;
                    let mut t = [0u8; 128];
                    self.read_full(&mut t)?;
                    for i in 0..64 {
                        self.quant[tq][i] = (i32::from(t[2 * i]) << 8) | i32::from(t[2 * i + 1]);
                    }
                }
                _ => return Err(format_err("bad Pq value")),
            }
        }
        if n != 0 {
            return Err(format_err("DQT has wrong length"));
        }
        Ok(())
    }

    /// `processDRI`.
    fn process_dri(&mut self, n: usize) -> Result<(), Error> {
        if n != 2 {
            return Err(format_err("DRI has wrong length"));
        }
        let mut t = [0u8; 2];
        self.read_full(&mut t)?;
        self.ri = (usize::from(t[0]) << 8) + usize::from(t[1]);
        Ok(())
    }

    /// `processApp0Marker`.
    fn process_app0(&mut self, n: usize) -> Result<(), Error> {
        if n < 5 {
            return self.ignore(n);
        }
        let mut t = [0u8; 5];
        self.read_full(&mut t)?;
        self.m.jfif = &t == b"JFIF\0";
        if n > 5 {
            return self.ignore(n - 5);
        }
        Ok(())
    }

    /// `processApp14Marker`.
    fn process_app14(&mut self, n: usize) -> Result<(), Error> {
        if n < 12 {
            return self.ignore(n);
        }
        let mut t = [0u8; 12];
        self.read_full(&mut t)?;
        if &t[..5] == b"Adobe" {
            self.m.adobe = Some(t[11]);
        }
        if n > 12 {
            return self.ignore(n - 12);
        }
        Ok(())
    }

    /// Go's `decode` up to the result conversion: the marker loop, then
    /// `reconstructProgressiveImage`. With `config_only` it stops at the first SOS (or after the
    /// SOF of a JFIF file) as `DecodeConfig` does.
    fn run(&mut self, config_only: bool) -> Result<(), Error> {
        let mut t = [0u8; 2];
        self.read_full(&mut t)?;
        if t != [0xff, 0xd8] {
            return Err(format_err("missing SOI marker"));
        }
        loop {
            self.read_full(&mut t)?;
            // Extraneous data before a marker is ignored, as libjpeg does.
            while t[0] != 0xff {
                t[0] = t[1];
                t[1] = self.read_byte()?;
            }
            let mut marker = t[1];
            if marker == 0 {
                continue;
            }
            while marker == 0xff {
                marker = self.read_byte()?;
            }
            if marker == 0xd9 {
                break;
            }
            if (0xd0..=0xd7).contains(&marker) {
                continue;
            }
            self.read_full(&mut t)?;
            let total = (usize::from(t[0]) << 8) + usize::from(t[1]);
            let Some(n) = total.checked_sub(2) else {
                return Err(format_err("short segment length"));
            };
            match marker {
                0xc0..=0xc2 => {
                    self.baseline = marker == 0xc0;
                    self.m.progressive = marker == 0xc2;
                    let r = self.process_sof(n);
                    if config_only && self.m.jfif {
                        return r;
                    }
                    r?;
                }
                0xc4 if config_only => self.ignore(n)?,
                0xc4 => self.process_dht(n)?,
                0xdb if config_only => self.ignore(n)?,
                0xdb => self.process_dqt(n)?,
                0xda if config_only => return Ok(()),
                0xda => self.process_sos(n)?,
                0xdd if config_only => self.ignore(n)?,
                0xdd => self.process_dri(n)?,
                0xe0 => self.process_app0(n)?,
                0xee => self.process_app14(n)?,
                0xe1..=0xef | 0xfe => self.ignore(n)?,
                _ if marker < 0xc0 => return Err(format_err("unknown marker")),
                _ => return Err(unsupported("unknown marker")),
            }
        }
        if self.m.progressive {
            self.reconstruct_progressive_image()?;
        }
        if self.img.is_none() {
            return Err(format_err("missing SOS marker"));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Scans (scan.go).

    /// `makeImg`: the planes, charged on the ledger before they are allocated.
    fn make_img(&mut self) -> Result<(), Error> {
        let lay = Layout::new(&self.m)?;
        self.held.grow_by(lay.plane_bytes(self.m.n_comp))?;
        let mut planes: Vec<Vec<u8>> = Vec::new();
        planes.try_reserve_exact(4)?;
        planes.push(alloc(lay.y_len)?);
        if self.m.n_comp > 1 {
            planes.push(alloc(lay.c_len)?);
            planes.push(alloc(lay.c_len)?);
        }
        if self.m.n_comp == 4 {
            planes.push(alloc(lay.black_len)?);
        }
        self.img = Some(Img { lay, planes });
        Ok(())
    }

    /// `processSOS`.
    fn process_sos(&mut self, n: usize) -> Result<(), Error> {
        let n_comp = self.m.n_comp;
        if n_comp == 0 {
            return Err(format_err("missing SOF marker"));
        }
        if n < 6 || 4 + 2 * n_comp < n || !n.is_multiple_of(2) {
            return Err(format_err("SOS has wrong length"));
        }
        let mut t = [0u8; 12];
        self.read_full(&mut t[..n])?;
        let ns = usize::from(t[0]);
        if n != 4 + 2 * ns {
            return Err(format_err(
                "SOS length inconsistent with number of components",
            ));
        }
        // Per scan component: the component index and the DC and AC table selectors.
        let mut scan = [(0usize, 0usize, 0usize); 4];
        let mut total_hv = 0;
        for i in 0..ns {
            let cs = t[1 + 2 * i];
            let mut ci = None;
            for j in 0..n_comp {
                if cs == self.m.comps[j].id {
                    ci = Some(j);
                }
            }
            let Some(ci) = ci else {
                return Err(format_err("unknown component selector"));
            };
            scan[i].0 = ci;
            for j in 0..i {
                if scan[i].0 == scan[j].0 {
                    return Err(format_err("repeated component selector"));
                }
            }
            total_hv += self.m.comps[ci].h * self.m.comps[ci].v;
            let td = t[2 + 2 * i] >> 4;
            if td > 3 || (self.baseline && td > 1) {
                return Err(format_err("bad Td value"));
            }
            let ta = t[2 + 2 * i] & 0x0f;
            if ta > 3 || (self.baseline && ta > 1) {
                return Err(format_err("bad Ta value"));
            }
            scan[i].1 = usize::from(td);
            scan[i].2 = usize::from(ta);
        }
        if n_comp > 1 && total_hv > 10 {
            return Err(format_err("total sampling factors too large"));
        }

        let progressive = self.m.progressive;
        let (mut zig_start, mut zig_end, mut ah, mut al) = (0i32, 63i32, 0u32, 0u32);
        if progressive {
            zig_start = i32::from(t[1 + 2 * ns]);
            zig_end = i32::from(t[2 + 2 * ns]);
            ah = u32::from(t[3 + 2 * ns] >> 4);
            al = u32::from(t[3 + 2 * ns] & 0x0f);
            if (zig_start == 0 && zig_end != 0) || zig_start > zig_end || 64 <= zig_end {
                return Err(format_err("bad spectral selection bounds"));
            }
            if zig_start != 0 && ns != 1 {
                return Err(format_err(
                    "progressive AC coefficients for more than one component",
                ));
            }
            if ah != 0 && ah != al + 1 {
                return Err(format_err("bad successive approximation values"));
            }
        }

        let mxx = self.m.width.div_ceil(8 * self.m.max_h);
        let myy = self.m.height.div_ceil(8 * self.m.max_v);
        if self.img.is_none() {
            self.make_img()?;
        }
        if progressive {
            for &(ci, _, _) in &scan[..ns] {
                if self.prog[ci].is_none() {
                    let c = self.m.comps[ci];
                    let blocks = mxx
                        .checked_mul(myy)
                        .and_then(|b| b.checked_mul(c.h * c.v))
                        .ok_or_else(oob)?;
                    let bytes = (blocks as u64).saturating_mul(256);
                    self.held.grow_by(bytes)?;
                    let mut v = Vec::new();
                    v.try_reserve_exact(blocks)?;
                    v.resize(blocks, [0i32; 64]);
                    self.prog[ci] = Some(v);
                }
            }
        }

        self.bits = Bits::default();
        let mut mcu = 0usize;
        let mut expected_rst = 0xd0u8;
        let mut dc = [0i32; 4];
        let mut block_count = 0usize;
        let delta = 1i32 << al;
        for my in 0..myy {
            check_cancel(self.cancel)?;
            for mx in 0..mxx {
                for &(ci, td, ta) in &scan[..ns] {
                    let hi = self.m.comps[ci].h;
                    let vi = self.m.comps[ci].v;
                    for j in 0..hi * vi {
                        let (bx, by) = if ns != 1 {
                            (hi * mx + j % hi, vi * my + j / hi)
                        } else {
                            let q = mxx * hi;
                            let (bx, by) = (block_count % q, block_count / q);
                            block_count += 1;
                            if bx * 8 >= self.m.width || by * 8 >= self.m.height {
                                continue;
                            }
                            (bx, by)
                        };
                        let idx = by * mxx * hi + bx;

                        // The previous partially decoded coefficients, if progressive.
                        let mut b = if progressive {
                            *self.prog[ci]
                                .as_ref()
                                .and_then(|p| p.get(idx))
                                .ok_or_else(oob)?
                        } else {
                            [0i32; 64]
                        };

                        if ah != 0 {
                            self.refine(&mut b, ta, zig_start, zig_end, delta)?;
                        } else {
                            let mut zig = zig_start;
                            if zig == 0 {
                                zig += 1;
                                // The DC coefficient (section F.2.2.1).
                                let value = self.decode_huffman(0, td)?;
                                if value > 16 {
                                    return Err(unsupported("excessive DC component"));
                                }
                                let dc_delta = self.receive_extend(value)?;
                                dc[ci] = dc[ci].wrapping_add(dc_delta);
                                b[0] = dc[ci].wrapping_shl(al);
                            }

                            if zig <= zig_end && self.eob_run > 0 {
                                self.eob_run -= 1;
                            } else {
                                // The AC coefficients (section F.2.2.2). Go, unlike libjpeg,
                                // stops without reading the extra bits when a run passes
                                // zigEnd, and starts an EOB run on any baseline (r, 0) symbol
                                // with r < 15.
                                while zig <= zig_end {
                                    let value = self.decode_huffman(1, ta)?;
                                    let val0 = value >> 4;
                                    let val1 = value & 0x0f;
                                    if val1 != 0 {
                                        zig += i32::from(val0);
                                        if zig > zig_end {
                                            break;
                                        }
                                        let ac = self.receive_extend(val1)?;
                                        b[UNZIG[zig as usize]] = ac.wrapping_shl(al);
                                    } else {
                                        if val0 != 0x0f {
                                            self.eob_run = 1 << val0;
                                            if val0 != 0 {
                                                let bits = self.decode_bits(i32::from(val0))?;
                                                self.eob_run |= bits as u16;
                                            }
                                            self.eob_run -= 1;
                                            break;
                                        }
                                        zig += 0x0f;
                                    }
                                    zig += 1;
                                }
                            }
                        }

                        if progressive {
                            // Saved; reconstructProgressiveImage runs after the last scan.
                            *self.prog[ci]
                                .as_mut()
                                .and_then(|p| p.get_mut(idx))
                                .ok_or_else(oob)? = b;
                            continue;
                        }
                        self.reconstruct_block(&mut b, bx, by, ci)?;
                    }
                }
                mcu += 1;
                if self.ri > 0 && mcu.is_multiple_of(self.ri) && mcu < mxx * myy {
                    // The restart marker follows for well-formed input; else resynchronize.
                    let mut t = [0u8; 2];
                    self.read_full(&mut t)?;
                    if t[0] != 0xff || t[1] != expected_rst {
                        self.find_rst(expected_rst, t)?;
                    }
                    expected_rst += 1;
                    if expected_rst == 0xd8 {
                        expected_rst = 0xd0;
                    }
                    self.bits = Bits::default();
                    dc = [0; 4];
                    self.eob_run = 0;
                }
            }
        }
        Ok(())
    }

    /// `refine`: a successive approximation refinement block (section G.1.2).
    fn refine(
        &mut self,
        b: &mut [i32; 64],
        ta: usize,
        zig_start: i32,
        zig_end: i32,
        delta: i32,
    ) -> Result<(), Error> {
        if zig_start == 0 {
            // zig_end is 0: processSOS rejected anything else.
            if self.decode_bit()? {
                b[0] |= delta;
            }
            return Ok(());
        }
        let mut zig = zig_start;
        if self.eob_run == 0 {
            while zig <= zig_end {
                let mut z = 0i32;
                let value = self.decode_huffman(1, ta)?;
                let val0 = value >> 4;
                let val1 = value & 0x0f;
                match val1 {
                    0 => {
                        if val0 != 0x0f {
                            self.eob_run = 1 << val0;
                            if val0 != 0 {
                                let bits = self.decode_bits(i32::from(val0))?;
                                self.eob_run |= bits as u16;
                            }
                            break;
                        }
                    }
                    1 => {
                        z = delta;
                        if !self.decode_bit()? {
                            z = -z;
                        }
                    }
                    _ => return Err(format_err("unexpected Huffman code")),
                }
                zig = self.refine_non_zeroes(b, zig, zig_end, i32::from(val0), delta)?;
                if zig > zig_end {
                    return Err(format_err("too many coefficients"));
                }
                if z != 0 {
                    b[UNZIG[zig as usize]] = z;
                }
                zig += 1;
            }
        }
        if self.eob_run > 0 {
            self.eob_run -= 1;
            self.refine_non_zeroes(b, zig, zig_end, -1, delta)?;
        }
        Ok(())
    }

    /// `refineNonZeroes`: refines the non-zero entries of `b` in zig-zag order, after skipping
    /// `nz` zero entries when `nz >= 0`.
    fn refine_non_zeroes(
        &mut self,
        b: &mut [i32; 64],
        mut zig: i32,
        zig_end: i32,
        mut nz: i32,
        delta: i32,
    ) -> Result<i32, Error> {
        while zig <= zig_end {
            let u = UNZIG[zig as usize];
            if b[u] == 0 {
                if nz == 0 {
                    break;
                }
                nz -= 1;
                zig += 1;
                continue;
            }
            if self.decode_bit()? {
                if b[u] >= 0 {
                    b[u] = b[u].wrapping_add(delta);
                } else {
                    b[u] = b[u].wrapping_sub(delta);
                }
            }
            zig += 1;
        }
        Ok(zig)
    }

    /// `reconstructProgressiveImage`.
    fn reconstruct_progressive_image(&mut self) -> Result<(), Error> {
        let mxx = self.m.width.div_ceil(8 * self.m.max_h);
        for i in 0..self.m.n_comp {
            let Some(mut coeffs) = self.prog[i].take() else {
                continue;
            };
            let c = self.m.comps[i];
            let v = 8 * self.m.max_v / c.v;
            let h = 8 * self.m.max_h / c.h;
            let stride = mxx * c.h;
            let mut by = 0;
            while by * v < self.m.height {
                check_cancel(self.cancel)?;
                let mut bx = 0;
                while bx * h < self.m.width {
                    let b = coeffs.get_mut(by * stride + bx).ok_or_else(oob)?;
                    self.reconstruct_block(b, bx, by, i)?;
                    bx += 1;
                }
                by += 1;
            }
        }
        Ok(())
    }

    /// `reconstructBlock`: dequantizes, runs the IDCT and stores the block.
    fn reconstruct_block(
        &mut self,
        b: &mut [i32; 64],
        bx: usize,
        by: usize,
        ci: usize,
    ) -> Result<(), Error> {
        let c = self.m.comps[ci];
        let qt = &self.quant[usize::from(c.tq)];
        for zig in 0..64 {
            let u = UNZIG[zig];
            b[u] = b[u].wrapping_mul(qt[zig]);
        }
        idct(b);
        let img = self.img.as_mut().ok_or_else(oob)?;
        let (h, v) = if img.lay.flex {
            (self.m.max_h / c.h, self.m.max_v / c.v)
        } else {
            (1, 1)
        };
        let stride = img.lay.stride(ci);
        let dst = img.planes.get_mut(ci).ok_or_else(oob)?;
        put_block(dst, stride, bx, by, h, v, b)
    }

    /// `findRST`: moves past the next `expected` restart marker. `t` holds the next two bytes.
    fn find_rst(&mut self, expected: u8, mut t: [u8; 2]) -> Result<(), Error> {
        loop {
            let mut i = 0;
            if t[0] == 0xff {
                if t[1] == expected {
                    return Ok(());
                } else if t[1] == 0xff {
                    i = 1;
                } else if t[1] != 0x00 {
                    return Err(format_err("bad RST marker"));
                }
            } else if t[1] == 0xff {
                t[0] = 0xff;
                i = 1;
            }
            self.read_full(&mut t[i..])?;
        }
    }
}

/// Go's `isRGB`.
fn is_rgb(m: &Meta) -> bool {
    if m.jfif {
        return false;
    }
    if m.adobe == Some(0) {
        return true;
    }
    m.comps[0].id == b'R' && m.comps[1].id == b'G' && m.comps[2].id == b'B'
}

/// `jpeg.DecodeConfig`: the colour model and size, reading headers only.
///
/// # Errors
/// Go's `FormatError` and `UnsupportedError` as [`Error::Jpeg`] (`Format` and
/// `UnsupportedFeature`), and [`Error::UnexpectedEof`] where Go returns `io.ErrUnexpectedEOF`.
pub fn decode_config(src: &[u8]) -> Result<JpegConfig, Error> {
    // The header pass allocates nothing, so it charges an empty ledger.
    let mem = Mem::with_max(0);
    let cancel = AtomicBool::new(false);
    let mut d = Decoder::new(src, mem.hold(), &cancel);
    d.run(true)?;
    let m = &d.m;
    let model = match m.n_comp {
        1 => ColorModel::Gray,
        3 => {
            if is_rgb(m) {
                ColorModel::Rgba
            } else {
                ColorModel::YCbCr
            }
        }
        4 => ColorModel::Cmyk,
        _ => return Err(format_err("missing SOF marker")),
    };
    Ok(JpegConfig {
        model,
        width: m.width,
        height: m.height,
    })
}

// ---------------------------------------------------------------------------------------------
// The planes (scan.go `makeImg`, `reconstructBlock`).

/// The plane geometry `makeImg` allocates.
struct Layout {
    flex: bool,
    ratio: Subsample,
    /// Y stride (the Gray stride for one component).
    y_stride: usize,
    y_len: usize,
    c_stride: usize,
    c_len: usize,
    black_stride: usize,
    black_len: usize,
}

fn mul(a: usize, b: usize) -> Result<usize, Error> {
    a.checked_mul(b).ok_or(Error::Alloc {
        got: u64::MAX,
        max: 0,
    })
}

impl Layout {
    fn new(m: &Meta) -> Result<Layout, Error> {
        let mxx = m.width.div_ceil(8 * m.max_h);
        let myy = m.height.div_ceil(8 * m.max_v);
        let mut lay = Layout {
            flex: false,
            ratio: Subsample::S444,
            y_stride: 8 * mxx,
            y_len: 0,
            c_stride: 0,
            c_len: 0,
            black_stride: 0,
            black_len: 0,
        };
        if m.n_comp == 1 {
            lay.y_len = mul(8 * mxx, 8 * myy)?;
            return Ok(lay);
        }
        let c0 = m.comps[0];
        let c1 = m.comps[1];
        let c2 = m.comps[2];
        if c1.h != c2.h || c1.v != c2.v || m.max_h != c0.h || m.max_v != c0.v {
            lay.flex = true;
        } else {
            match ((m.max_h / c1.h) << 4) | (m.max_v / c1.v) {
                0x11 => lay.ratio = Subsample::S444,
                0x12 => lay.ratio = Subsample::S440,
                0x21 => lay.ratio = Subsample::S422,
                0x22 => lay.ratio = Subsample::S420,
                0x41 => lay.ratio = Subsample::S411,
                0x42 => lay.ratio = Subsample::S410,
                _ => lay.flex = true,
            }
        }
        let w = 8 * m.max_h * mxx;
        let h = 8 * m.max_v * myy;
        let (cw, ch) = match lay.ratio {
            Subsample::S444 => (w, h),
            Subsample::S422 => (w.div_ceil(2), h),
            Subsample::S420 => (w.div_ceil(2), h.div_ceil(2)),
            Subsample::S440 => (w, h.div_ceil(2)),
            Subsample::S411 => (w.div_ceil(4), h),
            Subsample::S410 => (w.div_ceil(4), h.div_ceil(2)),
        };
        lay.y_stride = w;
        lay.y_len = mul(w, h)?;
        lay.c_stride = cw;
        lay.c_len = mul(cw, ch)?;
        if m.n_comp == 4 {
            lay.black_stride = 8 * m.comps[3].h * mxx;
            lay.black_len = mul(lay.black_stride, 8 * m.comps[3].v * myy)?;
        }
        Ok(lay)
    }

    /// The bytes of all planes.
    fn plane_bytes(&self, n_comp: usize) -> u64 {
        let mut t = self.y_len as u64;
        if n_comp > 1 {
            t = t.saturating_add(2 * self.c_len as u64);
        }
        if n_comp == 4 {
            t = t.saturating_add(self.black_len as u64);
        }
        t
    }

    fn stride(&self, ci: usize) -> usize {
        match ci {
            0 => self.y_stride,
            1 | 2 => self.c_stride,
            _ => self.black_stride,
        }
    }
}

/// Go's `reconstructBlock` store: the block's samples, level shifted and clamped, written
/// `h` by `v` times each (flex mode) at block (`bx`, `by`).
fn put_block(
    dst: &mut [u8],
    stride: usize,
    bx: usize,
    by: usize,
    h: usize,
    v: usize,
    b: &[i32; 64],
) -> Result<(), Error> {
    let base = 8 * (by * v * stride + bx * h);
    if h == 1 && v == 1 {
        // The normal path, as Go's direct store: one 8-byte row at a time.
        for (y, src) in b.chunks_exact(8).enumerate() {
            let o = base + y * stride;
            let row = dst.get_mut(o..o + 8).ok_or_else(oob)?;
            for (d, s) in row.iter_mut().zip(src) {
                *d = s.wrapping_add(128).clamp(0, 255) as u8;
            }
        }
        return Ok(());
    }
    for y in 0..8 {
        for x in 0..8 {
            let val = b[y * 8 + x].wrapping_add(128).clamp(0, 255) as u8;
            for yy in 0..v {
                let o = base + (y * v + yy) * stride + x * h;
                let row = dst.get_mut(o..o + h).ok_or_else(oob)?;
                row.fill(val);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Colour output (reader.go `applyBlack`, `convertToRGB`; imageutil `DrawYCbCr`).

/// `color.YCbCrToRGB`.
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let yy1 = i32::from(y) * 0x10101;
    let cb1 = i32::from(cb) - 128;
    let cr1 = i32::from(cr) - 128;
    let ch = |v: i32| -> u8 {
        if (v as u32) & 0xff00_0000 == 0 {
            (v >> 16) as u8
        } else {
            (!(v >> 31)) as u8
        }
    };
    let r = ch(yy1 + 91881 * cr1);
    let g = ch(yy1 - 22554 * cb1 - 46802 * cr1);
    let b = ch(yy1 + 116130 * cb1);
    (r, g, b)
}

/// `image.YCbCr.COffset` for a zero-based rectangle.
fn c_offset(r: Subsample, cs: usize, x: usize, y: usize) -> usize {
    match r {
        Subsample::S422 => y * cs + x / 2,
        Subsample::S420 => (y / 2) * cs + x / 2,
        Subsample::S440 => (y / 2) * cs + x,
        Subsample::S411 => y * cs + x / 4,
        Subsample::S410 => (y / 2) * cs + x / 4,
        Subsample::S444 => y * cs + x,
    }
}

fn at(p: &[u8], i: usize) -> Result<u8, Error> {
    p.get(i).copied().ok_or_else(oob)
}

fn rect_of(w: usize, h: usize) -> Rect {
    Rect::new(0, 0, w as i32, h as i32)
}

/// Go's `convertToRGB`.
fn convert_to_rgb(
    m: &Meta,
    lay: &Layout,
    p: &[Vec<u8>],
    cancel: &AtomicBool,
) -> Result<Rgba, Error> {
    let (h0, h1, h2) = (m.comps[0].h, m.comps[1].h, m.comps[2].h);
    let (v0, v1, v2) = (m.comps[0].v, m.comps[1].v, m.comps[2].v);
    if h1 != h2 || !h0.is_multiple_of(h1) || v1 != v2 || !v0.is_multiple_of(v1) {
        return Err(sub_ratio());
    }
    let c_scale = h0 / h1;
    let (w, h) = visible(m);
    let stride = mul(w, 4)?;
    let mut pix = alloc(mul(stride, h)?)?;
    for y in 0..h {
        check_cancel(cancel)?;
        let yo = y * lay.y_stride;
        let co = c_offset(lay.ratio, lay.c_stride, 0, y);
        for i in 0..w {
            let o = y * stride + 4 * i;
            let px = pix.get_mut(o..o + 4).ok_or_else(oob)?;
            px[0] = at(&p[0], yo + i)?;
            px[1] = at(&p[1], co + i / c_scale)?;
            px[2] = at(&p[2], co + i / c_scale)?;
            px[3] = 255;
        }
    }
    Ok(Rgba {
        pix,
        stride,
        rect: rect_of(w, h),
    })
}

/// Go's `applyBlack`: planes Y, Cb, Cr and K into a CMYK image.
fn apply_black(m: &Meta, lay: &Layout, p: &[Vec<u8>], cancel: &AtomicBool) -> Result<Cmyk, Error> {
    let Some(transform) = m.adobe else {
        return Err(unsupported(
            "unknown color model: 4-component JPEG doesn't have Adobe APP14 metadata",
        ));
    };
    let (w, h) = visible(m);
    let stride = mul(w, 4)?;
    let mut pix = alloc(mul(stride, h)?)?;
    if transform != 0 {
        // YCbCrK: the YCbCr part to RGB (DrawYCbCr), then the alpha byte becomes 255 - K.
        for y in 0..h {
            check_cancel(cancel)?;
            for x in 0..w {
                let yi = y * lay.y_stride + x;
                let ci = c_offset(lay.ratio, lay.c_stride, x, y);
                let (r, g, b) = ycbcr_to_rgb(at(&p[0], yi)?, at(&p[1], ci)?, at(&p[2], ci)?);
                let o = y * stride + 4 * x;
                let px = pix.get_mut(o..o + 4).ok_or_else(oob)?;
                px[0] = r;
                px[1] = g;
                px[2] = b;
                px[3] = 255 - at(&p[3], y * lay.black_stride + x)?;
            }
        }
        return Ok(Cmyk {
            pix,
            stride,
            rect: rect_of(w, h),
        });
    }
    for t in 0..4 {
        let subsample = m.comps[t].h != m.comps[0].h || m.comps[t].v != m.comps[0].v;
        let sstride = lay.stride(t);
        for y in 0..h {
            check_cancel(cancel)?;
            let sy = if subsample { y / 2 } else { y };
            for x in 0..w {
                let sx = if subsample { x / 2 } else { x };
                let o = y * stride + 4 * x + t;
                let dst = pix.get_mut(o).ok_or_else(oob)?;
                *dst = 255 - at(&p[t], sy * sstride + sx)?;
            }
        }
    }
    Ok(Cmyk {
        pix,
        stride,
        rect: rect_of(w, h),
    })
}

/// The visible size: Go's `SubImage` of an empty rectangle is the zero image, so a zero width or
/// height gives a zero rectangle and zero strides.
fn visible(m: &Meta) -> (usize, usize) {
    if m.width == 0 || m.height == 0 {
        (0, 0)
    } else {
        (m.width, m.height)
    }
}

/// The end of Go's `decode`: the planes as Go's result image. On return `held` also covers any
/// RGBA or CMYK output.
fn output(m: &Meta, img: Img, held: &mut Held, cancel: &AtomicBool) -> Result<GoImage, Error> {
    let Img { lay, mut planes } = img;
    let (w, h) = visible(m);
    let rect = rect_of(w, h);
    let empty = w == 0;
    match m.n_comp {
        1 => {
            if empty {
                return Ok(GoImage::Gray(Gray {
                    pix: Vec::new(),
                    stride: 0,
                    rect,
                }));
            }
            let pix = std::mem::take(planes.get_mut(0).ok_or_else(oob)?);
            Ok(GoImage::Gray(Gray {
                pix,
                stride: lay.y_stride,
                rect,
            }))
        }
        3 if !is_rgb(m) => {
            if empty {
                return Ok(GoImage::YCbCr(YCbCr {
                    y: Vec::new(),
                    cb: Vec::new(),
                    cr: Vec::new(),
                    y_stride: 0,
                    c_stride: 0,
                    ratio: lay.ratio,
                    rect,
                }));
            }
            let cr = planes.pop().ok_or_else(oob)?;
            let cb = planes.pop().ok_or_else(oob)?;
            let y = planes.pop().ok_or_else(oob)?;
            Ok(GoImage::YCbCr(YCbCr {
                y,
                cb,
                cr,
                y_stride: lay.y_stride,
                c_stride: lay.c_stride,
                ratio: lay.ratio,
                rect,
            }))
        }
        n => {
            // The planes are an intermediate; the output is a new RGBA or CMYK buffer.
            let out = mul(mul(w, 4)?, h)? as u64;
            held.grow_by(out)?;
            let img = if n == 3 {
                GoImage::Rgba(convert_to_rgb(m, &lay, &planes, cancel)?)
            } else {
                GoImage::Cmyk(apply_black(m, &lay, &planes, cancel)?)
            };
            Ok(img)
        }
    }
}

/// `jpeg.Decode`. Memory is charged on `mem` before it is allocated: the planes when the first
/// scan starts (sized from the SOF), each component's progressive coefficients when a scan first
/// covers it, then any RGBA or CMYK output. The returned [`Decoded::held`] is the output's charge.
///
/// # Errors
/// Go's errors as in [`decode_config`]; [`Error::Alloc`] and [`Error::OutOfMemory`] from the
/// ledger and the allocator; [`Error::Cancelled`] when `cancel` is set (checked per MCU row and
/// per output row); [`Error::Panic`] where Go would index out of range.
pub fn decode(src: &[u8], mem: &Mem, cancel: &AtomicBool) -> Result<Decoded, Error> {
    let mut d = Decoder::new(src, mem.hold(), cancel);
    d.run(false)?;
    let img = d.img.take().ok_or_else(oob)?;
    let image = output(&d.m, img, &mut d.held, cancel)?;
    let mut held = d.held;
    let out = match &image {
        GoImage::Gray(g) => g.pix.len(),
        GoImage::YCbCr(i) => i.y.len() + i.cb.len() + i.cr.len(),
        GoImage::Rgba(i) => i.pix.len(),
        GoImage::Cmyk(i) => i.pix.len(),
    };
    held.shrink_to(out as u64);
    Ok(Decoded { image, held })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_block_direct_matches_flex_loop() {
        let mut b = [0i32; 64];
        for (i, v) in b.iter_mut().enumerate() {
            *v = i as i32 * 9 - 300;
        }
        let mut direct = vec![7u8; 16 * 8];
        put_block(&mut direct, 16, 1, 0, 1, 1, &b).unwrap();
        for y in 0..8 {
            for x in 0..8 {
                let want = (b[y * 8 + x] + 128).clamp(0, 255) as u8;
                assert_eq!(direct[y * 16 + 8 + x], want);
            }
            assert!(direct[y * 16..y * 16 + 8].iter().all(|&p| p == 7));
        }
        assert!(put_block(&mut direct[..40], 16, 0, 0, 1, 1, &b).is_err());
    }

    /// Go's `slowIDCT` (dct_test.go): the textbook IDCT in floating point.
    fn slow_idct(b: &[i32; 64]) -> [i32; 64] {
        let cosines = |i: usize| (std::f64::consts::PI / 16.0 * i as f64).cos();
        let alpha = |i: usize| {
            if i == 0 {
                1.0
            } else {
                std::f64::consts::SQRT_2
            }
        };
        let mut dst = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut sum = 0.0;
                for v in 0..8 {
                    for u in 0..8 {
                        sum += alpha(u)
                            * alpha(v)
                            * f64::from(b[8 * v + u])
                            * cosines(((2 * x + 1) * u) % 32)
                            * cosines(((2 * y + 1) * v) % 32);
                    }
                }
                dst[8 * y + x] = (sum / 8.0).round() as i32;
            }
        }
        dst
    }

    /// Go's `TestDCT` IDCT case: tolerance 1, and at most 8 close calls per block.
    fn check_against_slow(b: [i32; 64]) {
        let mut have = b;
        idct(&mut have);
        let want = slow_idct(&b);
        let mut close = 0;
        for i in 0..64 {
            let d = (have[i] - want[i]).abs();
            assert!(
                d <= 1,
                "block {b:?}: diff at {i}: {} vs {}",
                have[i],
                want[i]
            );
            if d >= 1 {
                close += 1;
            }
        }
        assert!(close <= 8, "block {b:?}: {close} close calls");
    }

    #[test]
    fn idct_matches_slow_idct() {
        check_against_slow([0; 64]);
        for i in 0..64 {
            let mut b = [0i32; 64];
            b[i] = 255;
            check_against_slow(b);
        }
        let mut ones = [255i32; 64];
        check_against_slow(ones);
        for i in 0..64 {
            ones[i] = 0;
            check_against_slow(ones);
            ones[i] = 255;
        }
        // xorshift, as the Go test's seeded blocks of varying sparseness.
        let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..100 {
            let mut b = [0i32; 64];
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s
            };
            let n = (next() % 64) as usize;
            for _ in 0..n {
                let i = (next() % 64) as usize;
                b[i] = (next() % 256) as i32;
            }
            check_against_slow(b);
        }
    }

    #[test]
    fn idct_dc_only() {
        let mut b = [0i32; 64];
        b[0] = 80;
        idct(&mut b);
        assert!(b.iter().all(|&v| v == 10), "{b:?}");
    }

    #[test]
    fn constants() {
        // Go: c(sqrt2inv, 8) multiplies UQ8.0 by about 181/256.
        assert_eq!(c(SQRT2INV, 8), 181);
        assert_eq!(c(COS1, 12), 4017);
        assert_eq!(c(SIN1, 12), 799);
    }

    #[test]
    fn ycbcr_grey_is_grey() {
        for y in [0u8, 1, 127, 128, 254, 255] {
            assert_eq!(ycbcr_to_rgb(y, 128, 128), (y, y, y));
        }
        assert_eq!(ycbcr_to_rgb(255, 255, 255), (255, 121, 255));
        assert_eq!(ycbcr_to_rgb(0, 0, 0), (0, 135, 0));
    }

    #[test]
    fn chroma_offsets() {
        assert_eq!(c_offset(Subsample::S444, 10, 3, 2), 23);
        assert_eq!(c_offset(Subsample::S422, 10, 3, 2), 21);
        assert_eq!(c_offset(Subsample::S420, 10, 3, 3), 11);
        assert_eq!(c_offset(Subsample::S440, 10, 3, 3), 13);
        assert_eq!(c_offset(Subsample::S411, 10, 7, 2), 21);
        assert_eq!(c_offset(Subsample::S410, 10, 7, 3), 11);
    }

    fn app14(transform: u8) -> Vec<u8> {
        let mut v = vec![0xff, 0xee, 0x00, 0x0e];
        v.extend_from_slice(b"Adobe");
        v.extend_from_slice(&[0, 100, 0, 0, 0, 0, transform]);
        v
    }

    fn sof0(w: u16, h: u16, comps: &[(u8, u8)]) -> Vec<u8> {
        let len = 8 + 3 * comps.len() as u16;
        let mut v = vec![0xff, 0xc0, (len >> 8) as u8, len as u8, 8];
        v.extend_from_slice(&[(h >> 8) as u8, h as u8, (w >> 8) as u8, w as u8]);
        v.push(comps.len() as u8);
        for &(id, hv) in comps {
            v.extend_from_slice(&[id, hv, 0]);
        }
        v
    }

    fn sos(ids: &[u8]) -> Vec<u8> {
        let len = 6 + 2 * ids.len() as u16;
        let mut v = vec![0xff, 0xda, (len >> 8) as u8, len as u8, ids.len() as u8];
        for &id in ids {
            v.extend_from_slice(&[id, 0]);
        }
        v.extend_from_slice(&[0, 63, 0]);
        v
    }

    fn jpeg_bytes(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut v = vec![0xff, 0xd8];
        for p in parts {
            v.extend_from_slice(p);
        }
        v
    }

    fn is_format(e: &Error, msg: &str) -> bool {
        matches!(e, Error::Jpeg(jpeg::Error::Format(s)) if *s == msg)
    }

    #[test]
    fn config_models() {
        let gray = jpeg_bytes(&[sof0(20, 5, &[(1, 0x21)]), sos(&[1])]);
        let c = decode_config(&gray).unwrap();
        assert_eq!(
            c,
            JpegConfig {
                model: ColorModel::Gray,
                width: 20,
                height: 5
            }
        );
        let ycc = jpeg_bytes(&[
            sof0(16, 16, &[(1, 0x22), (2, 0x11), (3, 0x11)]),
            sos(&[1, 2, 3]),
        ]);
        assert_eq!(decode_config(&ycc).unwrap().model, ColorModel::YCbCr);
        let rgb = jpeg_bytes(&[
            sof0(16, 16, &[(b'R', 0x11), (b'G', 0x11), (b'B', 0x11)]),
            sos(b"RGB"),
        ]);
        assert_eq!(decode_config(&rgb).unwrap().model, ColorModel::Rgba);
        let cmyk = jpeg_bytes(&[
            app14(2),
            sof0(9, 7, &[(1, 0x22), (2, 0x11), (3, 0x11), (4, 0x22)]),
            sos(&[1, 2, 3, 4]),
        ]);
        let c = decode_config(&cmyk).unwrap();
        assert_eq!((c.model, c.width, c.height), (ColorModel::Cmyk, 9, 7));
    }

    #[test]
    fn config_errors() {
        assert!(is_format(
            &decode_config(&[0, 0]).unwrap_err(),
            "missing SOI marker"
        ));
        assert_eq!(decode_config(&[0xff]).unwrap_err(), Error::UnexpectedEof);
        let unknown = [0xff, 0xd8, 0xff, 0x01, 0x00, 0x02];
        assert!(is_format(
            &decode_config(&unknown).unwrap_err(),
            "unknown marker"
        ));
        let sof3 = [0xff, 0xd8, 0xff, 0xc3, 0x00, 0x02];
        assert_eq!(
            decode_config(&sof3).unwrap_err(),
            Error::Jpeg(jpeg::Error::UnsupportedFeature("unknown marker"))
        );
        let no_sos = jpeg_bytes(&[sof0(8, 8, &[(1, 0x11)]), vec![0xff, 0xd9]]);
        assert!(is_format(
            &decode_config(&no_sos).unwrap_err(),
            "missing SOS marker"
        ));
        let no_sof = jpeg_bytes(&[sos(&[1])]);
        assert!(is_format(
            &decode_config(&no_sof).unwrap_err(),
            "missing SOF marker"
        ));
        let three = jpeg_bytes(&[sof0(8, 8, &[(1, 0x31)]), sos(&[1])]);
        assert_eq!(
            decode_config(&three).unwrap_err(),
            Error::Jpeg(jpeg::Error::UnsupportedFeature(
                "luma/chroma subsampling ratio"
            ))
        );
        let dup = jpeg_bytes(&[sof0(8, 8, &[(1, 0x11), (1, 0x11), (3, 0x11)])]);
        assert!(is_format(
            &decode_config(&dup).unwrap_err(),
            "repeated component identifier"
        ));
        let cmyk_bad = jpeg_bytes(&[sof0(8, 8, &[(1, 0x11), (2, 0x22), (3, 0x11), (4, 0x11)])]);
        assert_eq!(
            decode_config(&cmyk_bad).unwrap_err(),
            Error::Jpeg(jpeg::Error::UnsupportedFeature(
                "luma/chroma subsampling ratio"
            ))
        );
    }

    #[test]
    fn config_jfif_stops_at_sof() {
        // Go returns right after the SOF of a JFIF file, whatever follows it.
        let mut app0 = vec![0xff, 0xe0, 0x00, 0x07];
        app0.extend_from_slice(b"JFIF\0");
        let src = jpeg_bytes(&[app0, sof0(8, 8, &[(1, 0x11), (2, 0x11), (3, 0x11)])]);
        assert_eq!(decode_config(&src).unwrap().model, ColorModel::YCbCr);
    }

    /// The headers up to the first SOS, as `DecodeConfig` reads them.
    fn meta(src: &[u8]) -> Meta {
        let mem = Mem::with_max(0);
        let cancel = AtomicBool::new(false);
        let mut d = Decoder::new(src, mem.hold(), &cancel);
        d.run(true).unwrap();
        d.m
    }

    #[test]
    fn rgb_rules() {
        let ids = [(b'R', 0x11), (b'G', 0x11), (b'B', 0x11)];
        assert!(is_rgb(&meta(&jpeg_bytes(&[sof0(8, 8, &ids), sos(b"RGB")]))));
        // Adobe transform 1 does not stop the ids from saying RGB; JFIF does.
        let m = meta(&jpeg_bytes(&[app14(1), sof0(8, 8, &ids), sos(b"RGB")]));
        assert!(is_rgb(&m));
        let ycc = [(1, 0x11), (2, 0x11), (3, 0x11)];
        let m = meta(&jpeg_bytes(&[app14(0), sof0(8, 8, &ycc), sos(&[1, 2, 3])]));
        assert!(is_rgb(&m));
        let m = meta(&jpeg_bytes(&[app14(1), sof0(8, 8, &ycc), sos(&[1, 2, 3])]));
        assert!(!is_rgb(&m));
    }

    #[test]
    fn layout_420() {
        let m = meta(&jpeg_bytes(&[
            sof0(17, 9, &[(1, 0x22), (2, 0x11), (3, 0x11)]),
            sos(&[1, 2, 3]),
        ]));
        let lay = Layout::new(&m).unwrap();
        assert_eq!(lay.ratio, Subsample::S420);
        assert!(!lay.flex);
        assert_eq!((lay.y_stride, lay.y_len), (32, 32 * 16));
        assert_eq!((lay.c_stride, lay.c_len), (16, 16 * 8));
    }

    #[test]
    fn flex_sampling_is_accepted() {
        // Cb 1x1 and Cr 2x2 under a 2x2 Y: Go 1.27's flex mode, a 4:4:4 YCbCr result.
        let m = meta(&jpeg_bytes(&[
            sof0(16, 16, &[(1, 0x22), (2, 0x11), (3, 0x22)]),
            sos(&[1, 2, 3]),
        ]));
        let lay = Layout::new(&m).unwrap();
        assert!(lay.flex);
        assert_eq!(lay.ratio, Subsample::S444);
        assert_eq!((lay.c_stride, lay.c_len), (16, 16 * 16));
    }

    /// A decoder on `src` with no ledger room, for the bit-level tests.
    fn bit_decoder<'a>(src: &'a [u8], cancel: &'a AtomicBool) -> Decoder<'a> {
        Decoder::new(src, Mem::with_max(0).hold(), cancel)
    }

    #[test]
    fn huffman_lut_and_slow_path() {
        // Class 0, table 0: "0" -> 5 (in the look-up table) and "100000000" -> 7 (9 bits, the
        // slow path). Then the bits 0 100000000 111111.
        let mut src = vec![0x00, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 5, 7];
        src.extend_from_slice(&[0x40, 0x3f]);
        let cancel = AtomicBool::new(false);
        let mut d = bit_decoder(&src, &cancel);
        d.process_dht(19).unwrap();
        let h = &d.huff[0][0];
        assert_eq!(h.n_codes, 2);
        assert_eq!(h.lut[0x00], (5 << 8) | 2);
        assert_eq!(h.lut[0x7f], (5 << 8) | 2);
        assert_eq!(h.lut[0x80], 0);
        assert_eq!(
            (h.min_codes[8], h.max_codes[8], h.vals_indices[8]),
            (256, 256, 1)
        );
        assert_eq!(h.max_codes[1], -1);
        assert_eq!(d.decode_huffman(0, 0).unwrap(), 5);
        assert_eq!(d.decode_huffman(0, 0).unwrap(), 7);
        // Six 1 bits and the end of the data: no code.
        assert!(is_format(
            &d.decode_huffman(0, 0).unwrap_err(),
            "short Huffman data"
        ));
        // An unset table.
        assert!(is_format(
            &d.decode_huffman(1, 3).unwrap_err(),
            "uninitialized Huffman table"
        ));
    }

    #[test]
    fn huffman_table_errors() {
        let cancel = AtomicBool::new(false);
        let bad = |seg: &[u8], n: usize| {
            let mut d = bit_decoder(seg, &cancel);
            d.process_dht(n).unwrap_err()
        };
        let mut zero = vec![0x00];
        zero.extend_from_slice(&[0; 16]);
        assert!(is_format(&bad(&zero, 17), "Huffman table has zero length"));
        assert!(is_format(&bad(&zero, 16), "DHT has wrong length"));
        let mut tc = zero.clone();
        tc[0] = 0x20;
        assert!(is_format(&bad(&tc, 17), "bad Tc value"));
        let mut big = vec![0x00];
        big.extend_from_slice(&[255, 2]);
        big.extend_from_slice(&[0; 14]);
        assert!(is_format(
            &bad(&big, 17 + 257),
            "Huffman table has excessive length"
        ));
        let mut short = vec![0x00, 2];
        short.extend_from_slice(&[0; 15]);
        assert!(is_format(&bad(&short, 18), "DHT has wrong length"));
    }

    #[test]
    fn receive_extend_and_bits() {
        // 0 011 100 1 0110...
        let src = [0b0011_1001, 0b0110_0000];
        let cancel = AtomicBool::new(false);
        let mut d = bit_decoder(&src, &cancel);
        assert_eq!(d.receive_extend(0).unwrap(), 0);
        assert_eq!(d.receive_extend(1).unwrap(), -1);
        assert_eq!(d.receive_extend(3).unwrap(), -4);
        assert_eq!(d.receive_extend(3).unwrap(), 4);
        assert!(d.decode_bit().unwrap());
        assert_eq!(d.decode_bits(4).unwrap(), 0b0110);
    }

    #[test]
    fn byte_stuffing_and_unread() {
        // 0xff 0x00 is a data 0xff; 0xff before a marker ends the segment.
        let src = [0xff, 0x00, 0x12, 0xff, 0xd9];
        let cancel = AtomicBool::new(false);
        let mut d = bit_decoder(&src, &cancel);
        assert_eq!(d.read_byte_stuffed_byte().unwrap(), 0xff);
        assert_eq!(d.n_unreadable, 2);
        assert_eq!(d.read_byte_stuffed_byte().unwrap(), 0x12);
        assert_eq!(d.read_byte_stuffed_byte().unwrap_err(), missing_ff00());
        assert_eq!((d.i, d.n_unreadable), (5, 2));
        // An unread gives the two bytes back: the marker is read whole.
        d.unread_byte_stuffed_byte();
        let mut t = [0u8; 2];
        d.read_full(&mut t).unwrap();
        assert_eq!(t, [0xff, 0xd9]);
        // With 8 or more bits buffered, readFull gives back the last stuffed byte too.
        let mut d = bit_decoder(&src, &cancel);
        d.ensure_n_bits(16).unwrap();
        assert_eq!(d.bits.n, 16);
        d.read_full(&mut t).unwrap();
        assert_eq!((t, d.bits.n), ([0x12, 0xff], 8));
    }

    fn dqt(q: u8) -> Vec<u8> {
        let mut v = vec![0xff, 0xdb, 0x00, 0x43, 0x00];
        v.extend_from_slice(&[q; 64]);
        v
    }

    /// DC table 0: "0" -> 0 and "10" -> 4. AC table 0: "00" -> 0x00 (EOB), "01" -> 0xf1 (a run
    /// of 15, then 1 bit) and "10" -> 0x10 (an EOB run of 2 or 3: Go's baseline reads it).
    fn dht() -> Vec<u8> {
        let mut v = vec![0xff, 0xc4, 0x00, 0x15, 0x00, 1, 1];
        v.extend_from_slice(&[0; 14]);
        v.extend_from_slice(&[0, 4]);
        v.extend_from_slice(&[0xff, 0xc4, 0x00, 0x16, 0x10, 0, 3]);
        v.extend_from_slice(&[0; 14]);
        v.extend_from_slice(&[0x00, 0xf1, 0x10]);
        v
    }

    /// A baseline gray JPEG with the tables above, `data` as its scan and no EOI.
    fn gray_no_eoi(w: u16, h: u16, q: u8, data: &[u8]) -> Vec<u8> {
        jpeg_bytes(&[
            dqt(q),
            dht(),
            sof0(w, h, &[(1, 0x11)]),
            sos(&[1]),
            data.to_vec(),
        ])
    }

    fn gray(w: u16, h: u16, q: u8, data: &[u8]) -> Vec<u8> {
        let mut v = gray_no_eoi(w, h, q, data);
        v.extend_from_slice(&[0xff, 0xd9]);
        v
    }

    fn go_decode(src: &[u8]) -> Result<GoImage, Error> {
        let mem = Mem::with_max(1 << 20);
        decode(src, &mem, &AtomicBool::new(false)).map(|d| d.image)
    }

    #[test]
    fn decode_gray_block() {
        // DC "10" "1010" (+10, times 8 is 80: 10 after the IDCT), then EOB "00".
        let src = gray(5, 3, 8, &[0xa8]);
        let mem = Mem::with_max(1 << 20);
        let dec = decode(&src, &mem, &AtomicBool::new(false)).unwrap();
        let GoImage::Gray(g) = &dec.image else {
            panic!("not gray");
        };
        // Go keeps the whole 8x8 backing array and a 5x3 visible rectangle.
        assert_eq!(g.pix.len(), 64);
        assert_eq!(g.stride, 8);
        assert_eq!(g.rect, Rect::new(0, 0, 5, 3));
        assert!(g.pix.iter().all(|&v| v == 138), "{:?}", g.pix);
        assert_eq!(dec.held.bytes(), 64);
        let e = decode(&src, &mem, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e, Error::Cancelled);
        // The planes are charged before they are allocated.
        let small = Mem::with_max(63);
        assert!(matches!(
            decode(&src, &small, &AtomicBool::new(false)).unwrap_err(),
            Error::Alloc { .. }
        ));
    }

    #[test]
    fn run_past_zig_end_reads_no_extra_bits() {
        // Block 1: DC "0", then "01" "1" three times (+1 at zig 16, 32, 48), then "01" again: the
        // run passes zig 63 and Go stops before the extra bit (scan.go:270-275). Block 2: DC
        // "10" "1010", EOB "00". libjpeg would take the extra bit from block 2's DC code.
        let go = gray(16, 8, 1, &[0x36, 0xda, 0x8f]);
        // The same coefficients, block 1 ending with an EOB.
        let plain = gray(16, 8, 1, &[0x36, 0xca, 0x8f]);
        let a = go_decode(&go).unwrap();
        assert_eq!(a, go_decode(&plain).unwrap());
        let GoImage::Gray(g) = a else {
            panic!("not gray");
        };
        assert_eq!(g.pix[8], 129);
    }

    #[test]
    fn baseline_eob_run() {
        // Block 1: DC "0", then 0x10 "10" with the bit "0": an EOB run of 2 (scan.go:282-291),
        // which also skips block 2's AC codes. Block 2: DC "10" "1010". libjpeg reads 0x10 as a
        // plain EOB and would then decode block 2's AC from the padding.
        let go = gray(16, 8, 1, &[0x4a, 0xbf]);
        // EOB "00" in both blocks.
        let plain = gray(16, 8, 1, &[0x15, 0x1f]);
        let a = go_decode(&go).unwrap();
        assert_eq!(a, go_decode(&plain).unwrap());
        let GoImage::Gray(g) = a else {
            panic!("not gray");
        };
        assert_eq!((g.pix[0], g.pix[8]), (128, 129));
    }

    #[test]
    fn truncated_data() {
        // No EOI after a complete scan: reading the next marker runs out (Go: io.ErrUnexpectedEOF).
        let src = gray_no_eoi(16, 8, 1, &[0x4a, 0xbf]);
        assert_eq!(go_decode(&src).unwrap_err(), Error::UnexpectedEof);
        // The scan cut inside block 2's DC bits.
        let src = gray_no_eoi(16, 8, 1, &[0x4a]);
        assert!(is_format(
            &go_decode(&src).unwrap_err(),
            "short Huffman data"
        ));
        // A marker inside the scan.
        let src = gray(16, 8, 1, &[0x4a]);
        assert!(is_format(
            &go_decode(&src).unwrap_err(),
            "missing 0xff00 sequence"
        ));
    }

    #[test]
    fn zero_width_is_the_empty_image() {
        let GoImage::Gray(g) = go_decode(&gray(0, 8, 1, &[])).unwrap() else {
            panic!("not gray");
        };
        assert_eq!(
            (g.pix.len(), g.stride, g.rect),
            (0, 0, Rect::new(0, 0, 0, 0))
        );
    }

    #[test]
    fn restart_resync() {
        // TestBadRestartMarker on a 16x8 gray image with DRI 1: block 1 "0" "00" padded, RST0,
        // block 2 "10" "1010" "00". Bytes before the restart marker are skipped unless they
        // form another marker.
        let build = |infix: &[u8]| {
            let mut v = jpeg_bytes(&[
                dqt(1),
                dht(),
                sof0(16, 8, &[(1, 0x11)]),
                vec![0xff, 0xdd, 0x00, 0x04, 0x00, 0x01],
                sos(&[1]),
                vec![0x1f],
            ]);
            v.extend_from_slice(infix);
            v.extend_from_slice(&[0xff, 0xd0, 0xa8, 0xff, 0xd9]);
            v
        };
        let want = go_decode(&build(&[])).unwrap();
        let pass: [&[u8]; 6] = [
            b"\x00",
            b"\x61",
            b"\x61\x62\x63\xff\x00\x64",
            b"\xff",
            b"\xff\x00",
            b"\xff\xff\xff\x00\xff\x00\x00\xff\xff\xff",
        ];
        for infix in pass {
            assert_eq!(go_decode(&build(infix)).unwrap(), want, "{infix:?}");
        }
        let fail: [&[u8]; 3] = [b"\xff\x03", b"\xff\xd5", b"\xff\xff\xd5"];
        for infix in fail {
            assert!(
                is_format(&go_decode(&build(infix)).unwrap_err(), "bad RST marker"),
                "{infix:?}"
            );
        }
    }

    #[test]
    fn large_image_with_short_data() {
        // Go's TestLargeImageWithShortData (issue 10413): an 8192x8192 SOF and 504 bytes.
        let input: &[u8] = b"\xff\xd8\xff\xe0\x00\x10\x4a\x46\x49\x46\x00\x01\x01\x00\x00\x01\
\x00\x01\x00\x00\xff\xdb\x00\x43\x00\x10\x0b\x0c\x0e\x0c\x0a\x10\
\x0e\x89\x0e\x12\x11\x10\x13\x18\xff\xd8\xff\xe0\x00\x10\x4a\x46\
\x49\x46\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\x00\x43\
\x00\x10\x0b\x0c\x0e\x0c\x0a\x10\x0e\x0d\x0e\x12\x11\x10\x13\x18\
\x28\x1a\x18\x16\x16\x18\x31\x23\x25\x1d\x28\x3a\x33\x3d\x3c\x39\
\x33\x38\x37\x40\x48\x5c\x4e\x40\x44\x57\x45\x37\x38\x50\x6d\x51\
\x57\x5f\x62\x67\x68\x67\x3e\x4d\x71\x79\x70\x64\x78\x5c\x65\x67\
\x63\xff\xc0\x00\x0b\x08\x20\x00\x20\x00\x01\x01\x11\x00\xff\xc4\
\x00\x1f\x00\x00\x01\x05\x01\x01\x01\x01\x01\x01\x00\x00\x00\x00\
\x00\x00\x00\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\xff\
\xc4\x00\xb5\x10\x00\x02\x01\x03\x03\x02\x04\x03\x05\x05\x04\x04\
\x00\x00\x01\x7d\x01\x02\x03\x00\x04\x11\x05\x12\x21\x31\x01\x06\
\x13\x51\x61\x07\x22\x71\x14\x32\x81\x91\xa1\x08\x23\xd8\xff\xdd\
\x42\xb1\xc1\x15\x52\xd1\xf0\x24\x33\x62\x72\x82\x09\x0a\x16\x17\
\x18\x19\x1a\x25\x26\x27\x28\x29\x2a\x34\x35\x36\x37\x38\x39\x3a\
\x43\x44\x45\x46\x47\x48\x49\x4a\x53\x54\x55\x56\x57\x58\x59\x5a\
\x00\x63\x64\x65\x66\x67\x68\x69\x6a\x73\x74\x75\x76\x77\x78\x79\
\x7a\x83\x84\x85\x86\x87\x88\x89\x8a\x92\x93\x94\x95\x96\x97\x98\
\x99\x9a\xa2\xa3\xa4\xa5\xa6\xa7\xa8\xa9\xaa\xb2\xb3\xb4\xb5\xb6\
\xb7\xb8\xb9\xba\xc2\xc3\xc4\xc5\xc6\xc7\xff\xd8\xff\xe0\x00\x10\
\x4a\x46\x49\x46\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\
\x00\x43\x00\x10\x0b\x0c\x0e\x0c\x0a\x10\x0e\x0d\x0e\x12\x11\x10\
\x13\x18\x28\x1a\x18\x16\x16\x18\x31\x23\x25\x1d\xc8\xc9\xca\xd2\
\xd3\xd4\xd5\xd6\xd7\xd8\xd9\xda\xe1\xe2\xe3\xe4\xe5\xe6\xe7\xe8\
\xe9\xea\xf1\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xff\xda\x00\x08\
\x01\x01\x00\x00\x3f\x00\xb9\xeb\x50\xb0\xdb\xc8\xa8\xe4\x63\x80\
\xdd\x31\xd6\x9d\xbb\xf2\xc5\x42\x1f\x6c\x6f\xf4\x34\xdd\x3c\xfc\
\xac\xe7\x3d\x80\xa9\xcc\x87\x34\xb3\x37\xfa\x2b\x9f\x6a\xad\x63\
\x20\x36\x9f\x78\x64\x75\xe6\xab\x7d\xb2\xde\x29\x70\xd3\x20\x27\
\xde\xaf\xa4\xf0\xca\x9f\x24\xa8\xdf\x46\xa8\x24\x84\x96\xe3\x77\
\xf9\x2e\xe0\x0a\x62\x7f\xdf\xd9";
        assert_eq!(input.len(), 504);
        let mem = Mem::with_max(1 << 27);
        let e = decode(input, &mem, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Jpeg(_) | Error::UnexpectedEof), "{e:?}");
    }

    #[test]
    fn issue_78368_does_not_panic() {
        let input: &[u8] = &[
            0xff, 0xd8, 0xff, 0xdb, 0x00, 0x84, 0x00, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0xff,
            0x20, 0x20, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0x01, 0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0x20, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x20,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc2,
            0x00, 0x11, 0x08, 0x00, 0x20, 0x00, 0x20, 0x03, 0x01, 0x11, 0x00, 0x20, 0x21, 0x01,
            0xff, 0x11, 0x01, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0xff, 0xc4,
            0x00, 0x27, 0x10, 0x01, 0x00, 0x02, 0x01, 0x04, 0x01, 0x03, 0x04, 0x03, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0xff, 0xda, 0x00,
            0x08, 0x01, 0x20, 0x20, 0x20, 0x20, 0xed, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0xff, 0xee, 0x00,
            0x0e, 0x41, 0x64, 0x6f, 0x62, 0x65, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0xff,
            0xdb, 0x00, 0x43, 0x00, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0xff, 0xd9, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x20, 0x20,
        ];
        let _ = decode(input, &Mem::with_max(1 << 24), &AtomicBool::new(false));
    }

    #[test]
    fn extraneous_data_before_eoi() {
        // TestExtraneousData: junk between the scan and the EOI does not change the decode.
        let img = jpeg::Rgba {
            pix: vec![0xff, 0x00, 0x00, 0xff],
            stride: 4,
            rect: Rect::new(0, 0, 1, 1),
        };
        let enc = jpeg::encode(
            jpeg::EncodeImage::Rgba(&img),
            &jpeg::EncodeOptions::default(),
            &jpeg::Limits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(&enc[enc.len() - 2..], &[0xff, 0xd9]);
        let want = go_decode(&enc).unwrap();
        let mut s: u64 = 1;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..200 {
            let mut data = enc[..enc.len() - 2].to_vec();
            for _ in 0..next() % 10 {
                let x = (next() % 256) as u8;
                if x == 0xff {
                    data.extend_from_slice(&[0xff, 0x00]);
                } else {
                    data.push(x);
                }
            }
            data.extend_from_slice(&[0xff, 0xd9]);
            assert_eq!(go_decode(&data).unwrap(), want);
        }
    }
}
