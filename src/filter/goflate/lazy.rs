//! Level 9 (`BestCompression`): Go's compress/flate deflate.go `fillDeflate`, `findMatch`,
//! `deflateLazy` and the helpers they call, with the level 9 `compressionLevel`. Go runs levels
//! 7-9 through this code; this port offers only 9.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::Sink;
use super::deflate::Compressor;
use super::fast::{PRIME4_BYTES, load32, match_len};
use super::huffman_code::HuffmanEncoder;
use super::tables::LENGTH_CODES;
use super::token::{LENGTH_EXTRA_BITS, OFFSET_EXTRA_BITS, offset_code};
use crate::error::Error;

/// Go's `windowSize`.
pub(super) const WINDOW_SIZE: i32 = 1 << 15;
/// Go's `windowMask`.
const WINDOW_MASK: i32 = WINDOW_SIZE - 1;
/// Go's `minMatchLength`: the smallest match the compressor looks for.
const MIN_MATCH_LENGTH: i32 = 4;
/// Go's `maxMatchLength`.
const MAX_MATCH_LENGTH: i32 = 258;
/// Go's `minOffsetSize`.
const MIN_OFFSET_SIZE: i32 = 1;
/// Go's `maxFlateBlockTokens`: the most tokens of one block.
const MAX_FLATE_BLOCK_TOKENS: u16 = 1 << 15;
/// Go's `hashBits`.
const HASH_BITS: u32 = 17;
/// Go's `hashSize`.
const HASH_SIZE: usize = 1 << HASH_BITS;
/// Go's `maxHashOffset`.
pub(super) const MAX_HASH_OFFSET: i32 = 1 << 28;

/// Level 9's `compressionLevel` (`good`, 32, is not read by `deflateLazy`): don't try to find
/// a later, better match above this length.
const LAZY: i32 = 258;
/// Stop looking for a better match above this length.
const NICE: i32 = 258;
/// The most hash chain entries to search. Being above 100, it turns on the literal cost
/// estimate of `deflateLazy` and the gain test of `findMatch`.
const CHAIN: i32 = 1024;

/// Bytes held by level 9: the window, the hash chains and a few KiB for the literal estimator.
pub(super) const LAZY_BYTES: usize =
    2 * WINDOW_SIZE as usize + (HASH_SIZE + WINDOW_SIZE as usize) * 4 + 4096;

/// Go's `advancedState`, with the `compressor` fields only levels 7-9 use: `blockStart`,
/// `byteAvailable` and the literal cost estimator `h`.
pub(super) struct Lazy {
    length: i32,
    offset: i32,
    max_insert_index: i32,
    chain_head: i32,
    pub(super) hash_offset: i32,
    /// Consecutive literal count; wraps after 64 KiB.
    literal_counter: u16,
    /// Unprocessed data is `window[index..window_end]`.
    index: i32,
    /// `hash_head[h]` is the largest input index (plus `hash_offset`) with hash `h`. If it is in
    /// the current window, `hash_prev[i & WINDOW_MASK]` is the previous index with that hash.
    hash_head: Vec<i32>,
    hash_prev: Vec<i32>,
    /// Window index where the current tokens start.
    block_start: i32,
    /// Whether `window[index - 1]` still needs to be processed.
    byte_available: bool,
    /// Go's `d.h`. Go sizes it for `maxFlateBlockTokens` symbols; only the 256 literals are used.
    h: HuffmanEncoder,
}

impl Lazy {
    /// The `advancedState` of `init` and `initDeflate`. The caller has charged `LAZY_BYTES`.
    pub(super) fn new() -> Result<Box<Lazy>, Error> {
        let mut hash_head = Vec::new();
        hash_head.try_reserve_exact(HASH_SIZE)?;
        hash_head.resize(HASH_SIZE, 0);
        let mut hash_prev = Vec::new();
        hash_prev.try_reserve_exact(WINDOW_SIZE as usize)?;
        hash_prev.resize(WINDOW_SIZE as usize, 0);
        Ok(Box::new(Lazy {
            length: MIN_MATCH_LENGTH - 1,
            offset: 0,
            max_insert_index: 0,
            chain_head: -1,
            hash_offset: 1,
            literal_counter: 0,
            index: 0,
            hash_head,
            hash_prev,
            block_start: 0,
            byte_available: false,
            h: HuffmanEncoder::new(256),
        }))
    }

    /// The level 7-9 part of `compressor.reset`.
    pub(super) fn reset(&mut self) {
        self.chain_head = -1;
        self.hash_head.fill(0);
        self.hash_prev.fill(0);
        self.hash_offset = 1;
        self.index = 0;
        self.block_start = 0;
        self.byte_available = false;
        self.length = MIN_MATCH_LENGTH - 1;
        self.offset = 0;
        self.literal_counter = 0;
        self.max_insert_index = 0;
    }

    /// Inserts `index` into the hash chains if it is below `max_insert_index` (Go writes this
    /// inline in three places).
    fn insert(&mut self, window: &[u8]) {
        if self.index < self.max_insert_index {
            let h = hash4(window, self.index);
            let ch = self.hash_head[h];
            self.chain_head = ch;
            self.hash_prev[(self.index & WINDOW_MASK) as usize] = ch;
            self.hash_head[h] = self.index + self.hash_offset;
        }
    }
}

/// `hash4(window[i:])`: the hash of the 4 bytes at `i`, as `hash4u(loadLE32(b, 0), hashBits)`.
/// Go's `bulkHash4` computes the same values.
fn hash4(window: &[u8], i: i32) -> usize {
    (load32(window, i).wrapping_mul(PRIME4_BYTES) >> (32 - HASH_BITS)) as usize
}

impl Compressor {
    /// `fillDeflate`: adds `b` to the window, shifting it down by `WINDOW_SIZE` first if the
    /// unprocessed data is near its end. Returns the bytes copied.
    pub(super) fn fill_deflate(&mut self, b: &[u8]) -> usize {
        if let Some(s) = &mut self.lazy
            && s.index >= 2 * WINDOW_SIZE - (MIN_MATCH_LENGTH + MAX_MATCH_LENGTH)
        {
            // Shift the window by WINDOW_SIZE.
            self.window
                .copy_within(WINDOW_SIZE as usize..2 * WINDOW_SIZE as usize, 0);
            s.index -= WINDOW_SIZE;
            self.window_end -= WINDOW_SIZE;
            if s.block_start >= WINDOW_SIZE {
                s.block_start -= WINDOW_SIZE;
            } else {
                s.block_start = i32::MAX;
            }
            s.hash_offset += WINDOW_SIZE;
            if s.hash_offset > MAX_HASH_OFFSET {
                let delta = s.hash_offset - 1;
                s.hash_offset -= delta;
                s.chain_head -= delta;
                for v in s.hash_prev.iter_mut() {
                    *v = (*v - delta).max(0);
                }
                for v in s.hash_head.iter_mut() {
                    *v = (*v - delta).max(0);
                }
            }
        }
        self.fill_block(b)
    }

    /// `writeBlock` with `eof` false: writes the tokens as a dynamic block whose window starts
    /// at `block_start` and ends at `index`.
    fn write_block(&mut self, s: &mut Lazy, index: i32, sink: &mut Sink<'_>) -> Option<Error> {
        if index > 0 {
            let window = if s.block_start <= index {
                Some(&self.window[s.block_start as usize..index as usize])
            } else {
                None
            };
            s.block_start = index;
            self.w
                .write_block_dynamic(&mut self.tokens, false, window, self.sync, sink);
            return self.w.out.err.clone();
        }
        None
    }

    /// Go's repeated `if d.tokens.n == maxFlateBlockTokens { d.err = d.writeBlock(..);
    /// d.tokens.Reset() }`. False if the write failed (the error is in `self.err`).
    fn write_if_full(&mut self, s: &mut Lazy, sink: &mut Sink<'_>) -> bool {
        if self.tokens.n == MAX_FLATE_BLOCK_TOKENS {
            let index = s.index;
            self.err = self.write_block(s, index, sink);
            if self.err.is_some() {
                return false;
            }
            self.tokens.reset();
        }
        true
    }

    /// `findMatch`: the longest match at `pos` in the hash chain that starts at `prev_head`,
    /// searching up to `CHAIN` entries, as `(length, offset)`.
    fn find_match(&self, s: &Lazy, pos: i32, prev_head: i32, lookahead: i32) -> Option<(i32, i32)> {
        let min_match_look = lookahead.min(MAX_MATCH_LENGTH);
        let win = &self.window[..(pos + min_match_look) as usize];

        // We quit when we get a match that's at least nice long.
        let nice = NICE.min(win.len() as i32 - pos);

        let mut tries = CHAIN;
        let mut length = MIN_MATCH_LENGTH - 1;
        let mut offset = 0;
        let mut ok = false;

        let mut w_end = win[(pos + length) as usize];
        let w_pos = &win[pos as usize..];
        let min_index = (pos - WINDOW_SIZE).max(0);

        // Minimum gain to accept a match.
        let mut c_gain: i64 = 4;
        // Base is 4 bytes with an additional cost. Matches must be better than this.
        const BASE_COST: i64 = 3;

        let mut i = prev_head;
        while tries > 0 {
            if w_end == win[(i + length) as usize] {
                let n = match_len(&win[i as usize..(i + min_match_look) as usize], w_pos) as i32;
                if n > length {
                    // CHAIN >= 100: estimate the gain of the match over literals.
                    let off_extra = OFFSET_EXTRA_BITS[offset_code((pos - i) as u32) as usize];
                    let len_extra =
                        LENGTH_EXTRA_BITS[LENGTH_CODES[((n - 3) & 255) as usize] as usize];
                    let new_gain = s.h.bit_length_raw(&w_pos[..n as usize])
                        - i64::from(off_extra)
                        - BASE_COST
                        - i64::from(len_extra);
                    if new_gain > c_gain {
                        c_gain = new_gain;
                        length = n;
                        offset = pos - i;
                        ok = true;
                        if n >= nice {
                            // The match is good enough that we don't try to find a better one.
                            break;
                        }
                        w_end = win[(pos + n) as usize];
                    }
                }
            }
            // next:
            if i <= min_index {
                // hash_prev[i & WINDOW_MASK] has already been overwritten, so stop now.
                break;
            }
            i = s.hash_prev[(i & WINDOW_MASK) as usize] - s.hash_offset;
            if i < min_index {
                break;
            }
            tries -= 1;
        }
        ok.then_some((length, offset))
    }

    /// `tryBetterMatchAtEnd`: checks whether a better match starts at the end of the previous
    /// match and, if so, emits the skipped literals and returns the adjusted match.
    fn try_better_match_at_end(
        &mut self,
        s: &mut Lazy,
        prev_length: i32,
        prev_offset: i32,
        lookahead: i32,
        sink: &mut Sink<'_>,
    ) -> (i32, i32) {
        // We start checking at CHECK_OFF from the current match position. This allows up to
        // two additional literals, but that could be compensated by a higher quality match.
        const CHECK_OFF: i32 = 2;

        if prev_length >= MAX_MATCH_LENGTH - CHECK_OFF {
            return (prev_length, prev_offset);
        }
        let prev_index = s.index - 1;
        if prev_index + prev_length >= s.max_insert_index {
            return (prev_length, prev_offset);
        }

        let end = lookahead.min(MAX_MATCH_LENGTH + CHECK_OFF) + prev_index;
        let min_index = (s.index - WINDOW_SIZE).max(0);

        let h = hash4(&self.window, prev_index + prev_length);
        let ch2 = s.hash_head[h] - s.hash_offset - prev_length;
        if prev_index - ch2 == prev_offset || ch2 <= min_index + CHECK_OFF {
            return (prev_length, prev_offset);
        }

        let length = match_len(
            &self.window[(prev_index + CHECK_OFF) as usize..end as usize],
            &self.window[(ch2 + CHECK_OFF) as usize..],
        ) as i32;
        if length <= prev_length {
            return (prev_length, prev_offset);
        }

        let mut prev_length = length;
        let prev_offset = prev_index - ch2;

        for i in (0..CHECK_OFF).rev() {
            if prev_length >= MAX_MATCH_LENGTH
                || self.window[(prev_index + i) as usize] != self.window[(ch2 + i) as usize]
            {
                for j in 0..=i {
                    self.tokens
                        .add_literal(self.window[(prev_index + j) as usize]);
                    if !self.write_if_full(s, sink) {
                        return (prev_length, prev_offset);
                    }
                    s.index += 1;
                    s.insert(&self.window);
                }
                break;
            }
            prev_length += 1;
        }
        (prev_length, prev_offset)
    }

    /// `skipLiterals`: emits extra literals in long runs of incompressible data, skipping
    /// match searches that would fail. False on a write error.
    fn skip_literals(&mut self, s: &mut Lazy, sink: &mut Sink<'_>) -> bool {
        let n = i32::from(s.literal_counter) - CHAIN;
        if n <= 0 {
            return true;
        }
        let n = 1 + (n >> 6);
        for _ in 0..n {
            if s.index >= self.window_end - 1 {
                break;
            }
            self.tokens.add_literal(self.window[(s.index - 1) as usize]);
            if !self.write_if_full(s, sink) {
                return false;
            }
            s.insert(&self.window);
            s.index += 1;
        }
        self.tokens.add_literal(self.window[(s.index - 1) as usize]);
        s.byte_available = false;
        self.write_if_full(s, sink)
    }

    /// `deflateLazy`: encodes the window with lazy matching, deferring a match to see whether
    /// the next position gives a better one.
    pub(super) fn deflate_lazy(&mut self, sink: &mut Sink<'_>) {
        if let Some(mut s) = self.lazy.take() {
            self.deflate_lazy_state(&mut s, sink);
            self.lazy = Some(s);
        }
    }

    fn deflate_lazy_state(&mut self, s: &mut Lazy, sink: &mut Sink<'_>) {
        if self.window_end - s.index < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH && !self.sync {
            return;
        }
        if self.window_end != s.index {
            // CHAIN > 100: get the literal Huffman coder that estimates the cost of a literal.
            let mut tmp = [0u16; 256];
            let to_index = &self.window[s.index as usize..self.window_end as usize];
            let to_index = &to_index[..to_index.len().min(MAX_FLATE_BLOCK_TOKENS as usize)];
            for &v in to_index {
                tmp[v as usize] += 1;
            }
            s.h.generate(&tmp, 15);
        }

        s.max_insert_index = self.window_end - (MIN_MATCH_LENGTH - 1);

        loop {
            let lookahead = self.window_end - s.index;
            if lookahead < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH {
                if !self.sync {
                    return;
                }
                if lookahead == 0 {
                    // Flush the current output block if any.
                    if s.byte_available {
                        // There is still one pending token that needs to be flushed.
                        self.tokens.add_literal(self.window[(s.index - 1) as usize]);
                        s.byte_available = false;
                    }
                    if self.tokens.n > 0 {
                        let index = s.index;
                        self.err = self.write_block(s, index, sink);
                        if self.err.is_some() {
                            return;
                        }
                        self.tokens.reset();
                    }
                    return;
                }
            }
            s.insert(&self.window);
            let mut prev_length = s.length;
            let mut prev_offset = s.offset;
            s.length = MIN_MATCH_LENGTH - 1;
            s.offset = 0;
            let min_index = (s.index - WINDOW_SIZE).max(0);

            if s.chain_head - s.hash_offset >= min_index
                && lookahead > prev_length
                && prev_length < LAZY
                && let Some((length, offset)) =
                    self.find_match(s, s.index, s.chain_head - s.hash_offset, lookahead)
            {
                s.length = length;
                s.offset = offset;
            }

            if prev_length >= MIN_MATCH_LENGTH && s.length <= prev_length {
                (prev_length, prev_offset) =
                    self.try_better_match_at_end(s, prev_length, prev_offset, lookahead, sink);
                if self.err.is_some() {
                    return;
                }

                // There was a match at the previous step, and the current match is not
                // better. Output the previous match.
                self.tokens.add_match(
                    (prev_length - 3) as u32,
                    (prev_offset - MIN_OFFSET_SIZE) as u32,
                );

                // Insert in the hash table all strings up to the end of the match. index and
                // index-1 are already inserted. If there is not enough lookahead, the last two
                // strings are not inserted into the hash table.
                let new_index = s.index + prev_length - 1;
                let end = new_index.min(s.max_insert_index) + MIN_MATCH_LENGTH - 1;
                let start = (s.index + 1).min(s.max_insert_index);
                for di in start..end - (MIN_MATCH_LENGTH - 1) {
                    let h = hash4(&self.window, di);
                    s.hash_prev[(di & WINDOW_MASK) as usize] = s.hash_head[h];
                    s.hash_head[h] = di + s.hash_offset;
                }

                s.index = new_index;
                s.byte_available = false;
                s.length = MIN_MATCH_LENGTH - 1;
                if !self.write_if_full(s, sink) {
                    return;
                }
                s.literal_counter = 0;
                continue;
            }
            if s.length >= MIN_MATCH_LENGTH {
                s.literal_counter = 0;
            }
            if s.byte_available {
                s.literal_counter = s.literal_counter.wrapping_add(1);
                self.tokens.add_literal(self.window[(s.index - 1) as usize]);
                if !self.write_if_full(s, sink) {
                    return;
                }
                s.index += 1;
                if !self.skip_literals(s, sink) {
                    return;
                }
            } else {
                s.index += 1;
                s.byte_available = true;
            }
        }
    }
}
