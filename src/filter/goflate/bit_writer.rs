//! The Huffman bit writer: Go's compress/flate huffman_bit_writer.go, the part the default
//! level reaches (dynamic, Huffman-only and stored blocks).
//!
//! Output leaves through `sink`, one call for each call of Go's `w.writer.Write`, with the
//! same bytes. The first error of the sink is kept, as in Go (`w.err`), and every later write is
//! dropped.
// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file (see NOTICE).

use super::Sink;
use super::fast::MAX_STORE_BLOCK_SIZE;
use super::huffman_code::Hcode;
use super::huffman_code::{
    HuffmanEncoder, fixed_literal_encoding, fixed_offset_encoding, hcode_code64, hcode_len,
    histogram, huff_offset,
};
use super::tables::LENGTH_CODES;
use super::token::{
    END_BLOCK_MARKER, LENGTH_BASE, LENGTH_CODES_START, LENGTH_EXTRA_BITS, LITERAL_COUNT,
    MATCH_OFFSET_ONLY_MASK, OFFSET_CODE_COUNT, OFFSET_COMBINED, OFFSET_EXTRA_BITS, Tokens,
    token_length, token_offset,
};
use crate::error::Error;

/// Go's `codegenCodeCount`.
const CODEGEN_CODE_COUNT: usize = 19;
/// Go's `badCode`.
const BAD_CODE: u8 = 255;
/// Go's `maxPredefinedTokens`: the most tokens for which we check if fixed size is smaller.
const MAX_PREDEFINED_TOKENS: i64 = 250;
/// Go's `bufferFlushSize`: the buffer size after which bytes are flushed to the writer.
const BUFFER_FLUSH_SIZE: usize = 246;
/// Go's `lengthExtraBitsMinCode`.
const LENGTH_EXTRA_BITS_MIN_CODE: usize = 8;
/// Go's `offsetExtraBitsMinCode`.
const OFFSET_EXTRA_BITS_MIN_CODE: usize = 4;

/// Go's `codegenOrder`: the order in which codegen code sizes are written.
const CODEGEN_ORDER: [usize; CODEGEN_CODE_COUNT] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Writes `b` to the sink unless an error is already kept (Go's `write`).
fn emit(err: &mut Option<Error>, sink: &mut Sink<'_>, b: &[u8]) {
    if err.is_some() {
        return;
    }
    if let Err(e) = sink(b) {
        *err = Some(e);
    }
}

/// Go's `storeLE64`.
fn store_le64(b: &mut [u8], v: u64) {
    b[..8].copy_from_slice(&v.to_le_bytes());
}

/// `storedSize`: the stored size of `input`, header included, in bits, and whether it fits
/// inside a single block. Go's `nil` input (no data to store) is `None`.
fn stored_size(input: Option<&[u8]>) -> (i64, bool) {
    match input {
        Some(b) if b.len() <= MAX_STORE_BLOCK_SIZE => (((b.len() + 5) * 8) as i64, true),
        _ => (0, false),
    }
}

/// The bit accumulator and the output buffer of Go's `huffmanBitWriter`.
pub(super) struct Out {
    /// Data waiting to be written is `bytes[0..nbytes]` and then the low `nbits` of `bits`.
    bits: u64,
    pub(super) nbits: u8,
    nbytes: u8,
    bytes: [u8; 256 + 8],
    pub(super) err: Option<Error>,
}

impl Out {
    /// `writeBits`: writes `nb` bits of `b` to the stream.
    fn write_bits(&mut self, b: i32, nb: u8, sink: &mut Sink<'_>) {
        self.bits |= (b as u64) << (self.nbits & 63);
        self.nbits += nb;
        if self.nbits >= 48 {
            self.flush_bits(sink);
        }
    }

    /// `writeCode`: writes the code `c` to the stream.
    fn write_code(&mut self, c: Hcode, sink: &mut Sink<'_>) {
        self.bits |= hcode_code64(c) << (self.nbits & 63);
        self.nbits += hcode_len(c);
        if self.nbits >= 48 {
            self.flush_bits(sink);
        }
    }

    /// `flushBits`: writes the accumulated bits to the byte buffer.
    pub(super) fn flush_bits(&mut self, sink: &mut Sink<'_>) {
        let bits = self.bits;
        self.bits >>= 48;
        self.nbits -= 48;
        let mut n = usize::from(self.nbytes);

        // We overwrite, but faster...
        store_le64(&mut self.bytes[n..], bits);
        n += 6;

        if n >= BUFFER_FLUSH_SIZE {
            if self.err.is_some() {
                // Go sets its local n to 0 and returns; w.nbytes stays as it was.
                return;
            }
            emit(&mut self.err, sink, &self.bytes[..n]);
            n = 0;
        }

        self.nbytes = n as u8;
    }

    /// The end of `flush`: writes the whole bytes and the partial last byte, and clears.
    fn flush_bytes(&mut self, sink: &mut Sink<'_>) {
        let mut n = usize::from(self.nbytes);
        while self.nbits != 0 {
            self.bytes[n] = self.bits as u8;
            self.bits >>= 8;
            if self.nbits > 8 {
                // Avoid underflow.
                self.nbits -= 8;
            } else {
                self.nbits = 0;
            }
            n += 1;
        }
        self.bits = 0;
        if n > 0 {
            emit(&mut self.err, sink, &self.bytes[..n]);
        }
        self.nbytes = 0;
    }

    /// `writeBytes`: writes `input` to the stream, after the bits that wait.
    pub(super) fn write_bytes(&mut self, input: &[u8], sink: &mut Sink<'_>) {
        if self.err.is_some() {
            return;
        }
        let mut n = usize::from(self.nbytes);
        if self.nbits & 7 != 0 {
            self.err = Some(Error::Corrupt(
                "flate: writeBytes with unfinished bits".to_string(),
            ));
            return;
        }
        while self.nbits != 0 {
            self.bytes[n] = self.bits as u8;
            self.bits >>= 8;
            self.nbits -= 8;
            n += 1;
        }
        if n != 0 {
            emit(&mut self.err, sink, &self.bytes[..n]);
        }
        self.nbytes = 0;
        emit(&mut self.err, sink, input);
    }

    /// `writeTokens`: writes the tokens to the output. The codes for literal and offset
    /// encoding are supplied.
    fn write_tokens(
        &mut self,
        tokens: &[u32],
        len_codes: &[Hcode],
        off_codes: &[Hcode],
        sink: &mut Sink<'_>,
    ) {
        if self.err.is_some() {
            return;
        }
        if tokens.is_empty() {
            return;
        }

        // Only the last token should be the end-of-block marker.
        let mut tokens = tokens;
        let mut defer_eob = false;
        if tokens[tokens.len() - 1] == END_BLOCK_MARKER as u32 {
            tokens = &tokens[..tokens.len() - 1];
            defer_eob = true;
        }

        let lits = &len_codes[..256];
        let offs = &off_codes[..32];
        let lengths = &len_codes[LENGTH_CODES_START..LENGTH_CODES_START + 32];

        let bytes = &mut self.bytes;
        let err = &mut self.err;
        let mut bits = self.bits;
        let mut nbits = self.nbits;
        let mut nbytes = usize::from(self.nbytes);

        // Go inlines this four times: or the code into the accumulator and, from 48 bits,
        // move 6 bytes into the buffer and flush the buffer from `BUFFER_FLUSH_SIZE`. On an
        // earlier error Go returns without saving its local state.
        macro_rules! put {
            ($code:expr, $n:expr) => {{
                bits |= $code << (nbits & 63);
                nbits += $n;
                if nbits >= 48 {
                    store_le64(&mut bytes[nbytes..], bits);
                    bits >>= 48;
                    nbits -= 48;
                    nbytes += 6;
                    if nbytes >= BUFFER_FLUSH_SIZE {
                        if err.is_some() {
                            return;
                        }
                        emit(err, sink, &bytes[..nbytes]);
                        nbytes = 0;
                    }
                }
            }};
        }

        for &t in tokens {
            if t < 256 {
                let c = lits[t as usize];
                put!(hcode_code64(c), hcode_len(c));
                continue;
            }

            // Write the length.
            let length = token_length(t);
            let len_code = usize::from(LENGTH_CODES[usize::from(length)] & 31);
            let c = lengths[len_code];
            put!(hcode_code64(c), hcode_len(c));

            if len_code >= LENGTH_EXTRA_BITS_MIN_CODE {
                let extra_length_bits = LENGTH_EXTRA_BITS[len_code];
                let extra_length = u64::from(length.wrapping_sub(LENGTH_BASE[len_code]));
                put!(extra_length, extra_length_bits);
            }

            // Write the offset.
            let offset = token_offset(t);
            let off_code = ((offset >> 16) & 31) as usize;
            let c = offs[off_code];
            put!(hcode_code64(c), hcode_len(c));

            if off_code >= OFFSET_EXTRA_BITS_MIN_CODE {
                let offset_comb = OFFSET_COMBINED[off_code];
                let extra =
                    u64::from(offset.wrapping_sub(offset_comb >> 8) & MATCH_OFFSET_ONLY_MASK);
                put!(extra, offset_comb as u8);
            }
        }

        // Restore...
        self.bits = bits;
        self.nbits = nbits;
        self.nbytes = nbytes as u8;

        if defer_eob {
            self.write_code(len_codes[END_BLOCK_MARKER], sink);
        }
    }

    /// The literal loop of `writeBlockHuff`: writes `input` with `encoding` (256 codes). On an
    /// earlier error Go returns without saving its local state.
    fn write_literals(&mut self, encoding: &[Hcode], input: &[u8], sink: &mut Sink<'_>) {
        let bytes = &mut self.bytes;
        let err = &mut self.err;
        let mut bits = self.bits;
        let mut nbits = self.nbits;
        let mut nbytes = usize::from(self.nbytes);
        let mut input = input;

        // Unroll, write 3 codes/loop.
        while input.len() > 3 {
            // We must have at least 48 bits free.
            if nbits >= 8 {
                let n = nbits >> 3;
                store_le64(&mut bytes[nbytes..], bits);
                bits >>= (n * 8) & 63;
                nbits -= n * 8;
                nbytes += usize::from(n);
            }
            if nbytes >= BUFFER_FLUSH_SIZE {
                if err.is_some() {
                    return;
                }
                emit(err, sink, &bytes[..nbytes]);
                nbytes = 0;
            }
            let a = encoding[usize::from(input[0])];
            let b = encoding[usize::from(input[1])];
            bits |= hcode_code64(a) << (nbits & 63);
            bits |= hcode_code64(b) << ((nbits + hcode_len(a)) & 63);
            let c = encoding[usize::from(input[2])];
            nbits += hcode_len(b) + hcode_len(a);
            bits |= hcode_code64(c) << (nbits & 63);
            nbits += hcode_len(c);
            input = &input[3..];
        }

        // Remaining...
        for &t in input {
            if nbits >= 48 {
                store_le64(&mut bytes[nbytes..], bits);
                bits >>= 48;
                nbits -= 48;
                nbytes += 6;
                if nbytes >= BUFFER_FLUSH_SIZE {
                    if err.is_some() {
                        return;
                    }
                    emit(err, sink, &bytes[..nbytes]);
                    nbytes = 0;
                }
            }
            // Bit writing inlined.
            let c = encoding[usize::from(t)];
            bits |= hcode_code64(c) << (nbits & 63);
            nbits += hcode_len(c);
        }

        // Restore...
        self.bits = bits;
        self.nbits = nbits;
        self.nbytes = nbytes as u8;
    }
}

/// `generateCodegen`: the run-length encoding of RFC 1951 3.2.7 for the literal and offset
/// code lengths. The result is written into `codegen`, and the frequency of each code into
/// `codegen_freq`. Codes 0-15 are single byte codes, 16-18 are followed by additional
/// information, `BAD_CODE` is an end marker.
fn generate_codegen(
    codegen: &mut [u8; LITERAL_COUNT + OFFSET_CODE_COUNT + 1],
    codegen_freq: &mut [u16; CODEGEN_CODE_COUNT],
    num_literals: usize,
    num_offsets: usize,
    lit_codes: &[Hcode],
    off_codes: &[Hcode],
) {
    codegen_freq.fill(0);
    // Note that we are using codegen both as a temporary variable for holding a copy of the
    // code lengths, and as the place where we put the result. This is fine because the output
    // is always shorter than the input used so far.
    // Copy the concatenated code sizes to codegen. Put a marker at the end.
    for i in 0..num_literals {
        codegen[i] = hcode_len(lit_codes[i]);
    }
    for i in 0..num_offsets {
        codegen[num_literals + i] = hcode_len(off_codes[i]);
    }
    codegen[num_literals + num_offsets] = BAD_CODE;

    let mut size = codegen[0];
    let mut count: i32 = 1;
    let mut out_index = 0usize;
    let mut in_index = 1usize;
    while size != BAD_CODE {
        // INVARIANT: We have seen "count" copies of size that have not yet had output
        // generated for them.
        let next_size = codegen[in_index];
        if next_size == size {
            count += 1;
            in_index += 1;
            continue;
        }
        // We need to generate codegen indicating "count" of size.
        if size != 0 {
            codegen[out_index] = size;
            out_index += 1;
            codegen_freq[usize::from(size)] += 1;
            count -= 1;
            while count >= 3 {
                let n = count.min(6);
                codegen[out_index] = 16;
                out_index += 1;
                codegen[out_index] = (n - 3) as u8;
                out_index += 1;
                codegen_freq[16] += 1;
                count -= n;
            }
        } else {
            while count >= 11 {
                let n = count.min(138);
                codegen[out_index] = 18;
                out_index += 1;
                codegen[out_index] = (n - 11) as u8;
                out_index += 1;
                codegen_freq[18] += 1;
                count -= n;
            }
            if count >= 3 {
                // count >= 3 && count <= 10
                codegen[out_index] = 17;
                out_index += 1;
                codegen[out_index] = (count - 3) as u8;
                out_index += 1;
                codegen_freq[17] += 1;
                count = 0;
            }
        }
        count -= 1;
        while count >= 0 {
            codegen[out_index] = size;
            out_index += 1;
            codegen_freq[usize::from(size)] += 1;
            count -= 1;
        }
        // Set up invariant for next time through the loop.
        size = next_size;
        count = 1;
        in_index += 1;
    }
    // Marker indicating the end of the codegen.
    codegen[out_index] = BAD_CODE;
}

/// Go's `huffmanBitWriter`. It supports reusing Huffman tables and combines blocks if that
/// compresses better than creating a new table.
pub(super) struct HuffmanBitWriter {
    pub(super) out: Out,

    /// If set, a table for outputting only literals has been generated and offsets are invalid.
    wrote_huffman: bool,
    literal_encoding: HuffmanEncoder,
    tmp_lit_encoding: HuffmanEncoder,
    offset_encoding: HuffmanEncoder,
    codegen_encoding: HuffmanEncoder,

    /// If non-zero the Huffman table can be reused. It also indicates that an end-of-block has
    /// not yet been emitted, so if a new table is generated, an end-of-block with the previous
    /// table must be written.
    prev_header: i64,

    /// A log2 penalty reduction for creating new tables. The initial penalty is 100%. Adding 1
    /// will cut the penalty in half.
    log_new_table_penalty: u32,
    literal_freq: [u16; LENGTH_CODES_START + 32],
    offset_freq: [u16; 32],
    codegen_freq: [u16; CODEGEN_CODE_COUNT],

    /// Must have an extra space for the final symbol.
    codegen: [u8; LITERAL_COUNT + OFFSET_CODE_COUNT + 1],
}

impl HuffmanBitWriter {
    /// `newHuffmanBitWriter`, with `logNewTablePenalty` set.
    pub(super) fn new(log_new_table_penalty: u32) -> HuffmanBitWriter {
        HuffmanBitWriter {
            out: Out {
                bits: 0,
                nbits: 0,
                nbytes: 0,
                bytes: [0; 256 + 8],
                err: None,
            },
            wrote_huffman: false,
            literal_encoding: HuffmanEncoder::new(LITERAL_COUNT),
            tmp_lit_encoding: HuffmanEncoder::new(LITERAL_COUNT),
            offset_encoding: HuffmanEncoder::new(OFFSET_CODE_COUNT),
            codegen_encoding: HuffmanEncoder::new(CODEGEN_CODE_COUNT),
            prev_header: 0,
            log_new_table_penalty,
            literal_freq: [0; LENGTH_CODES_START + 32],
            offset_freq: [0; 32],
            codegen_freq: [0; CODEGEN_CODE_COUNT],
            codegen: [0; LITERAL_COUNT + OFFSET_CODE_COUNT + 1],
        }
    }

    /// `reset`: resets the state (the output is the sink of the next call).
    pub(super) fn reset(&mut self) {
        self.out.bits = 0;
        self.out.nbits = 0;
        self.out.nbytes = 0;
        self.out.err = None;
        self.prev_header = 0;
        self.wrote_huffman = false;
    }

    /// Writes the end-of-block code of the current literal encoding.
    fn write_eob(&mut self, sink: &mut Sink<'_>) {
        let c = self.literal_encoding.codes[END_BLOCK_MARKER];
        self.out.write_code(c, sink);
    }

    /// `canReuse`: whether the tables generated last can be reused for the tokens.
    fn can_reuse(&self, t: &Tokens) -> bool {
        let a = &t.off_hist[..OFFSET_CODE_COUNT];
        let b = &self.offset_encoding.codes[..OFFSET_CODE_COUNT];
        for i in 0..a.len() {
            if a[i] != 0 && b[i] == 0 {
                return false;
            }
        }

        let a = &t.extra_hist[..LITERAL_COUNT - 256];
        let b = &self.literal_encoding.codes[256..LITERAL_COUNT];
        for i in 0..a.len() {
            if a[i] != 0 && b[i] == 0 {
                return false;
            }
        }

        let a = &t.lit_hist[..256];
        let b = &self.literal_encoding.codes[..256];
        for i in 0..a.len() {
            if a[i] != 0 && b[i] == 0 {
                return false;
            }
        }
        true
    }

    /// `flush`: flushes the currently encoded data. An end-of-block is written if the current
    /// block has not ended.
    pub(super) fn flush(&mut self, sink: &mut Sink<'_>) {
        if self.out.err.is_some() {
            self.out.nbits = 0;
            return;
        }
        if self.prev_header > 0 {
            // We owe an end-of-block.
            self.write_eob(sink);
            self.prev_header = 0;
        }
        self.out.flush_bytes(sink);
    }

    /// `codegens`: the current number of non-zero codegens.
    fn codegens(&self) -> usize {
        let mut num_codegens = self.codegen_freq.len();
        while num_codegens > 4 && self.codegen_freq[CODEGEN_ORDER[num_codegens - 1]] == 0 {
            num_codegens -= 1;
        }
        num_codegens
    }

    /// `headerSize`: the size of the header with the current encodings, and its number of
    /// codegens.
    fn header_size(&self) -> (i64, usize) {
        let num_codegens = self.codegens();
        let size = 3
            + 5
            + 5
            + 4
            + (3 * num_codegens as i64)
            + self.codegen_encoding.bit_length(&self.codegen_freq)
            + i64::from(self.codegen_freq[16]) * 2
            + i64::from(self.codegen_freq[17]) * 3
            + i64::from(self.codegen_freq[18]) * 7;
        (size, num_codegens)
    }

    /// `dynamicReuseSize`: the size of dynamically encoded data in bits, with the current
    /// encodings.
    fn dynamic_reuse_size(&self) -> i64 {
        self.literal_encoding.bit_length(&self.literal_freq)
            + self.offset_encoding.bit_length(&self.offset_freq)
    }

    /// `dynamicSize`: the size of dynamically encoded data in bits, and its number of codegens.
    fn dynamic_size(&self, extra_bits: i64) -> (i64, usize) {
        let (header, num_codegens) = self.header_size();
        let size = header
            + self.literal_encoding.bit_length(&self.literal_freq)
            + self.offset_encoding.bit_length(&self.offset_freq)
            + extra_bits;
        (size, num_codegens)
    }

    /// `extraBitSize`: the number of bits that will be written as "extra" bits on matches.
    fn extra_bit_size(&self) -> i64 {
        let mut total: i64 = 0;
        for (i, &n) in self.literal_freq[LENGTH_CODES_START..LITERAL_COUNT]
            .iter()
            .enumerate()
        {
            total += i64::from(n) * i64::from(LENGTH_EXTRA_BITS[i & 31]);
        }
        for (i, &n) in self.offset_freq[..OFFSET_CODE_COUNT].iter().enumerate() {
            total += i64::from(n) * i64::from(OFFSET_EXTRA_BITS[i & 31]);
        }
        total
    }

    /// `fixedSize`: the size in bits with the fixed encoding.
    fn fixed_size(&self, extra_bits: i64) -> i64 {
        3 + fixed_literal_encoding().bit_length(&self.literal_freq)
            + fixed_offset_encoding().bit_length(&self.offset_freq)
            + extra_bits
    }

    /// `writeDynamicHeader`: writes the header of a dynamic Huffman block.
    fn write_dynamic_header(
        &mut self,
        num_literals: usize,
        num_offsets: usize,
        num_codegens: usize,
        is_eof: bool,
        sink: &mut Sink<'_>,
    ) {
        if self.out.err.is_some() {
            return;
        }
        let first_bits: i32 = if is_eof { 5 } else { 4 };
        self.out.write_bits(first_bits, 3, sink);
        self.out.write_bits(num_literals as i32 - 257, 5, sink);
        self.out.write_bits(num_offsets as i32 - 1, 5, sink);
        self.out.write_bits(num_codegens as i32 - 4, 4, sink);

        for i in 0..num_codegens {
            let value = hcode_len(self.codegen_encoding.codes[CODEGEN_ORDER[i]]);
            self.out.write_bits(i32::from(value), 3, sink);
        }

        let mut i = 0usize;
        loop {
            let code_word = self.codegen[i];
            i += 1;
            if code_word == BAD_CODE {
                break;
            }
            let c = self.codegen_encoding.codes[usize::from(code_word)];
            self.out.write_code(c, sink);

            match code_word {
                16 => {
                    self.out.write_bits(i32::from(self.codegen[i]), 2, sink);
                    i += 1;
                }
                17 => {
                    self.out.write_bits(i32::from(self.codegen[i]), 3, sink);
                    i += 1;
                }
                18 => {
                    self.out.write_bits(i32::from(self.codegen[i]), 7, sink);
                    i += 1;
                }
                _ => {}
            }
        }
    }

    /// `writeStoredHeader`: writes a stored header. If the stored block is only used for EOF,
    /// it is replaced with a fixed Huffman block.
    pub(super) fn write_stored_header(&mut self, length: usize, is_eof: bool, sink: &mut Sink<'_>) {
        if self.out.err.is_some() {
            return;
        }
        if self.prev_header > 0 {
            // We owe an end-of-block.
            self.write_eob(sink);
            self.prev_header = 0;
        }

        // To write EOF, use a fixed encoding block: 10 bits instead of 5 bytes.
        if length == 0 && is_eof {
            self.write_fixed_header(is_eof, sink);
            // End-of-block: 7 bits, value 0.
            self.out.write_bits(0, 7, sink);
            self.flush(sink);
            return;
        }

        let flag = i32::from(is_eof);
        self.out.write_bits(flag, 3, sink);
        self.flush(sink);
        self.out.write_bits(i32::from(length as u16), 16, sink);
        self.out.write_bits(i32::from(!(length as u16)), 16, sink);
    }

    /// `writeFixedHeader`: writes a fixed encoding header.
    fn write_fixed_header(&mut self, is_eof: bool, sink: &mut Sink<'_>) {
        if self.out.err.is_some() {
            return;
        }
        if self.prev_header > 0 {
            // We owe an end-of-block.
            self.write_eob(sink);
            self.prev_header = 0;
        }

        // Indicate that we are a fixed Huffman block.
        let value: i32 = if is_eof { 3 } else { 2 };
        self.out.write_bits(value, 3, sink);
    }

    /// Writes `input` as a stored block.
    fn write_stored(&mut self, input: &[u8], eof: bool, sink: &mut Sink<'_>) {
        self.write_stored_header(input.len(), eof, sink);
        self.out.write_bytes(input, sink);
    }

    /// `writeBlockDynamic`: encodes a block using a dynamic Huffman table. This should be used
    /// if the symbols used have a disproportionate histogram distribution. `input` is the data
    /// the tokens were made from.
    pub(super) fn write_block_dynamic(
        &mut self,
        tokens: &mut Tokens,
        eof: bool,
        input: Option<&[u8]>,
        sync: bool,
        sink: &mut Sink<'_>,
    ) {
        if self.out.err.is_some() {
            return;
        }

        let sync = sync || eof;
        if sync {
            tokens.add_eob();
        } else {
            // Ensure we can always write the end-of-block.
            tokens.extra_hist[0] = 1;
        }

        // We cannot reuse a pure Huffman table, and must mark as EOF.
        if (self.wrote_huffman || eof) && self.prev_header > 0 {
            // We will not try to reuse.
            self.write_eob(sink);
            self.prev_header = 0;
            self.wrote_huffman = false;
        }

        if self.prev_header > 0 && !self.can_reuse(tokens) {
            self.write_eob(sink);
            self.prev_header = 0;
        }

        let (num_literals, num_offsets) = self.index_tokens(tokens);
        let mut extra_bits: i64 = 0;
        let (ssize, storable) = stored_size(input);

        if storable || self.prev_header > 0 {
            extra_bits = self.extra_bit_size();
        }

        // Check whether we should reuse the previous Huffman table.
        if self.prev_header > 0 {
            // Estimate the size for using a new table. Use the previous header size as the
            // best estimate.
            let mut new_size = self.prev_header + tokens.estimated_bits();

            // The estimated size is calculated as an optimal table. We add a penalty to make
            // it more realistic and re-use a bit more.
            new_size += i64::from(hcode_len(self.literal_encoding.codes[END_BLOCK_MARKER]))
                + (new_size >> self.log_new_table_penalty);

            // Calculate the size for reusing the current table.
            let reuse_size = self.dynamic_reuse_size() + extra_bits;

            // Check if a new table is better.
            let size = if new_size < reuse_size {
                // Write the end-of-block we owe.
                self.write_eob(sink);
                self.prev_header = 0;
                new_size
            } else {
                reuse_size
            };

            // Small blocks can be more efficient with fixed encoding.
            if i64::from(tokens.n) < MAX_PREDEFINED_TOKENS {
                let pre_size = self.fixed_size(extra_bits) + 7;
                if pre_size < size {
                    // Check if we get a reasonable size decrease.
                    if storable && ssize <= size {
                        self.write_stored(input.unwrap_or_default(), eof, sink);
                        return;
                    }
                    self.write_fixed_header(eof, sink);
                    if !sync {
                        tokens.add_eob();
                    }
                    self.out.write_tokens(
                        tokens.slice(),
                        &fixed_literal_encoding().codes,
                        &fixed_offset_encoding().codes,
                        sink,
                    );
                    return;
                }
            }

            // Check if we get a reasonable size decrease.
            if storable && ssize <= size {
                self.write_stored(input.unwrap_or_default(), eof, sink);
                return;
            }
        }

        // We want a new block/table.
        if self.prev_header == 0 {
            self.literal_freq[END_BLOCK_MARKER] = 1;

            self.generate();
            // Generate codegen and codegen frequencies, which indicate how to encode the
            // literal encoding and the offset encoding.
            generate_codegen(
                &mut self.codegen,
                &mut self.codegen_freq,
                num_literals,
                num_offsets,
                &self.literal_encoding.codes,
                &self.offset_encoding.codes,
            );
            self.codegen_encoding.generate(&self.codegen_freq, 7);

            let (size, num_codegens) = self.dynamic_size(extra_bits);

            // Store predefined or raw, if we don't get a reasonable improvement.
            if i64::from(tokens.n) < MAX_PREDEFINED_TOKENS {
                let pre_size = self.fixed_size(extra_bits);
                if pre_size <= size {
                    // Store bytes, if we don't get an improvement.
                    if storable && ssize <= pre_size {
                        self.write_stored(input.unwrap_or_default(), eof, sink);
                        return;
                    }
                    self.write_fixed_header(eof, sink);
                    if !sync {
                        tokens.add_eob();
                    }
                    self.out.write_tokens(
                        tokens.slice(),
                        &fixed_literal_encoding().codes,
                        &fixed_offset_encoding().codes,
                        sink,
                    );
                    return;
                }
            }

            if storable && ssize <= size {
                // Store bytes, if we don't get an improvement.
                self.write_stored(input.unwrap_or_default(), eof, sink);
                return;
            }

            // Write the Huffman table.
            self.write_dynamic_header(num_literals, num_offsets, num_codegens, eof, sink);
            if !sync {
                self.prev_header = self.header_size().0;
            }
            self.wrote_huffman = false;
        }

        if sync {
            self.prev_header = 0;
        }
        // Write the tokens.
        self.out.write_tokens(
            tokens.slice(),
            &self.literal_encoding.codes,
            &self.offset_encoding.codes,
            sink,
        );
    }

    /// `indexTokens`: copies the histograms of the tokens into the frequencies. It returns the
    /// number of literal and offset codes in use.
    fn index_tokens(&mut self, t: &Tokens) -> (usize, usize) {
        self.literal_freq[..256].copy_from_slice(&t.lit_hist);
        self.literal_freq[256..288].copy_from_slice(&t.extra_hist);
        self.offset_freq = t.off_hist;

        if t.n == 0 {
            return (0, 0);
        }
        // Get the number of literals.
        let mut num_literals = self.literal_freq.len();
        while self.literal_freq[num_literals - 1] == 0 {
            num_literals -= 1;
        }
        // Get the number of offsets.
        let mut num_offsets = self.offset_freq.len();
        while num_offsets > 0 && self.offset_freq[num_offsets - 1] == 0 {
            num_offsets -= 1;
        }
        if num_offsets == 0 {
            // We haven't found a single match. If we want to go with the dynamic encoding, we
            // should count at least one offset to be sure that the offset Huffman tree could be
            // encoded.
            self.offset_freq[0] = 1;
            num_offsets = 1;
        }
        (num_literals, num_offsets)
    }

    /// `generate`: the literal and offset encodings from their histograms.
    fn generate(&mut self) {
        self.literal_encoding
            .generate(&self.literal_freq[..LITERAL_COUNT], 15);
        self.offset_encoding
            .generate(&self.offset_freq[..OFFSET_CODE_COUNT], 15);
    }

    /// `writeBlockHuff`: encodes a block of bytes as either Huffman-encoded literals or
    /// uncompressed bytes if the results gain very little from compression.
    pub(super) fn write_block_huff(
        &mut self,
        eof: bool,
        input: &[u8],
        sync: bool,
        sink: &mut Sink<'_>,
    ) {
        if self.out.err.is_some() {
            return;
        }

        // Clear the histogram.
        self.literal_freq.fill(0);
        if !self.wrote_huffman {
            self.offset_freq.fill(0);
        }

        const NUM_LITERALS: usize = END_BLOCK_MARKER + 1;
        const NUM_OFFSETS: usize = 1;

        // Estimate the size of the literal encoding.
        // 70 bytes; see https://stackoverflow.com/a/25454430
        const GUESS_HEADER_SIZE_BITS: i64 = 70 * 8;
        histogram(input, &mut self.literal_freq[..NUM_LITERALS]);
        let (ssize, storable) = stored_size(Some(input));
        if storable && input.len() > 1024 {
            // Quick check for incompressible content. The following checks if all frequencies
            // lie close to the average frequency. If so, we quickly store the data
            // uncompressed. This will typically only trigger on random data. Most other data
            // will typically exit after only a few iterations.
            let mut abs = 0f64;
            let avg = input.len() as f64 / 256.0;
            let max = (input.len() * 2) as f64;
            for &v in &self.literal_freq[..256] {
                let diff = f64::from(v) - avg;
                abs += diff * diff;
                if abs >= max {
                    break;
                }
            }
            if abs < max {
                // No chance we can compress this...
                self.write_stored(input, eof, sink);
                return;
            }
        }
        self.literal_freq[END_BLOCK_MARKER] = 1;
        self.tmp_lit_encoding
            .generate(&self.literal_freq[..NUM_LITERALS], 15);
        let mut est_bits = self
            .tmp_lit_encoding
            .can_encode_len(&self.literal_freq[..NUM_LITERALS]);
        if est_bits < i64::from(i32::MAX) {
            est_bits += self.prev_header;
            if self.prev_header == 0 {
                est_bits += GUESS_HEADER_SIZE_BITS;
            }
            est_bits += est_bits >> self.log_new_table_penalty;
        }

        // Store bytes, if we don't get a reasonable improvement.
        if storable && ssize <= est_bits {
            self.write_stored(input, eof, sink);
            return;
        }

        if self.prev_header > 0 {
            let reuse_size = self
                .literal_encoding
                .can_encode_len(&self.literal_freq[..256]);
            if est_bits < reuse_size {
                // We owe an end-of-block.
                self.write_eob(sink);
                self.prev_header = 0;
            }
        }

        if self.prev_header == 0 {
            // Use the temp encoding, so swap.
            std::mem::swap(&mut self.literal_encoding, &mut self.tmp_lit_encoding);
            // Generate codegen and codegen frequencies, which indicate how to encode the
            // literal encoding and the offset encoding.
            generate_codegen(
                &mut self.codegen,
                &mut self.codegen_freq,
                NUM_LITERALS,
                NUM_OFFSETS,
                &self.literal_encoding.codes,
                &huff_offset().codes,
            );
            self.codegen_encoding.generate(&self.codegen_freq, 7);
            let num_codegens = self.codegens();

            // Huffman.
            self.write_dynamic_header(NUM_LITERALS, NUM_OFFSETS, num_codegens, eof, sink);
            self.wrote_huffman = true;
            self.prev_header = self.header_size().0;
        }

        self.out
            .write_literals(&self.literal_encoding.codes[..256], input, sink);

        // Flush if needed to have space.
        if self.out.nbits >= 48 {
            self.out.flush_bits(sink);
        }

        if eof || sync {
            self.write_eob(sink);
            self.prev_header = 0;
            self.wrote_huffman = false;
        }
    }
}
