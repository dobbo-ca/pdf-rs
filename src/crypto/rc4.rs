// Copyright 2009 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! RC4, ported from Go's crypto/rc4.

use crate::error::Error;

/// `rc4.Cipher`.
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// `rc4.NewCipher`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] ("invalid key size") unless the key is 1 to 256 bytes.
    pub fn new(key: &[u8]) -> Result<Rc4, Error> {
        let k = key.len();
        if !(1..=256).contains(&k) {
            return Err(Error::Corrupt(format!("crypto/rc4: invalid key size {k}")));
        }
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j: u8 = 0;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % k]);
            s.swap(i, usize::from(j));
        }
        Ok(Rc4 { s, i: 0, j: 0 })
    }

    /// `XORKeyStream` in place.
    pub fn apply(&mut self, buf: &mut [u8]) {
        let (mut i, mut j) = (self.i, self.j);
        for v in buf.iter_mut() {
            i = i.wrapping_add(1);
            let x = self.s[usize::from(i)];
            j = j.wrapping_add(x);
            let y = self.s[usize::from(j)];
            self.s[usize::from(i)] = y;
            self.s[usize::from(j)] = x;
            *v ^= self.s[usize::from(x.wrapping_add(y))];
        }
        self.i = i;
        self.j = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vectors from Go's crypto/rc4/rc4_test.go (golden) and RFC 6229.
    #[test]
    fn known_streams() {
        let mut c = Rc4::new(b"Key").unwrap();
        let mut b = *b"Plaintext";
        c.apply(&mut b);
        assert_eq!(b, [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]);

        let mut c = Rc4::new(b"Wiki").unwrap();
        let mut b = *b"pedia";
        c.apply(&mut b);
        assert_eq!(b, [0x10, 0x21, 0xBF, 0x04, 0x20]);

        let mut c = Rc4::new(&[1, 2, 3, 4, 5]).unwrap();
        let mut b = [0u8; 16];
        c.apply(&mut b);
        assert_eq!(
            b,
            [
                0xb2, 0x39, 0x63, 0x05, 0xf0, 0x3d, 0xc0, 0x27, 0xcc, 0xc3, 0x52, 0x4a, 0x0a, 0x11,
                0x18, 0xa8
            ]
        );
    }

    #[test]
    fn key_size_is_checked_and_stream_continues() {
        assert!(Rc4::new(&[]).is_err());
        assert!(Rc4::new(&[0; 257]).is_err());
        assert!(Rc4::new(&[0; 256]).is_ok());
        let key = [9u8; 7];
        let mut whole = vec![0u8; 40];
        Rc4::new(&key).unwrap().apply(&mut whole);
        let mut parts = vec![0u8; 40];
        let mut c = Rc4::new(&key).unwrap();
        let (a, b) = parts.split_at_mut(13);
        c.apply(a);
        c.apply(b);
        assert_eq!(whole, parts);
    }
}
