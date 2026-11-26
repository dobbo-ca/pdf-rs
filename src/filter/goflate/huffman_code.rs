//! Huffman code generation: Go's compress/flate huffman_code.go.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::token::LITERAL_COUNT;
use std::sync::OnceLock;

/// Go's `hcode`: a code in bits 8 and up and its length in bits 0-7.
pub(super) type Hcode = u32;

/// `hcode.len`.
pub(super) fn hcode_len(h: Hcode) -> u8 {
    h as u8
}

/// `hcode.code64`.
pub(super) fn hcode_code64(h: Hcode) -> u64 {
    u64::from(h >> 8)
}

/// `newhcode`.
pub(super) fn newhcode(code: u16, length: u8) -> Hcode {
    u32::from(length) | (u32::from(code) << 8)
}

/// Go's `literalNode`.
#[derive(Clone, Copy, Default)]
pub(super) struct LiteralNode {
    literal: u16,
    freq: u16,
}

/// Go's `levelInfo`.
#[derive(Clone, Copy, Default)]
struct LevelInfo {
    level: i32,
    last_freq: i32,
    next_char_freq: i32,
    next_pair_freq: i32,
    needed: i32,
}

/// Go's `huffmanEncoder`. `codes` has Go's capacity (the next power of two), not its length:
/// Go's writers reslice up to the capacity, and the entries past the length stay zero.
pub(super) struct HuffmanEncoder {
    pub(super) codes: Vec<Hcode>,
    bit_count: [i32; 17],
    freqcache: [LiteralNode; LITERAL_COUNT + 1],
}

impl HuffmanEncoder {
    /// `newHuffmanEncoder`.
    pub(super) fn new(size: usize) -> HuffmanEncoder {
        // bits.Len32(size - 1)
        let c = 32 - ((size as u32).wrapping_sub(1)).leading_zeros();
        HuffmanEncoder {
            codes: vec![0; 1usize << c],
            bit_count: [0; 17],
            freqcache: [LiteralNode::default(); LITERAL_COUNT + 1],
        }
    }

    /// `bitLength`: the number of bits needed to encode `freq`.
    pub(super) fn bit_length(&self, freq: &[u16]) -> i64 {
        let mut total: i64 = 0;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                total += i64::from(f) * i64::from(hcode_len(self.codes[i]));
            }
        }
        total
    }

    /// `bitLengthRaw`: the number of bits needed to encode `b`; a byte with no code counts 1.
    pub(super) fn bit_length_raw(&self, b: &[u8]) -> i64 {
        b.iter()
            .map(|&f| i64::from(hcode_len(self.codes[f as usize]).max(1)))
            .sum()
    }

    /// `canEncodeLen`: the number of bits to encode `freq`, or `i32::MAX` if it cannot be
    /// encoded.
    pub(super) fn can_encode_len(&self, freq: &[u16]) -> i64 {
        let mut total: i64 = 0;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                let code = self.codes[i];
                if code == 0 {
                    return i64::from(i32::MAX);
                }
                total += i64::from(f) * i64::from(hcode_len(code));
            }
        }
        total
    }

    /// `generate`: rewrites the codes to be the Huffman code for the frequency count. `freq[i]`
    /// is the frequency of literal `i`, `max_bits` the most bits of any code (below 16).
    pub(super) fn generate(&mut self, freq: &[u16], max_bits: i32) {
        let HuffmanEncoder {
            codes,
            bit_count,
            freqcache,
        } = self;
        let list = &mut freqcache[..freq.len() + 1];
        // Number of non-zero literals.
        let mut count = 0usize;
        // Set list to be the set of all non-zero literals and their frequencies.
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                list[count] = LiteralNode {
                    literal: i as u16,
                    freq: f,
                };
                count += 1;
            } else {
                codes[i] = 0;
            }
        }
        list[count] = LiteralNode::default();

        if count <= 2 {
            // With two or fewer literals, everything has bit length 1. The list is in order of
            // increasing literal value.
            for i in 0..count {
                let node = list[i];
                codes[node.literal as usize] = newhcode(i as u16, 1);
            }
            return;
        }
        list[..count].sort_unstable_by_key(|a| (u32::from(a.freq) << 10) + u32::from(a.literal));

        // Get the number of literals for each bit count, and do the assignment.
        let mb = bit_counts(&mut list[..=count], bit_count, max_bits);
        assign_encoding_and_size(codes, &bit_count[..=mb], &mut list[..count]);
    }
}

/// `bitCounts`: fills `bit_count[i]` with the number of literals that are encoded with `i`
/// bits, and returns the `max_bits` it used (Go returns `h.bitCount[:maxBits+1]`).
///
/// `list` is the literals with non-zero frequency in order of increasing frequency, plus one
/// spare entry at the end for a sentinel; at least three literals. `max_bits` is below 16.
fn bit_counts(list: &mut [LiteralNode], bit_count: &mut [i32; 17], max_bits: i32) -> usize {
    let n = (list.len() - 1) as i32;
    let last = list.len() - 1;
    list[last] = LiteralNode {
        literal: u16::MAX,
        freq: u16::MAX,
    };

    // The tree can't have greater depth than n - 1, no matter what.
    let mut max_bits = max_bits;
    if max_bits > n - 1 {
        max_bits = n - 1;
    }
    let mb = max_bits as usize;

    // Create information about each of the levels. A bogus "level 0" makes level1.prev.needed
    // zero.
    let mut levels = [LevelInfo::default(); 16];
    // leaf_counts[i][j] is the number of literals at the left of the level j ancestor of the
    // rightmost node at level i.
    let mut leaf_counts = [[0i32; 16]; 16];

    for level in 1..=mb {
        // For every level, the first two items are the first two characters.
        levels[level] = LevelInfo {
            level: level as i32,
            last_freq: i32::from(list[1].freq),
            next_char_freq: i32::from(list[2].freq),
            next_pair_freq: i32::from(list[0].freq) + i32::from(list[1].freq),
            needed: 0,
        };
        leaf_counts[level][level] = 2;
        if level == 1 {
            levels[level].next_pair_freq = i32::MAX;
        }
    }

    // We need a total of 2*n - 2 items at top level and have already generated 2.
    levels[mb].needed = 2 * n - 4;

    let mut level = mb;
    while level < 16 {
        if levels[level].next_pair_freq == i32::MAX && levels[level].next_char_freq == i32::MAX {
            // We've run out of both leafs and pairs. End all calculations for this level.
            levels[level].needed = 0;
            levels[level + 1].next_pair_freq = i32::MAX;
            level += 1;
            continue;
        }

        let prev_freq = levels[level].last_freq;
        if levels[level].next_char_freq < levels[level].next_pair_freq {
            // The next item on this row is a leaf node.
            let k = leaf_counts[level][level] + 1;
            levels[level].last_freq = levels[level].next_char_freq;
            // Lower leaf_counts are the same of the previous node.
            leaf_counts[level][level] = k;
            let e = list[k as usize];
            if e.literal < u16::MAX {
                levels[level].next_char_freq = i32::from(e.freq);
            } else {
                levels[level].next_char_freq = i32::MAX;
            }
        } else {
            // The next item on this row is a pair from the previous row.
            levels[level].last_freq = levels[level].next_pair_freq;
            // Take leaf counts from the lower level, except counts[level] remains the same.
            let save = leaf_counts[level][level];
            leaf_counts[level] = leaf_counts[level - 1];
            leaf_counts[level][level] = save;
            levels[level - 1].needed = 2;
        }

        levels[level].needed -= 1;
        if levels[level].needed == 0 {
            // We've done everything we need to do for this level.
            if levels[level].level == max_bits {
                // All done!
                break;
            }
            levels[level + 1].next_pair_freq = prev_freq + levels[level].last_freq;
            level += 1;
        } else {
            // If we stole from below, move down temporarily to replenish it.
            while levels[level - 1].needed > 0 {
                level -= 1;
            }
        }
    }

    // Go panics here if the top level has not used all the leaves; that cannot happen for a
    // list of at least three literals.
    let counts = leaf_counts[mb];
    for (k, level) in (1..=mb).rev().enumerate() {
        // counts[level] - counts[level-1] is the number of literals requiring at least `k + 1`
        // bits to encode.
        bit_count[k + 1] = counts[level] - counts[level - 1];
    }
    mb
}

/// `assignEncodingAndSize`: assigns the bit counts and the codes of RFC 1951 3.2.2 to the
/// leaves.
fn assign_encoding_and_size(codes: &mut [Hcode], bit_count: &[i32], list: &mut [LiteralNode]) {
    let mut code: u16 = 0;
    let mut end = list.len();
    for (n, &bits) in bit_count.iter().enumerate() {
        code <<= 1;
        if n == 0 || bits == 0 {
            continue;
        }
        // The literals list[end-bits..end] are encoded using n bits, and get the values code,
        // code + 1, ... in literal order (not frequency order).
        let start = end - bits as usize;
        let chunk = &mut list[start..end];
        chunk.sort_unstable_by_key(|a| a.literal);
        for node in chunk.iter() {
            codes[node.literal as usize] = newhcode(reverse_bits(code, n as u8), n as u8);
            code = code.wrapping_add(1);
        }
        end = start;
    }
}

/// `reverseBits`: the `b`-bit reversal of `x`.
pub(super) fn reverse_bits(x: u16, b: u8) -> u16 {
    (x << ((16 - u32::from(b)) & 15)).reverse_bits()
}

/// `histogram` (and `histogramSplit`, which computes the same counts).
pub(super) fn histogram(b: &[u8], h: &mut [u16]) {
    let h = &mut h[..256];
    for &t in b {
        h[t as usize] = h[t as usize].wrapping_add(1);
    }
}

fn generate_fixed_literal_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(LITERAL_COUNT);
    for ch in 0..LITERAL_COUNT as u16 {
        let (bits, size): (u16, u8) = if ch < 144 {
            // size 8, 000110000 .. 10111111
            (ch + 48, 8)
        } else if ch < 256 {
            // size 9, 110010000 .. 111111111
            (ch + 400 - 144, 9)
        } else if ch < 280 {
            // size 7, 0000000 .. 0010111
            (ch - 256, 7)
        } else {
            // size 8, 11000000 .. 11000111
            (ch + 192 - 280, 8)
        };
        h.codes[ch as usize] = newhcode(reverse_bits(bits, size), size);
    }
    h
}

fn generate_fixed_offset_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(30);
    for ch in 0..30u16 {
        h.codes[ch as usize] = newhcode(reverse_bits(ch, 5), 5);
    }
    h
}

/// `fixedLiteralEncoding`.
pub(super) fn fixed_literal_encoding() -> &'static HuffmanEncoder {
    static ENC: OnceLock<HuffmanEncoder> = OnceLock::new();
    ENC.get_or_init(generate_fixed_literal_encoding)
}

/// `fixedOffsetEncoding`.
pub(super) fn fixed_offset_encoding() -> &'static HuffmanEncoder {
    static ENC: OnceLock<HuffmanEncoder> = OnceLock::new();
    ENC.get_or_init(generate_fixed_offset_encoding)
}

/// `huffOffset`: the static offset encoder of the Huffman-only blocks, which never encode
/// offset values.
pub(super) fn huff_offset() -> &'static HuffmanEncoder {
    static ENC: OnceLock<HuffmanEncoder> = OnceLock::new();
    ENC.get_or_init(|| {
        let mut freq = [0u16; 30];
        freq[0] = 1;
        let mut h = HuffmanEncoder::new(30);
        h.generate(&freq, 15);
        h
    })
}
