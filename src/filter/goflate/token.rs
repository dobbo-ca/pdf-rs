//! Tokens and their histograms: Go's compress/flate token.go, the part levels 1, 6 and 9
//! reach.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::tables::{LENGTH_CODES1, OFFSET_CODES, OFFSET_CODES14};
use crate::error::Error;

/// Go's `lengthShift`.
pub(super) const LENGTH_SHIFT: u32 = 22;
/// Go's `offsetMask`.
pub(super) const OFFSET_MASK: u32 = (1 << LENGTH_SHIFT) - 1;
/// Go's `matchType`.
pub(super) const MATCH_TYPE: u32 = 1 << 30;
/// Go's `matchOffsetOnlyMask`.
pub(super) const MATCH_OFFSET_ONLY_MASK: u32 = 0xffff;
/// Go's `endBlockMarker`.
pub(super) const END_BLOCK_MARKER: usize = 256;
/// Go's `lengthCodesStart`.
pub(super) const LENGTH_CODES_START: usize = 257;
/// Go's `offsetCodeCount`.
pub(super) const OFFSET_CODE_COUNT: usize = 30;
/// Go's `literalCount`, the number of valid literal/length codes.
pub(super) const LITERAL_COUNT: usize = 286;
/// Go's `baseMatchLength`, the smallest match length of RFC 1951.
pub(super) const BASE_MATCH_LENGTH: i32 = 3;

/// Go's `lengthExtraBits`: the extra bits of length code `i + 257`.
pub(super) const LENGTH_EXTRA_BITS: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0, 0, 0, 0,
];

/// Go's `lengthBase`: the length of length code `i + 257` (the table has 29 entries, the rest
/// is zero).
pub(super) const LENGTH_BASE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224, 255, 0, 0, 0,
];

/// Go's `offsetExtraBits`.
pub(super) const OFFSET_EXTRA_BITS: [i8; 32] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14,
];

/// Go's `offsetCombined`: the extra bits of an offset code and its base, in one value.
pub(super) const OFFSET_COMBINED: [u32; 32] = [
    0x0, 0x0, 0x0, 0x0, 0x401, 0x601, 0x802, 0xc02, 0x1003, 0x1803, 0x2004, 0x3004, 0x4005, 0x6005,
    0x8006, 0xc006, 0x10007, 0x18007, 0x20008, 0x30008, 0x40009, 0x60009, 0x8000a, 0xc000a,
    0x10000b, 0x18000b, 0x20000c, 0x30000c, 0x40000d, 0x60000d, 0x0, 0x0,
];

/// Go's `tokens`: the tokens of one block and their histograms. A token is a `u32` as Go's
/// `token`: a literal below 256, the end-of-block marker 256, or a match with its length in
/// bits 22-29, its offset code in bits 16-21 and its offset in bits 0-15.
///
/// Go's `nFilled` is never above zero on the default level, so it is not modelled.
pub(super) struct Tokens {
    pub(super) extra_hist: [u16; 32],
    pub(super) off_hist: [u16; 32],
    pub(super) lit_hist: [u16; 256],
    /// Go's `n`: a `uint16`, which wraps.
    pub(super) n: u16,
    /// 65,536 entries.
    pub(super) tokens: Vec<u32>,
}

impl Tokens {
    /// Number of `u32` entries of the token array.
    pub(super) const CAP: usize = 65536;

    pub(super) fn new() -> Result<Tokens, Error> {
        let mut tokens = Vec::new();
        tokens.try_reserve_exact(Self::CAP)?;
        tokens.resize(Self::CAP, 0);
        Ok(Tokens {
            extra_hist: [0; 32],
            off_hist: [0; 32],
            lit_hist: [0; 256],
            n: 0,
            tokens,
        })
    }

    /// `Reset`: like Go, a block with no tokens leaves the histograms as they are.
    pub(super) fn reset(&mut self) {
        if self.n == 0 {
            return;
        }
        self.n = 0;
        self.lit_hist = [0; 256];
        self.extra_hist = [0; 32];
        self.off_hist = [0; 32];
    }

    /// `emitLiterals`.
    pub(super) fn emit_literals(&mut self, lit: &[u8]) {
        for &v in lit {
            self.tokens[self.n as usize] = u32::from(v);
            self.lit_hist[v as usize] = self.lit_hist[v as usize].wrapping_add(1);
            self.n = self.n.wrapping_add(1);
        }
    }

    /// `AddLiteral`.
    pub(super) fn add_literal(&mut self, lit: u8) {
        self.tokens[self.n as usize] = u32::from(lit);
        self.lit_hist[lit as usize] = self.lit_hist[lit as usize].wrapping_add(1);
        self.n = self.n.wrapping_add(1);
    }

    /// `AddMatch`: `xlength` and `xoffset` have their bases subtracted.
    pub(super) fn add_match(&mut self, xlength: u32, xoffset: u32) {
        let oc = offset_code(xoffset);
        let xoffset = xoffset | (oc << 16);
        let lc = LENGTH_CODES1[usize::from(xlength as u8)] as usize;
        self.extra_hist[lc] = self.extra_hist[lc].wrapping_add(1);
        let oi = (oc & 31) as usize;
        self.off_hist[oi] = self.off_hist[oi].wrapping_add(1);
        self.tokens[self.n as usize] = MATCH_TYPE | (xlength << LENGTH_SHIFT) | xoffset;
        self.n = self.n.wrapping_add(1);
    }

    /// `AddMatchLong`: `xlength` has no base subtracted, `xoffset` has.
    pub(super) fn add_match_long(&mut self, xlength: i32, xoffset: u32) {
        let oc = offset_code(xoffset);
        let xoffset = xoffset | (oc << 16);
        let mut xlength = xlength;
        while xlength > 0 {
            let mut xl = xlength;
            if xl > 258 {
                // We need to have at least BASE_MATCH_LENGTH left over for the next loop.
                if xl > 258 + BASE_MATCH_LENGTH {
                    xl = 258;
                } else {
                    xl = 258 - BASE_MATCH_LENGTH;
                }
            }
            xlength -= xl;
            xl -= BASE_MATCH_LENGTH;
            let lc = LENGTH_CODES1[usize::from(xl as u8)] as usize;
            self.extra_hist[lc] = self.extra_hist[lc].wrapping_add(1);
            let oi = (oc & 31) as usize;
            self.off_hist[oi] = self.off_hist[oi].wrapping_add(1);
            self.tokens[self.n as usize] = MATCH_TYPE | ((xl as u32) << LENGTH_SHIFT) | xoffset;
            self.n = self.n.wrapping_add(1);
        }
    }

    /// `AddEOB`.
    pub(super) fn add_eob(&mut self) {
        self.tokens[self.n as usize] = END_BLOCK_MARKER as u32;
        self.extra_hist[0] = self.extra_hist[0].wrapping_add(1);
        self.n = self.n.wrapping_add(1);
    }

    /// `Slice`.
    pub(super) fn slice(&self) -> &[u32] {
        &self.tokens[..self.n as usize]
    }

    /// `EstimatedBits`: an estimated minimum size for the optimal compression of the block.
    /// Float32 arithmetic without fused multiply-add, as Go on amd64.
    pub(super) fn estimated_bits(&self) -> i64 {
        let mut shannon = 0f32;
        let mut bits: i64 = 0;
        let mut n_matches: i64 = 0;
        let total = i64::from(self.n);
        if total > 0 {
            let inv_total = 1.0f32 / (total as f32);
            for &v in self.lit_hist.iter() {
                if v > 0 {
                    let n = f32::from(v);
                    shannon += (-m_fast_log2(n * inv_total)).clamp(1.0, 15.0) * n;
                }
            }
            // Just add 15 for EOB.
            shannon += 15.0;
            for (i, &v) in self.extra_hist[1..LITERAL_COUNT - 256].iter().enumerate() {
                if v > 0 {
                    let n = f32::from(v);
                    shannon += (-m_fast_log2(n * inv_total)).clamp(1.0, 15.0) * n;
                    bits += i64::from(LENGTH_EXTRA_BITS[i & 31]) * i64::from(v);
                    n_matches += i64::from(v);
                }
            }
        }
        if n_matches > 0 {
            let inv_total = 1.0f32 / (n_matches as f32);
            for (i, &v) in self.off_hist[..OFFSET_CODE_COUNT].iter().enumerate() {
                if v > 0 {
                    let n = f32::from(v);
                    shannon += (-m_fast_log2(n * inv_total)).clamp(1.0, 15.0) * n;
                    bits += i64::from(OFFSET_EXTRA_BITS[i & 31]) * i64::from(v);
                }
            }
        }
        (shannon as i64) + bits
    }

    /// `indexTokens` (the method), for the tests: clears `self` and sets it from unindexed
    /// tokens.
    #[cfg(test)]
    pub(super) fn index_tokens(&mut self, input: &[u32]) {
        self.reset();
        for &tok in input {
            if tok < MATCH_TYPE {
                self.tokens[self.n as usize] = tok & 0xff;
                let l = (tok & 0xff) as usize;
                self.lit_hist[l] = self.lit_hist[l].wrapping_add(1);
                self.n = self.n.wrapping_add(1);
                continue;
            }
            // AddMatch(uint32(tok.length()), tok.offset()&matchOffsetOnlyMask)
            let xlength = u32::from(token_length(tok));
            let mut xoffset = token_offset(tok) & MATCH_OFFSET_ONLY_MASK;
            let oc = offset_code(xoffset);
            xoffset |= oc << 16;
            let lc = LENGTH_CODES1[usize::from(xlength as u8)] as usize;
            self.extra_hist[lc] = self.extra_hist[lc].wrapping_add(1);
            let oi = (oc & 31) as usize;
            self.off_hist[oi] = self.off_hist[oi].wrapping_add(1);
            self.tokens[self.n as usize] = MATCH_TYPE | (xlength << LENGTH_SHIFT) | xoffset;
            self.n = self.n.wrapping_add(1);
        }
    }
}

/// `mFastLog2`: a fast approximation of log2(val), from https://stackoverflow.com/a/28730362.
#[allow(clippy::excessive_precision)]
pub(super) fn m_fast_log2(val: f32) -> f32 {
    let mut ux = val.to_bits() as i32;
    let mut log2 = (((ux >> 23) & 255) - 128) as f32;
    ux &= -0x7f80_0001;
    ux = ux.wrapping_add(127 << 23);
    let uval = f32::from_bits(ux as u32);
    log2 += ((-0.344_848_43_f32) * uval + 2.024_665_78_f32) * uval - 0.674_877_59_f32;
    log2
}

/// `token.offset`.
pub(super) fn token_offset(t: u32) -> u32 {
    t & OFFSET_MASK
}

/// `token.length`.
pub(super) fn token_length(t: u32) -> u8 {
    (t >> LENGTH_SHIFT) as u8
}

/// `offsetCode`: the offset code of a match offset.
pub(super) fn offset_code(off: u32) -> u32 {
    if off < 256 {
        OFFSET_CODES[usize::from(off as u8)]
    } else {
        OFFSET_CODES14[usize::from((off >> 7) as u8)]
    }
}
