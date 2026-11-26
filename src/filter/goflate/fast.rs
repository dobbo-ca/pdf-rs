//! The level 6 match finder: Go's compress/flate level6.go with the `fastGen` history of
//! deflatefast.go.
// Copyright 2016 The Go Authors. All rights reserved.
// Copyright 2026 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::token::Tokens;
use crate::error::Error;

/// Go's `tableBits`: the bits of the hash tables.
pub(super) const TABLE_BITS: u32 = 15;
/// Go's `tableSize`.
pub(super) const TABLE_SIZE: usize = 1 << TABLE_BITS;
/// Go's `baseMatchOffset`: the smallest match offset.
pub(super) const BASE_MATCH_OFFSET: i32 = 1;
/// Go's `maxMatchOffset`: the largest match offset.
pub(super) const MAX_MATCH_OFFSET: i32 = 1 << 15;
/// Go's `maxMatchLength`.
const MAX_MATCH_LENGTH: i32 = 258;
/// Go's `maxStoreBlockSize`.
pub(super) const MAX_STORE_BLOCK_SIZE: usize = 65535;
/// Go's `allocHistory`: the size of the history.
pub(super) const ALLOC_HISTORY: usize = MAX_STORE_BLOCK_SIZE * 5;
/// Go's `bufferReset`: the buffer offset at which the history is reset. Go writes it as
/// `(1 << 31) - allocHistory - maxStoreBlockSize - 1`, an untyped constant; `i32::MAX` is
/// `(1 << 31) - 1`.
pub(super) const BUFFER_RESET: i32 = i32::MAX - ALLOC_HISTORY as i32 - MAX_STORE_BLOCK_SIZE as i32;

pub(super) const PRIME4_BYTES: u32 = 2_654_435_761;
const PRIME7_BYTES: u64 = 58_295_818_150_454_627;

/// Bytes held by one encoder: the history and the two tables.
pub(super) const FAST_BYTES: usize =
    ALLOC_HISTORY + TABLE_SIZE * 4 + TABLE_SIZE * std::mem::size_of::<TableEntryPrev>();

/// Go's `tableEntryPrev`: the current and the previous offset of a hash entry.
#[derive(Clone, Copy, Default)]
struct TableEntryPrev {
    cur: i32,
    prev: i32,
}

impl TableEntryPrev {
    /// `eLong.cur, eLong.prev = entry, eLong.cur`.
    fn push(&mut self, entry: i32) {
        self.prev = self.cur;
        self.cur = entry;
    }
}

/// Go's `fastGen`: the history of the fast encoders and the position of its start.
pub(super) struct FastGen {
    /// The history; its length is Go's `len(e.hist)`, its capacity `ALLOC_HISTORY`.
    pub(super) hist: Vec<u8>,
    pub(super) cur: i32,
}

impl FastGen {
    /// The `fastGen{cur: maxStoreBlockSize}` of `newFastEnc`.
    pub(super) fn new() -> Result<FastGen, Error> {
        let mut hist = Vec::new();
        hist.try_reserve_exact(ALLOC_HISTORY)?;
        Ok(FastGen {
            hist,
            cur: MAX_STORE_BLOCK_SIZE as i32,
        })
    }

    /// `fastGen.reset`: prepares for a new compression stream. Everything in the tables is
    /// moved out of reach.
    pub(super) fn reset(&mut self) {
        // We offset the current position so everything will be out of reach. If we are above
        // the buffer reset it will be cleared anyway since len(hist) == 0.
        if self.cur <= BUFFER_RESET {
            self.cur += MAX_MATCH_OFFSET + self.hist.len() as i32;
        }
        self.hist.clear();
    }

    /// `addBlock`: appends `src` to the history and returns the offset where it starts.
    pub(super) fn add_block(&mut self, src: &[u8]) -> i32 {
        // Check if we have space already.
        if self.hist.len() + src.len() > ALLOC_HISTORY {
            // Move down.
            let offset = self.hist.len() as i32 - MAX_MATCH_OFFSET;
            let from = offset as usize;
            self.hist
                .copy_within(from..from + MAX_MATCH_OFFSET as usize, 0);
            self.cur += offset;
            self.hist.truncate(MAX_MATCH_OFFSET as usize);
        }
        let s = self.hist.len() as i32;
        self.hist.extend_from_slice(src);
        s
    }
}

/// Go's `fastEncL6` with its `fastGen`.
pub(super) struct FastEncL6 {
    pub(super) g: FastGen,
    table: Vec<i32>,
    b_table: Vec<TableEntryPrev>,
}

impl FastEncL6 {
    /// A new encoder, as `newFastEnc(6)`. The caller has charged `FAST_BYTES`.
    pub(super) fn new() -> Result<FastEncL6, Error> {
        let mut table = Vec::new();
        table.try_reserve_exact(TABLE_SIZE)?;
        table.resize(TABLE_SIZE, 0);
        let mut b_table = Vec::new();
        b_table.try_reserve_exact(TABLE_SIZE)?;
        b_table.resize(TABLE_SIZE, TableEntryPrev::default());
        Ok(FastEncL6 {
            g: FastGen::new()?,
            table,
            b_table,
        })
    }

    /// `encode`: encodes `src` into `dst`.
    pub(super) fn encode(&mut self, dst: &mut Tokens, src_in: &[u8]) {
        const INPUT_MARGIN: i32 = 12 - 1;
        const MIN_NON_LITERAL_BLOCK_SIZE: usize = 1 + 1 + INPUT_MARGIN as usize;
        const SKIP_LOG: u32 = 7;
        const DO_EVERY: i32 = 1;

        // Protect against e.cur wraparound.
        while self.g.cur >= BUFFER_RESET {
            if self.g.hist.is_empty() {
                self.table.fill(0);
                self.b_table.fill(TableEntryPrev::default());
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
            for e in self.b_table.iter_mut() {
                let mut v = *e;
                if v.cur <= min_off {
                    v.cur = 0;
                    v.prev = 0;
                } else {
                    v.cur = v.cur - self.g.cur + MAX_MATCH_OFFSET;
                    if v.prev <= min_off {
                        v.prev = 0;
                    } else {
                        v.prev = v.prev - self.g.cur + MAX_MATCH_OFFSET;
                    }
                }
                *e = v;
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
        let b_table = &mut self.b_table;

        // next_emit is where in src the next emit_literals should start from.
        let mut s = s0;
        let mut next_emit = s;

        // s_limit is when to stop looking for offset/length copies.
        let s_limit = src.len() as i32 - INPUT_MARGIN;

        let mut cv = load64(src, s);
        // Repeat MUST be > 1 and within range.
        let mut repeat: i32 = 1;

        // `break 'emit` is Go's `goto emitRemainder`.
        'emit: loop {
            let mut next_s = s;
            let mut l: i32 = 0;
            let mut t: i32;
            loop {
                let mut next_hash_s = hash4(cv);
                let mut next_hash_l = hash7(cv);
                s = next_s;
                next_s = s + DO_EVERY + ((s - next_emit) >> SKIP_LOG);
                if next_s > s_limit {
                    break 'emit;
                }
                // Fetch a short+long candidate.
                let s_candidate = table[next_hash_s];
                let mut l_candidate = b_table[next_hash_l];
                let next = load64(src, next_s);
                let entry = s + cur;
                table[next_hash_s] = entry;
                b_table[next_hash_l].push(entry);

                // Calculate hashes of 'next'.
                next_hash_s = hash4(next);
                next_hash_l = hash7(next);

                t = l_candidate.cur - cur;
                if s - t < MAX_MATCH_OFFSET {
                    if cv as u32 == load32(src, t) {
                        // Long candidate matches at least 4 bytes.

                        // Store the next match.
                        table[next_hash_s] = next_s + cur;
                        b_table[next_hash_l].push(next_s + cur);

                        // Check the previous long candidate as well.
                        let t2 = l_candidate.prev - cur;
                        if s - t2 < MAX_MATCH_OFFSET && cv as u32 == load32(src, t2) {
                            l = match_len_limited(s + 4, t + 4, src) + 4;
                            let ml1 = match_len_limited(s + 4, t2 + 4, src) + 4;
                            if ml1 > l {
                                t = t2;
                                l = ml1;
                                break;
                            }
                        }
                        break;
                    }
                    // Current value did not match, but check if previous long value does.
                    t = l_candidate.prev - cur;
                    if s - t < MAX_MATCH_OFFSET && cv as u32 == load32(src, t) {
                        // Store the next match.
                        table[next_hash_s] = next_s + cur;
                        b_table[next_hash_l].push(next_s + cur);
                        break;
                    }
                }

                t = s_candidate - cur;
                if s - t < MAX_MATCH_OFFSET && cv as u32 == load32(src, t) {
                    // Found a 4 match...
                    l = match_len_limited(s + 4, t + 4, src) + 4;

                    // Look up next long candidate (at next_s).
                    l_candidate = b_table[next_hash_l];

                    // Store the next match.
                    table[next_hash_s] = next_s + cur;
                    b_table[next_hash_l].push(next_s + cur);

                    // Check repeat at s + REP_OFF.
                    const REP_OFF: i32 = 1;
                    let mut t2 = s - repeat + REP_OFF;
                    if load32(src, t2) == (cv >> (8 * REP_OFF)) as u32 {
                        let ml = match_len_limited(s + 4 + REP_OFF, t2 + 4, src) + 4;
                        if ml > l {
                            t = t2;
                            l = ml;
                            s += REP_OFF;
                            // Not worth checking more.
                            break;
                        }
                    }

                    // If the next long is a candidate, use that...
                    t2 = l_candidate.cur - cur;
                    if next_s - t2 < MAX_MATCH_OFFSET {
                        if load32(src, t2) == next as u32 {
                            let ml = match_len_limited(next_s + 4, t2 + 4, src) + 4;
                            if ml > l {
                                t = t2;
                                s = next_s;
                                l = ml;
                                // This is ok, but check previous as well.
                            }
                        }
                        // If the previous long is a candidate, use that...
                        t2 = l_candidate.prev - cur;
                        if next_s - t2 < MAX_MATCH_OFFSET && load32(src, t2) == next as u32 {
                            let ml = match_len_limited(next_s + 4, t2 + 4, src) + 4;
                            if ml > l {
                                t = t2;
                                s = next_s;
                                l = ml;
                                break;
                            }
                        }
                    }
                    break;
                }
                cv = next;
            }

            // Extend the 4-byte match as long as possible.
            if l == 0 {
                l = match_len_long(s + 4, t + 4, src) + 4;
            } else if l == MAX_MATCH_LENGTH {
                l += match_len_long(s + l, t + l, src);
            }

            // Try to locate a better match by checking the end-of-match...
            let s_at = s + l;
            if s_at < s_limit {
                // Allow some bytes at the beginning to mismatch. Sweet spot is 2/3 bytes
                // depending on input. The skipped bytes are tested in extend backwards, and
                // still picked up as part of the match if they do.
                const SKIP_BEGINNING: i32 = 2;
                let e_long = b_table[hash7(load64(src, s_at))];
                // Test current.
                let mut t2 = e_long.cur - cur - l + SKIP_BEGINNING;
                let s2 = s + SKIP_BEGINNING;
                let off = s2 - t2;
                if off < MAX_MATCH_OFFSET {
                    if off > 0 && t2 >= 0 {
                        let l2 = match_len_long(s2, t2, src);
                        if l2 > l {
                            t = t2;
                            l = l2;
                            s = s2;
                        }
                    }
                    // Test previous entry.
                    t2 = e_long.prev - cur - l + SKIP_BEGINNING;
                    let off = s2 - t2;
                    if off > 0 && off < MAX_MATCH_OFFSET && t2 >= 0 {
                        let l2 = match_len_long(s2, t2, src);
                        if l2 > l {
                            t = t2;
                            l = l2;
                            s = s2;
                        }
                    }
                }
            }

            // Extend backwards.
            while t > 0 && s > next_emit && src[(t - 1) as usize] == src[(s - 1) as usize] {
                s -= 1;
                t -= 1;
                l += 1;
            }
            if next_emit < s {
                dst.emit_literals(&src[next_emit as usize..s as usize]);
            }

            dst.add_match_long(l, (s - t - BASE_MATCH_OFFSET) as u32);
            repeat = s - t;
            s += l;
            next_emit = s;
            if next_s >= s {
                s = next_s + 1;
            }

            if s >= s_limit {
                // Index after match end.
                let mut i = next_s + 1;
                while i < src.len() as i32 - 8 {
                    let cv = load64(src, i);
                    table[hash4(cv)] = i + cur;
                    b_table[hash7(cv)].push(i + cur);
                    i += 2;
                }
                break 'emit;
            }

            // Store every long hash in-between and every second short.
            let mut i = next_s + 1;
            while i < s - 1 {
                let cv = load64(src, i);
                let t = i + cur;
                let t2 = t + 1;
                let e_long = hash7(cv);
                let e_long2 = hash7(cv >> 8);
                table[hash4(cv)] = t;
                b_table[e_long].push(t);
                b_table[e_long2].push(t2);
                i += 2;
            }
            cv = load64(src, s);
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

/// `hashLen(u, tableBits, 4)`.
fn hash4(u: u64) -> usize {
    (((u as u32).wrapping_mul(PRIME4_BYTES)) >> (32 - TABLE_BITS)) as usize
}

/// `hashLen(u, tableBits, hashLongBytes)` with `hashLongBytes` = 7.
fn hash7(u: u64) -> usize {
    (((u << (64 - 56)).wrapping_mul(PRIME7_BYTES)) >> (64 - TABLE_BITS)) as usize
}

/// `loadLE32`. An index outside `b` (where Go panics) reads as zero.
pub(super) fn load32(b: &[u8], i: i32) -> u32 {
    let Ok(i) = usize::try_from(i) else {
        return 0;
    };
    match b.get(i..i + 4) {
        Some(c) => u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
        None => 0,
    }
}

/// `loadLE64`. An index outside `b` (where Go panics) reads as zero.
pub(super) fn load64(b: &[u8], i: i32) -> u64 {
    let Ok(i) = usize::try_from(i) else {
        return 0;
    };
    match b.get(i..i + 8) {
        Some(c) => u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]),
        None => 0,
    }
}

/// `matchLen`: the length of the common prefix of `a` and `b`, at most `a.len()`.
pub(super) fn match_len(a: &[u8], b: &[u8]) -> usize {
    let mut n = 0usize;
    let mut left = a.len();
    while left >= 8 {
        let (Some(x), Some(y)) = (a.get(n..n + 8), b.get(n..n + 8)) else {
            break;
        };
        let diff = u64::from_le_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]])
            ^ u64::from_le_bytes([y[0], y[1], y[2], y[3], y[4], y[5], y[6], y[7]]);
        if diff != 0 {
            return n + (diff.trailing_zeros() >> 3) as usize;
        }
        n += 8;
        left -= 8;
    }
    while n < a.len() && n < b.len() && a[n] == b[n] {
        n += 1;
    }
    n
}

/// `matchLenLimited`: the match length between offsets `s` and `t` in `src`, at most
/// `MAX_MATCH_LENGTH - 4`. It is assumed that `s > t`, `t >= 0` and `s < len(src)`.
fn match_len_limited(s: i32, t: i32, src: &[u8]) -> i32 {
    let (Ok(s), Ok(t)) = (usize::try_from(s), usize::try_from(t)) else {
        return 0;
    };
    let end = (s + (MAX_MATCH_LENGTH as usize - 4)).min(src.len());
    match (src.get(s..end), src.get(t..)) {
        (Some(a), Some(b)) => match_len(a, b) as i32,
        _ => 0,
    }
}

/// `matchLenLong`: the match length between offsets `s` and `t` in `src`. It is assumed that
/// `s > t`, `t >= 0` and `s < len(src)`.
pub(super) fn match_len_long(s: i32, t: i32, src: &[u8]) -> i32 {
    let (Ok(s), Ok(t)) = (usize::try_from(s), usize::try_from(t)) else {
        return 0;
    };
    match (src.get(s..), src.get(t..)) {
        (Some(a), Some(b)) => match_len(a, b) as i32,
        _ => 0,
    }
}
