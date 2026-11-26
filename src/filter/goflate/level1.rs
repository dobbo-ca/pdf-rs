//! The level 1 match finder (`BestSpeed`): Go's compress/flate level1.go.
// Copyright 2026 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::fast::{
    ALLOC_HISTORY, BASE_MATCH_OFFSET, BUFFER_RESET, FastGen, MAX_MATCH_OFFSET, TABLE_BITS,
    TABLE_SIZE, load32, load64, match_len_long,
};
use super::token::Tokens;
use crate::error::Error;

const PRIME5_BYTES: u64 = 889_523_592_379;

/// Bytes held by one encoder: the history and the table.
pub(super) const L1_BYTES: usize = ALLOC_HISTORY + TABLE_SIZE * 4;

/// Go's `fastEncL1`: a single small table with 5-byte hashes.
pub(super) struct FastEncL1 {
    pub(super) g: FastGen,
    table: Vec<i32>,
}

impl FastEncL1 {
    /// A new encoder, as `newFastEnc(1)`. The caller has charged `L1_BYTES`.
    pub(super) fn new() -> Result<FastEncL1, Error> {
        let mut table = Vec::new();
        table.try_reserve_exact(TABLE_SIZE)?;
        table.resize(TABLE_SIZE, 0);
        Ok(FastEncL1 {
            g: FastGen::new()?,
            table,
        })
    }

    /// `encode`: encodes `src` into `dst`.
    pub(super) fn encode(&mut self, dst: &mut Tokens, src_in: &[u8]) {
        const INPUT_MARGIN: i32 = 12 - 1;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN as usize;
        const SKIP_LOG: u32 = 5;
        const DO_EVERY: i32 = 2;

        // Protect against e.cur wraparound.
        while self.g.cur >= BUFFER_RESET {
            if self.g.hist.is_empty() {
                self.table.fill(0);
                self.g.cur = MAX_MATCH_OFFSET;
                break;
            }
            // Shift down everything in the table that isn't already too far away.
            let min_off = self.g.cur + self.g.hist.len() as i32 - MAX_MATCH_OFFSET;
            for e in self.table.iter_mut() {
                let v = *e;
                *e = if v <= min_off {
                    0
                } else {
                    v - self.g.cur + MAX_MATCH_OFFSET
                };
            }
            self.g.cur = MAX_MATCH_OFFSET;
        }

        let s0 = self.g.add_block(src_in);

        if src_in.len() < MIN_NON_LITERAL_BLOCK_SIZE {
            // We do not fill the token table. This will be picked up by the caller.
            dst.n = src_in.len() as u16;
            return;
        }

        let cur = self.g.cur;
        // Override src.
        let src: &[u8] = &self.g.hist;
        let table = &mut self.table;

        // next_emit is where in src the next emit_literals should start from.
        let mut s = s0;
        let mut next_emit = s;

        // s_limit is when to stop looking for offset/length copies.
        let s_limit = src.len() as i32 - INPUT_MARGIN;

        let mut cv = load64(src, s);

        // `break 'emit` is Go's `goto emitRemainder`.
        'emit: loop {
            let mut next_s;
            let mut t: i32;
            loop {
                let mut next_hash = hash5(cv);
                let candidate = table[next_hash];
                next_s = s + DO_EVERY + ((s - next_emit) >> SKIP_LOG);
                if next_s > s_limit {
                    break 'emit;
                }

                let mut now = load64(src, next_s);
                table[next_hash] = s + cur;
                next_hash = hash5(now);
                t = candidate - cur;
                if s - t < MAX_MATCH_OFFSET && cv as u32 == load32(src, t) {
                    table[next_hash] = next_s + cur;
                    break;
                }

                // Do one right away...
                cv = now;
                s = next_s;
                next_s += 1;
                let candidate = table[next_hash];
                now >>= 8;
                table[next_hash] = s + cur;

                t = candidate - cur;
                if s - t < MAX_MATCH_OFFSET && cv as u32 == load32(src, t) {
                    table[next_hash] = next_s + cur;
                    break;
                }
                cv = now;
                s = next_s;
            }

            // A 4-byte match has been found. src[next_emit..s] are unmatched: emit them as
            // literal bytes.
            loop {
                // Invariant: we have a 4-byte match at s, and no need to emit any literal bytes
                // prior to s.

                // Extend the 4-byte match as long as possible.
                let mut l = match_len_long(s + 4, t + 4, src) + 4;

                // Extend backwards.
                while t > 0 && s > next_emit && src[(t - 1) as usize] == src[(s - 1) as usize] {
                    s -= 1;
                    t -= 1;
                    l += 1;
                }
                if next_emit < s {
                    dst.emit_literals(&src[next_emit as usize..s as usize]);
                }

                // Save the match found.
                dst.add_match_long(l, (s - t - BASE_MATCH_OFFSET) as u32);
                s += l;
                next_emit = s;
                if next_s >= s {
                    s = next_s + 1;
                }
                if s >= s_limit {
                    // Index first pair after match end.
                    if ((s + l + 8) as usize) < src.len() {
                        let cv = load64(src, s);
                        table[hash5(cv)] = s + cur;
                    }
                    break 'emit;
                }

                // We could immediately start working at s now, but to improve compression we
                // first update the hash table at s-2 and at s.
                let mut x = load64(src, s - 2);
                let o = cur + s - 2;
                let prev_hash = hash5(x);
                table[prev_hash] = o;
                x >>= 16;
                let curr_hash = hash5(x);
                let candidate = table[curr_hash];
                table[curr_hash] = o + 2;

                t = candidate - cur;
                if s - t > MAX_MATCH_OFFSET || x as u32 != load32(src, t) {
                    cv = x >> 8;
                    s += 1;
                    break;
                }
            }
        }

        // emitRemainder:
        if (next_emit as usize) < src.len() {
            // If nothing was added, don't encode literals.
            if dst.n == 0 {
                return;
            }
            dst.emit_literals(&src[next_emit as usize..]);
        }
    }
}

/// `hashLen(u, tableBits, 5)`.
fn hash5(u: u64) -> usize {
    (((u << (64 - 40)).wrapping_mul(PRIME5_BYTES)) >> (64 - TABLE_BITS)) as usize
}
