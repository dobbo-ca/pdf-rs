// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! AES-128, AES-192 and AES-256 and CBC mode, ported from Go's crypto/internal/fips140/aes
//! (`aes_generic.go`: `expandKeyGeneric`, `encryptBlockGeneric`, `decryptBlockGeneric`) and
//! crypto/cipher (`cbc.go`). The tables are in [`super::aes_tables`].

use super::aes_tables::{SBOX0, SBOX1, TD0, TD1, TD2, TD3, TE0, TE1, TE2, TE3};
use crate::error::Error;

/// The block size, `aes.BlockSize`.
pub const BLOCK_SIZE: usize = 16;

/// Powers of x mod the AES polynomial (`powx`).
const POWX: [u8; 16] = [
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36, 0x6c, 0xd8, 0xab, 0x4d, 0x9a, 0x2f,
];

/// An expanded key, `aes.NewCipher`.
pub struct Aes {
    enc: [u32; 60],
    dec: [u32; 60],
    rounds: usize,
}

fn subw(w: u32) -> u32 {
    (u32::from(SBOX0[(w >> 24) as usize]) << 24)
        | (u32::from(SBOX0[((w >> 16) & 0xff) as usize]) << 16)
        | (u32::from(SBOX0[((w >> 8) & 0xff) as usize]) << 8)
        | u32::from(SBOX0[(w & 0xff) as usize])
}

fn be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Aes {
    /// `aes.NewCipher`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] ("invalid key size") unless the key is 16, 24 or 32 bytes.
    pub fn new(key: &[u8]) -> Result<Aes, Error> {
        if !matches!(key.len(), 16 | 24 | 32) {
            return Err(Error::Corrupt(format!(
                "crypto/aes: invalid key size {}",
                key.len()
            )));
        }
        let nk = key.len() / 4;
        let rounds = nk + 6;
        let n = 4 * (rounds + 1);
        let mut enc = [0u32; 60];
        let mut dec = [0u32; 60];
        for i in 0..nk {
            enc[i] = be(&key[4 * i..]);
        }
        for i in nk..n {
            let mut t = enc[i - 1];
            if i % nk == 0 {
                t = subw(t.rotate_left(8)) ^ (u32::from(POWX[i / nk - 1]) << 24);
            } else if nk > 6 && i % nk == 4 {
                t = subw(t);
            }
            enc[i] = enc[i - nk] ^ t;
        }
        // The decryption schedule is the encryption one reversed by round, with InvMixColumns
        // applied to all but the first and last round key.
        for i in (0..n).step_by(4) {
            let ei = n - i - 4;
            for j in 0..4 {
                let mut x = enc[ei + j];
                if i > 0 && i + 4 < n {
                    x = TD0[usize::from(SBOX0[(x >> 24) as usize])]
                        ^ TD1[usize::from(SBOX0[((x >> 16) & 0xff) as usize])]
                        ^ TD2[usize::from(SBOX0[((x >> 8) & 0xff) as usize])]
                        ^ TD3[usize::from(SBOX0[(x & 0xff) as usize])];
                }
                dec[i + j] = x;
            }
        }
        Ok(Aes { enc, dec, rounds })
    }

    /// `Encrypt`: one block, the first 16 bytes of `b`, in place. `b` must have at least 16
    /// bytes.
    pub fn encrypt_block(&self, b: &mut [u8]) {
        let xk = &self.enc;
        let mut s0 = be(&b[0..4]) ^ xk[0];
        let mut s1 = be(&b[4..8]) ^ xk[1];
        let mut s2 = be(&b[8..12]) ^ xk[2];
        let mut s3 = be(&b[12..16]) ^ xk[3];
        let mut k = 4;
        for _ in 0..self.rounds - 1 {
            let t0 = xk[k]
                ^ TE0[(s0 >> 24) as usize]
                ^ TE1[((s1 >> 16) & 0xff) as usize]
                ^ TE2[((s2 >> 8) & 0xff) as usize]
                ^ TE3[(s3 & 0xff) as usize];
            let t1 = xk[k + 1]
                ^ TE0[(s1 >> 24) as usize]
                ^ TE1[((s2 >> 16) & 0xff) as usize]
                ^ TE2[((s3 >> 8) & 0xff) as usize]
                ^ TE3[(s0 & 0xff) as usize];
            let t2 = xk[k + 2]
                ^ TE0[(s2 >> 24) as usize]
                ^ TE1[((s3 >> 16) & 0xff) as usize]
                ^ TE2[((s0 >> 8) & 0xff) as usize]
                ^ TE3[(s1 & 0xff) as usize];
            let t3 = xk[k + 3]
                ^ TE0[(s3 >> 24) as usize]
                ^ TE1[((s0 >> 16) & 0xff) as usize]
                ^ TE2[((s1 >> 8) & 0xff) as usize]
                ^ TE3[(s2 & 0xff) as usize];
            k += 4;
            (s0, s1, s2, s3) = (t0, t1, t2, t3);
        }
        let sb = |w: u32, shift: u32| u32::from(SBOX0[((w >> shift) & 0xff) as usize]) << shift;
        let r0 = sb(s0, 24) | sb(s1, 16) | sb(s2, 8) | sb(s3, 0);
        let r1 = sb(s1, 24) | sb(s2, 16) | sb(s3, 8) | sb(s0, 0);
        let r2 = sb(s2, 24) | sb(s3, 16) | sb(s0, 8) | sb(s1, 0);
        let r3 = sb(s3, 24) | sb(s0, 16) | sb(s1, 8) | sb(s2, 0);
        b[0..4].copy_from_slice(&(r0 ^ xk[k]).to_be_bytes());
        b[4..8].copy_from_slice(&(r1 ^ xk[k + 1]).to_be_bytes());
        b[8..12].copy_from_slice(&(r2 ^ xk[k + 2]).to_be_bytes());
        b[12..16].copy_from_slice(&(r3 ^ xk[k + 3]).to_be_bytes());
    }

    /// `Decrypt`: one block, the first 16 bytes of `b`, in place. `b` must have at least 16
    /// bytes.
    pub fn decrypt_block(&self, b: &mut [u8]) {
        let xk = &self.dec;
        let mut s0 = be(&b[0..4]) ^ xk[0];
        let mut s1 = be(&b[4..8]) ^ xk[1];
        let mut s2 = be(&b[8..12]) ^ xk[2];
        let mut s3 = be(&b[12..16]) ^ xk[3];
        let mut k = 4;
        for _ in 0..self.rounds - 1 {
            let t0 = xk[k]
                ^ TD0[(s0 >> 24) as usize]
                ^ TD1[((s3 >> 16) & 0xff) as usize]
                ^ TD2[((s2 >> 8) & 0xff) as usize]
                ^ TD3[(s1 & 0xff) as usize];
            let t1 = xk[k + 1]
                ^ TD0[(s1 >> 24) as usize]
                ^ TD1[((s0 >> 16) & 0xff) as usize]
                ^ TD2[((s3 >> 8) & 0xff) as usize]
                ^ TD3[(s2 & 0xff) as usize];
            let t2 = xk[k + 2]
                ^ TD0[(s2 >> 24) as usize]
                ^ TD1[((s1 >> 16) & 0xff) as usize]
                ^ TD2[((s0 >> 8) & 0xff) as usize]
                ^ TD3[(s3 & 0xff) as usize];
            let t3 = xk[k + 3]
                ^ TD0[(s3 >> 24) as usize]
                ^ TD1[((s2 >> 16) & 0xff) as usize]
                ^ TD2[((s1 >> 8) & 0xff) as usize]
                ^ TD3[(s0 & 0xff) as usize];
            k += 4;
            (s0, s1, s2, s3) = (t0, t1, t2, t3);
        }
        let sb = |w: u32, shift: u32| u32::from(SBOX1[((w >> shift) & 0xff) as usize]) << shift;
        let r0 = sb(s0, 24) | sb(s3, 16) | sb(s2, 8) | sb(s1, 0);
        let r1 = sb(s1, 24) | sb(s0, 16) | sb(s3, 8) | sb(s2, 0);
        let r2 = sb(s2, 24) | sb(s1, 16) | sb(s0, 8) | sb(s3, 0);
        let r3 = sb(s3, 24) | sb(s2, 16) | sb(s1, 8) | sb(s0, 0);
        b[0..4].copy_from_slice(&(r0 ^ xk[k]).to_be_bytes());
        b[4..8].copy_from_slice(&(r1 ^ xk[k + 1]).to_be_bytes());
        b[8..12].copy_from_slice(&(r2 ^ xk[k + 2]).to_be_bytes());
        b[12..16].copy_from_slice(&(r3 ^ xk[k + 3]).to_be_bytes());
    }

    /// `cipher.NewCBCEncrypter(..).CryptBlocks` in place. `data.len()` must be a multiple of
    /// the block size; a trailing partial block is left as it is.
    pub fn cbc_encrypt(&self, iv: &[u8; BLOCK_SIZE], data: &mut [u8]) {
        let mut prev = *iv;
        for blk in data.chunks_exact_mut(BLOCK_SIZE) {
            for (x, p) in blk.iter_mut().zip(prev) {
                *x ^= p;
            }
            self.encrypt_block(blk);
            prev.copy_from_slice(blk);
        }
    }

    /// `cipher.NewCBCDecrypter(..).CryptBlocks` in place. Same length rule as
    /// [`Aes::cbc_encrypt`].
    pub fn cbc_decrypt(&self, iv: &[u8; BLOCK_SIZE], data: &mut [u8]) {
        let mut prev = *iv;
        for blk in data.chunks_exact_mut(BLOCK_SIZE) {
            let mut ct = [0u8; BLOCK_SIZE];
            ct.copy_from_slice(blk);
            self.decrypt_block(blk);
            for (x, p) in blk.iter_mut().zip(prev) {
                *x ^= p;
            }
            prev = ct;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        crate::object::string::hex_decode(s.as_bytes()).unwrap()
    }

    // FIPS 197 appendix C: one block under each key size.
    #[test]
    fn fips197_block_vectors() {
        let pt = unhex("00112233445566778899aabbccddeeff");
        let cases = [
            (
                "000102030405060708090a0b0c0d0e0f",
                "69c4e0d86a7b0430d8cdb78070b4c55a",
            ),
            (
                "000102030405060708090a0b0c0d0e0f1011121314151617",
                "dda97ca4864cdfe06eaf70a0ec0d7191",
            ),
            (
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
                "8ea2b7ca516745bfeafc49904b496089",
            ),
        ];
        for (key, ct) in cases {
            let c = Aes::new(&unhex(key)).unwrap();
            let mut b = pt.clone();
            c.encrypt_block(&mut b);
            assert_eq!(b, unhex(ct), "key {key}");
            c.decrypt_block(&mut b);
            assert_eq!(b, pt, "key {key}");
        }
    }

    // NIST SP 800-38A F.2.1 and F.2.5: CBC-AES128 and CBC-AES256, first two blocks.
    #[test]
    fn sp800_38a_cbc_vectors() {
        let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
            .try_into()
            .unwrap();
        let pt = unhex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let cases = [
            (
                "2b7e151628aed2a6abf7158809cf4f3c",
                "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2",
            ),
            (
                "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
                "f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d",
            ),
        ];
        for (key, ct) in cases {
            let c = Aes::new(&unhex(key)).unwrap();
            let mut b = pt.clone();
            c.cbc_encrypt(&iv, &mut b);
            assert_eq!(b, unhex(ct));
            c.cbc_decrypt(&iv, &mut b);
            assert_eq!(b, pt);
        }
    }

    #[test]
    fn key_size_is_checked() {
        for n in [0, 5, 15, 17, 31, 33] {
            assert!(Aes::new(&vec![0; n]).is_err(), "{n}");
        }
    }
}
