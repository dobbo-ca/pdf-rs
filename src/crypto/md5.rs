// Copyright 2013 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! MD5, ported from Go's crypto/md5 (`blockGeneric` in md5block.go, `digest.Write` and
//! `checkSum` in md5.go). The 64 unrolled steps of md5block.go are one loop over the same
//! constants, message-word order and rotations.

const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The index of the message word each step reads.
const X: [usize; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 1, 6, 11, 0, 5, 10, 15, 4, 9, 14, 3, 8,
    13, 2, 7, 12, 5, 8, 11, 14, 1, 4, 7, 10, 13, 0, 3, 6, 9, 12, 15, 2, 0, 7, 14, 5, 12, 3, 10, 1,
    8, 15, 6, 13, 4, 11, 2, 9,
];

/// The left rotation of each step.
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `md5.New()`: write with [`Md5::update`], read with [`Md5::finish`].
#[derive(Clone)]
pub struct Md5 {
    s: [u32; 4],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Md5::new()
    }
}

fn block(s: &mut [u32; 4], p: &[u8]) {
    for q in p.chunks_exact(64) {
        let mut x = [0u32; 16];
        for (w, c) in x.iter_mut().zip(q.chunks_exact(4)) {
            *w = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let [mut a, mut b, mut c, mut d] = *s;
        for i in 0..64 {
            let f = match i / 16 {
                0 => ((c ^ d) & b) ^ d,
                1 => ((b ^ c) & d) ^ c,
                2 => b ^ c ^ d,
                _ => c ^ (b | !d),
            };
            let t = a.wrapping_add(f).wrapping_add(x[X[i]]).wrapping_add(K[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(t.rotate_left(S[i]));
        }
        s[0] = s[0].wrapping_add(a);
        s[1] = s[1].wrapping_add(b);
        s[2] = s[2].wrapping_add(c);
        s[3] = s[3].wrapping_add(d);
    }
}

impl Md5 {
    pub fn new() -> Md5 {
        Md5 {
            s: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }

    /// `Write`.
    pub fn update(&mut self, mut p: &[u8]) {
        self.len = self.len.wrapping_add(p.len() as u64);
        if self.n > 0 {
            let take = (64 - self.n).min(p.len());
            self.buf[self.n..self.n + take].copy_from_slice(&p[..take]);
            self.n += take;
            p = &p[take..];
            if self.n == 64 {
                let b = self.buf;
                block(&mut self.s, &b);
                self.n = 0;
            }
        }
        let whole = p.len() - p.len() % 64;
        block(&mut self.s, &p[..whole]);
        let rest = &p[whole..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.n += rest.len();
    }

    /// `Sum(nil)`. The state is consumed; use [`Md5::new`] for the next hash (Go's `Reset`).
    pub fn finish(mut self) -> [u8; 16] {
        let bits = self.len.wrapping_mul(8);
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let padlen = if self.n < 56 {
            56 - self.n
        } else {
            120 - self.n
        };
        self.update(&pad[..padlen]);
        self.update(&bits.to_le_bytes());
        let mut out = [0u8; 16];
        for (o, v) in out.chunks_exact_mut(4).zip(self.s) {
            o.copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// `md5.Sum` of the concatenation of `parts` (Go crypto/md5 `Sum`; `md5(&[data])` is
/// `md5.Sum(data)`).
///
/// It takes no [`Limits`](crate::Limits): the digest state is a fixed-size value on the stack, the
/// input is only read, and nothing is allocated, so there is nothing to charge or cap. It cannot
/// fail, so there is no `# Errors` section.
pub fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        String::from_utf8(crate::object::string::hex_encode(b)).unwrap()
    }

    #[test]
    fn rfc1321_vectors() {
        let cases: [(&[u8], &str); 5] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(hex(&md5(&[input])), want);
        }
    }

    #[test]
    fn chunked_writes_match_one_write() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        let want = md5(&[&data]);
        for split in [0, 1, 55, 56, 63, 64, 65, 128, 299, 300] {
            assert_eq!(
                md5(&[&data[..split], &data[split..]]),
                want,
                "split {split}"
            );
        }
        let mut h = Md5::new();
        for b in &data {
            h.update(std::slice::from_ref(b));
        }
        assert_eq!(h.finish(), want);
    }
}
