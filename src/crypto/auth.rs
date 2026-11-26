//! Keys, password checks and permissions (Go: crypto.go `encKey` :220, `validateUserPassword`
//! :271, `key` :313, `u` :396, the AES-256 checks :456-690, `validateOwnerPassword` :693,
//! `permissionBytes` :791, `normalizePermission` :808, `validatePermissions` :822, and
//! crypto_password.go).
//!
//! Only empty passwords are tried (docs/scope.md section 4): the owner password, then the user
//! password. The functions still take the passwords, because Go's `validateOwnerPassword` derives
//! a user password from `/O` for revisions 2 to 4 and tries that. A non-empty password for an
//! AES-256 file would go through SASLprep (crypto_password.go), which is not ported:
//! [`prepare_password`] refuses it as unsupported. Go's `calcOAndU`, `writePermissions` and the
//! other code that writes an encryption dictionary are out of scope: no new dictionary is built.

use super::aes::Aes;
use super::md5::{Md5, md5};
use super::rc4::Rc4;
use super::sha2::{sha256, sha384, sha512};
use super::{ct, malformed, unsupported};
use crate::error::Error;
use crate::model::Enc;

/// `pad`: the password padding string of the standard security handler.
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// `preparePassword`. SASLprep of the empty string is the empty string; anything else is not
/// ported.
///
/// # Errors
/// [`Error::UnsupportedEncryption`] for a non-empty password.
pub fn prepare_password(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.is_empty() {
        Ok(Vec::new())
    } else {
        Err(unsupported(
            "non-empty password for an AES-256 file (SASLprep is not ported)",
        ))
    }
}

/// The password cut or padded to 32 bytes (steps 2a and 3a).
fn padded(pw: &[u8]) -> [u8; 32] {
    let mut out = PAD;
    let n = pw.len().min(32);
    out[..n].copy_from_slice(&pw[..n]);
    out
}

/// `e.L/8` as the end of a slice of a 16-byte digest. Go panics for any other value.
fn key_len(l: i64) -> Result<usize, Error> {
    usize::try_from(l / 8)
        .ok()
        .filter(|n| *n <= 16)
        .ok_or(Error::Panic("encryption key length out of range"))
}

/// The key with every byte XORed with `i` (Algorithms 3 and 5).
fn xor_key(key: &[u8], i: u8) -> Vec<u8> {
    key.iter().map(|b| b ^ i).collect()
}

/// `encKey`: Algorithm 2, the file key from the user password.
fn enc_key_alg2(userpw: &[u8], e: &Enc) -> Result<Vec<u8>, Error> {
    let mut h = Md5::new();
    h.update(&padded(userpw));
    h.update(&e.o);
    h.update(&(e.p as u32).to_le_bytes());
    h.update(&e.id);
    if e.r == 4 && !e.emd {
        h.update(&[0xff; 4]);
    }
    let mut key = h.finish().to_vec();
    if e.r >= 3 {
        let n = key_len(e.l)?;
        for _ in 0..50 {
            key = md5(&[&key[..n]]).to_vec();
        }
        key.truncate(n);
    } else {
        key.truncate(5);
    }
    Ok(key)
}

/// `key`: Algorithm 3 steps a to d, the RC4 key for `/O`.
fn key_alg3(ownerpw: &[u8], userpw: &[u8], r: i64, l: i64) -> Result<Vec<u8>, Error> {
    let pw = padded(if ownerpw.is_empty() { userpw } else { ownerpw });
    let mut key = md5(&[&pw[..]]).to_vec();
    if r >= 3 {
        for _ in 0..50 {
            key = md5(&[&key[..]]).to_vec();
        }
        key.truncate(key_len(l)?);
    } else {
        key.truncate(5);
    }
    Ok(key)
}

/// `u`: Algorithms 4 and 5, the user password digest and the file key.
fn u_alg45(userpw: &[u8], e: &Enc) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let key = enc_key_alg2(userpw, e)?;
    let mut c = Rc4::new(&key)?;
    let mut u = match e.r {
        2 => {
            let mut u = PAD.to_vec();
            c.apply(&mut u);
            u
        }
        3 | 4 => {
            let mut u = md5(&[&PAD[..], &e.id[..]]).to_vec();
            c.apply(&mut u);
            for i in 1..=19u8 {
                Rc4::new(&xor_key(&key, i))?.apply(&mut u);
            }
            u
        }
        _ => Vec::new(),
    };
    if u.len() < 32 {
        u.resize(32, 0);
    }
    Ok((u, key))
}

/// `passwordHashPrefixEqual`: whether `b` starts with `prefix`, compared in constant time.
fn prefix_equal(b: &[u8], prefix: &[u8]) -> bool {
    b.len() >= prefix.len() && ct::eq(&b[..prefix.len()], prefix)
}

fn entry_part(b: &[u8], from: usize, to: usize) -> Result<&[u8], Error> {
    b.get(from..to)
        .ok_or(Error::Panic("encryption entry shorter than 48 bytes"))
}

/// `validationSalt`.
fn validation_salt(b: &[u8]) -> Result<&[u8], Error> {
    entry_part(b, 32, 40)
}

/// `keySalt`.
fn key_salt(b: &[u8]) -> Result<&[u8], Error> {
    entry_part(b, 40, 48)
}

/// CBC-decrypts the 32-byte `/UE` or `/OE` with a zero IV (`decryptUE`, `decryptOE`).
fn decrypt_file_key(key: &[u8], data: &[u8]) -> Result<Vec<u8>, Error> {
    let aes = Aes::new(key)?;
    if data.len() != 32 {
        return Err(Error::Panic("UE or OE is not 32 bytes"));
    }
    let mut out = data.to_vec();
    aes.cbc_decrypt(&[0; 16], &mut out);
    Ok(out)
}

/// `hashRev6`: Algorithm 2.B, the 32-byte hash of revision 6.
fn hash_rev6(input: &[u8], pw: &[u8], u: &[u8]) -> Result<[u8; 32], Error> {
    let mut k = sha256(input).to_vec();
    let mut e: Vec<u8> = Vec::new();
    let mut j = 0usize;
    // At least 64 rounds, then until the last byte of E is at most round - 32 (mod 256). The
    // round counter wraps, so the loop ends by round 287.
    while j < 64 || e.last().copied().unwrap_or(0) > (j - 32) as u8 {
        let mut bb = Vec::with_capacity(pw.len() + k.len() + u.len());
        bb.extend_from_slice(pw);
        bb.extend_from_slice(&k);
        bb.extend_from_slice(u);
        e = bb.repeat(64);
        let aes = Aes::new(&k[..16])?;
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&k[16..32]);
        aes.cbc_encrypt(&iv, &mut e);
        // The first 16 bytes of E as a big-endian number, mod 3. 256 is 1 mod 3, so that is
        // the byte sum.
        let r = e[..16].iter().map(|&b| usize::from(b)).sum::<usize>() % 3;
        k = match r {
            0 => sha256(&e).to_vec(),
            1 => sha384(&e).to_vec(),
            _ => sha512(&e).to_vec(),
        };
        j += 1;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[..32]);
    Ok(out)
}

/// `validateUserPasswordAES256`: revision 5.
fn validate_user_password_r5(e: &Enc, userpw: &[u8], enc_key: &mut Vec<u8>) -> Result<bool, Error> {
    let upw = prepare_password(userpw)?;
    // Algorithm 3.2a 4.
    let s = sha256(&[upw.as_slice(), validation_salt(&e.u)?].concat());
    if !prefix_equal(&e.u, &s) {
        return Ok(false);
    }
    let key = sha256(&[upw.as_slice(), key_salt(&e.u)?].concat());
    *enc_key = decrypt_file_key(&key, &e.ue)?;
    Ok(true)
}

/// `validateUserPasswordAES256Rev6`.
fn validate_user_password_r6(e: &Enc, userpw: &[u8], enc_key: &mut Vec<u8>) -> Result<bool, Error> {
    if e.ue.len() != 32 {
        return Err(Error::Corrupt("UE: invalid length".into()));
    }
    let upw = prepare_password(userpw)?;
    let s = hash_rev6(
        &[upw.as_slice(), validation_salt(&e.u)?].concat(),
        &upw,
        &[],
    )?;
    if !prefix_equal(&e.u, &s) {
        return Ok(false);
    }
    let key = hash_rev6(&[upw.as_slice(), key_salt(&e.u)?].concat(), &upw, &[])?;
    *enc_key = decrypt_file_key(&key, &e.ue)?;
    Ok(true)
}

/// `validateUserPassword`: whether `userpw` opens the document. Sets `enc_key` (Go's
/// `ctx.EncKey`): for revisions 2 to 4 to the key derived from `userpw` even when the password
/// is wrong, for revisions 5 and 6 only when it is right.
///
/// # Errors
/// [`Error::UnsupportedEncryption`] for a non-empty password on an AES-256 file; a key length
/// that is out of range.
pub fn validate_user_password(
    e: &Enc,
    userpw: &[u8],
    enc_key: &mut Vec<u8>,
) -> Result<bool, Error> {
    match e.r {
        5 => return validate_user_password_r5(e, userpw, enc_key),
        6 => return validate_user_password_r6(e, userpw, enc_key),
        _ => {}
    }
    // Alg. 4 and 5: 4a and 5a create the key with Alg. 2.
    let (u, key) = u_alg45(userpw, e)?;
    *enc_key = key;
    Ok(match e.r {
        2 => ct::eq(&e.u, &u),
        3 | 4 => prefix_equal(&e.u, &u[..16]),
        _ => false,
    })
}

/// `validateOwnerPassword`: whether `ownerpw` is the owner password. For revisions 2 to 4 it
/// recovers the user password from `/O` and checks that with [`validate_user_password`], which
/// sets `enc_key`. Revisions 5 and 6 with an empty owner password are not owner-authenticated.
///
/// # Errors
/// As [`validate_user_password`]; [`Error::UnsupportedEncryption`] for a non-empty owner
/// password on an AES-256 file.
pub fn validate_owner_password(
    e: &Enc,
    ownerpw: &[u8],
    userpw: &[u8],
    enc_key: &mut Vec<u8>,
) -> Result<bool, Error> {
    if e.r == 5 || e.r == 6 {
        if ownerpw.is_empty() {
            return Ok(false);
        }
        return prepare_password(ownerpw).map(|_| false);
    }
    // 7a: Alg. 3 a-d.
    let key = key_alg3(ownerpw, userpw, e.r, e.l)?;
    // 7b.
    let mut upw = e.o.clone();
    match e.r {
        2 => Rc4::new(&key)?.apply(&mut upw),
        3 | 4 => {
            for i in (0..=19u8).rev() {
                Rc4::new(&xor_key(&key, i))?.apply(&mut upw);
            }
        }
        _ => {}
    }
    validate_user_password(e, &upw, enc_key)
}

/// `validatePermission`: `p` must fit a signed 32-bit integer.
fn validate_permission(p: i64) -> Result<(), Error> {
    if i32::try_from(p).is_err() {
        return Err(malformed(format!(
            "permission value P {p} out of signed 32-bit range"
        )));
    }
    Ok(())
}

/// `permissionBytes`: `p` as four little-endian bytes.
///
/// # Errors
/// [`Error::MalformedEncryption`] when `p` is outside the signed 32-bit range.
pub fn permission_bytes(p: i64) -> Result<[u8; 4], Error> {
    validate_permission(p)?;
    Ok((p as i32).to_le_bytes())
}

/// `normalizePermission` in relaxed mode: a `P` that was written as an unsigned 32-bit number is
/// accepted and wrapped to the signed range. The flag says that it was (a specification
/// violation Go digests).
///
/// # Errors
/// [`Error::MalformedEncryption`] for any other value outside the signed 32-bit range.
pub fn normalize_permission(p: i64) -> Result<(i64, bool), Error> {
    match validate_permission(p) {
        Ok(()) => Ok((p, false)),
        Err(err) => {
            if !(0..=4_294_967_295).contains(&p) {
                return Err(err);
            }
            Ok((p - (1 << 32), true))
        }
    }
}

/// `validatePermissions`: Algorithm 3.2a step 5, the check of `/Perms` for revisions 5 and 6.
/// Always true for the other revisions.
///
/// # Errors
/// [`Error::Corrupt`] if `/Perms` is shorter than a block or the key has no AES size.
pub fn validate_permissions(e: &Enc, enc_key: &[u8]) -> Result<bool, Error> {
    if e.r != 5 && e.r != 6 {
        return Ok(true);
    }
    let aes = Aes::new(enc_key)?;
    let Some(block) = e.perms.get(..16) else {
        return Err(Error::Panic("Perms is shorter than one block"));
    };
    let mut p = block.to_vec();
    aes.decrypt_block(&mut p);
    if &p[9..12] != b"adb" {
        return Ok(false);
    }
    if p[8] != b'T' && p[8] != b'F' {
        return Ok(false);
    }
    if (p[8] == b'T') != e.emd {
        return Ok(false);
    }
    let expected = permission_bytes(e.p)?;
    Ok(p[..4] == expected)
}

/// The tail of `setupEncryptionKey` (read.go:4425) for empty passwords and a command that does
/// not need both passwords: the owner password is tried first, then the user password, and the
/// permissions of an AES-256 file are checked. On success `enc_key` is the file key. `e.id` must
/// be set.
///
/// # Errors
/// [`Error::WrongPassword`] if neither empty password opens the file; an
/// [`Error::Corrupt`] "invalid permissions" if `/Perms` does not match; the errors of the checks.
pub fn authenticate(e: &Enc, enc_key: &mut Vec<u8>) -> Result<(), Error> {
    // The owner password aka permissions or master password.
    if validate_owner_password(e, &[], &[], enc_key)
        .map_err(|err| err.context("validate owner password"))?
    {
        let ok =
            validate_permissions(e, enc_key).map_err(|err| err.context("validate permissions"))?;
        if !ok {
            return Err(Error::Corrupt(
                "owner password permissions: invalid permissions".into(),
            ));
        }
        return Ok(());
    }
    // The user password aka document open password.
    if !validate_user_password(e, &[], enc_key)
        .map_err(|err| err.context("validate user password"))?
    {
        return Err(Error::WrongPassword);
    }
    let ok = validate_permissions(e, enc_key).map_err(|err| err.context("validate permissions"))?;
    if !ok {
        return Err(Error::Corrupt(
            "user password permissions: invalid permissions".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        crate::object::string::hex_decode(s.as_bytes()).unwrap()
    }

    // The files below were made with `qpdf --encrypt "" owner <bits> ...` (qpdf 12), so the
    // dictionaries and the keys come from an independent implementation. The file key is what
    // `qpdf --show-encryption-key` prints.
    fn r2() -> Enc {
        Enc {
            o: unhex("c92422687facee686e373f10b5c7d04738053152f7e2ee30e11c69ec442576ab"),
            u: unhex("4499a4996ed25de042be3e1e8b8a44ce931bc9a5b6248bdf6af1020eaf0393e3"),
            l: 40,
            p: -4,
            r: 2,
            v: 1,
            emd: true,
            id: unhex("af5a0bc2e84136018337bb9602334dd9"),
            ..Enc::default()
        }
    }

    fn r3() -> Enc {
        Enc {
            o: unhex("566fa873ee33c797cd3b904fdadf814afa34df9a38f6ed41b984e2c6da2aa6f5"),
            u: unhex("e8e34a5f95fe108719f346ff745ac4ea0021446990b9e4114071a4d9104984c1"),
            l: 128,
            p: -4,
            r: 3,
            v: 2,
            emd: true,
            id: unhex("d52eff26934a089990665dd2753ea7ea"),
            ..Enc::default()
        }
    }

    fn r4() -> Enc {
        Enc {
            o: unhex("566fa873ee33c797cd3b904fdadf814afa34df9a38f6ed41b984e2c6da2aa6f5"),
            u: unhex("94396b23b852e6dcb62993cae45a2ea80021446990b9e4114071a4d9104984c1"),
            l: 128,
            p: -4,
            r: 4,
            v: 4,
            emd: true,
            id: unhex("b1ca7718e6ebd53fae92f08198ad65c0"),
            ..Enc::default()
        }
    }

    fn r5() -> Enc {
        Enc {
            o: unhex(
                "4aa979673374589038313b2ec8e6dd5c500d8186b2c42d256d2b24e21cbdfa77d70df75def42d4d6a07175b234e3fd19",
            ),
            oe: unhex("d1fcb0e399324ccaf5b6d5eaf3fd7782a64ecacc042a5666bb8b23c2f7f8af37"),
            u: unhex(
                "a30ed36c77651b6de1486dd9ad1def0c99860f3c6c4af1321edce1a3b93f770ad9fc4c444e2dd95351685e66d4603185",
            ),
            ue: unhex("8f17e02e67f53f00a6350512a43ed1df77c927ae35ceda11b7c3e68b8604ee03"),
            perms: unhex("dd1ecd64ec4db1a03a92e5b974e94b43"),
            l: 256,
            p: -4,
            r: 5,
            v: 5,
            emd: true,
            id: Vec::new(),
        }
    }

    fn r6() -> Enc {
        Enc {
            o: unhex(
                "a9c6736455bda0afe610db3e92b4d6929b3e98319341a49dbcb31e72f9f18590f5694d3698ba7b6f34102bee664ea455",
            ),
            oe: unhex("ab384a90a5c8c3bb6ddec744a0df01db193b62d50a83eb0adae0f813f0268145"),
            u: unhex(
                "8798e2b884849a0f062939582e7d3d11c914e4e36b74fd8909726bf1e873d0f15aae1d35996062a2bb4bfd499696fb34",
            ),
            ue: unhex("822cbbf578473a073d826837cea91543f2e0a18ad33fd8c2170fe1e9b324e2a7"),
            perms: unhex("ce365a864923aec7b643025e3a9b3a50"),
            l: 256,
            p: -4,
            r: 6,
            v: 5,
            emd: true,
            id: Vec::new(),
        }
    }

    const KEY_R2: &str = "59802ed6a8";
    const KEY_R3: &str = "d5594260d4b556b2ae36650542d6636d";
    const KEY_R4: &str = "e80e9ea8bdb0fa7efc3e880e9e83233f";
    const KEY_R5: &str = "2850b7b14f8bc0bdd0826585a3e1549cc1bb324d00c89c262631529cb3ac7bd3";
    const KEY_R6: &str = "dcf9cadc23f2faa1b75a105c0beb5e94baaa6376ab6561e0405b3d62d8d09ccf";

    #[test]
    fn empty_user_password_recovers_the_file_key() {
        let cases = [
            (r2(), KEY_R2),
            (r3(), KEY_R3),
            (r4(), KEY_R4),
            (r5(), KEY_R5),
            (r6(), KEY_R6),
        ];
        for (e, want) in cases {
            let mut key = Vec::new();
            assert!(
                validate_user_password(&e, &[], &mut key).unwrap(),
                "R{}",
                e.r
            );
            assert_eq!(key, unhex(want), "R{}", e.r);
            // The owner password was "owner", so the empty one is not it.
            let mut key = Vec::new();
            assert!(
                !validate_owner_password(&e, &[], &[], &mut key).unwrap(),
                "R{}",
                e.r
            );
            // The whole flow lands on the user password.
            let mut key = Vec::new();
            authenticate(&e, &mut key).unwrap();
            assert_eq!(key, unhex(want), "R{}", e.r);
        }
    }

    #[test]
    fn a_wrong_digest_is_a_wrong_password() {
        for mut e in [r2(), r3(), r4(), r5(), r6()] {
            e.u[0] ^= 1;
            let mut key = Vec::new();
            assert_eq!(
                authenticate(&e, &mut key).unwrap_err().root(),
                &Error::WrongPassword,
                "R{}",
                e.r
            );
        }
    }

    #[test]
    fn r3_and_r4_compare_only_16_bytes_of_u() {
        let mut e = r3();
        e.u[20] ^= 0xff;
        let mut key = Vec::new();
        assert!(validate_user_password(&e, &[], &mut key).unwrap());
        // A U shorter than the prefix never matches.
        e.u.truncate(15);
        assert!(!validate_user_password(&e, &[], &mut key).unwrap());
        // R2 needs all of it.
        let mut e = r2();
        e.u.push(0);
        assert!(!validate_user_password(&e, &[], &mut key).unwrap());
    }

    #[test]
    fn encrypt_metadata_changes_the_r4_key() {
        let mut e = r4();
        e.emd = false;
        let mut key = Vec::new();
        assert!(!validate_user_password(&e, &[], &mut key).unwrap());
        // The key is set even though the password failed, as in Go.
        assert_ne!(key, unhex(KEY_R4));
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn permissions_of_r5_and_r6() {
        for (e, key) in [(r5(), KEY_R5), (r6(), KEY_R6)] {
            assert!(validate_permissions(&e, &unhex(key)).unwrap(), "R{}", e.r);
            let mut bad = e.clone();
            bad.emd = false;
            assert!(!validate_permissions(&bad, &unhex(key)).unwrap());
            let mut bad = e.clone();
            bad.p = -8;
            assert!(!validate_permissions(&bad, &unhex(key)).unwrap());
            // A wrong key decrypts to noise.
            assert!(!validate_permissions(&e, &[7; 32]).unwrap());
        }
        // Other revisions have nothing to check.
        assert!(validate_permissions(&r4(), &[]).unwrap());
    }

    #[test]
    fn bad_perms_fail_authentication() {
        let mut e = r6();
        e.p = -8;
        let mut key = Vec::new();
        let err = authenticate(&e, &mut key).unwrap_err();
        assert!(err.to_string().contains("invalid permissions"));
    }

    #[test]
    fn non_empty_aes256_passwords_are_unsupported() {
        for e in [r5(), r6()] {
            let mut key = Vec::new();
            for r in [
                validate_user_password(&e, b"x", &mut key),
                validate_owner_password(&e, b"x", &[], &mut key),
            ] {
                assert_eq!(r.unwrap_err().class(), "encryption/unsupported");
            }
        }
        assert_eq!(prepare_password(b"").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn short_aes256_entries_are_an_error_not_a_panic() {
        let mut e = r6();
        e.u.truncate(40);
        let mut key = Vec::new();
        assert_eq!(
            validate_user_password(&e, &[], &mut key)
                .unwrap_err()
                .class(),
            "panic"
        );
        let mut e = r6();
        e.ue.truncate(16);
        assert!(validate_user_password(&e, &[], &mut key).is_err());
    }

    // Port of TestPermissionBytes.
    #[test]
    fn permission_bytes_cases() {
        assert_eq!(
            permission_bytes(-(1 << 31)).unwrap(),
            [0x00, 0x00, 0x00, 0x80]
        );
        assert_eq!(permission_bytes(-4).unwrap(), [0xFC, 0xFF, 0xFF, 0xFF]);
        assert_eq!(
            permission_bytes((1 << 31) - 1).unwrap(),
            [0xFF, 0xFF, 0xFF, 0x7F]
        );
        for p in [-(1i64 << 31) - 1, 1 << 31] {
            assert_eq!(
                permission_bytes(p).unwrap_err().class(),
                "encryption/malformed"
            );
        }
    }

    // Port of TestNormalizeUnsignedPermission.
    #[test]
    fn normalize_unsigned_permission() {
        assert_eq!(normalize_permission(1 << 31).unwrap(), (-(1 << 31), true));
        assert_eq!(normalize_permission((1 << 32) - 1).unwrap(), (-1, true));
        assert_eq!(normalize_permission(-4).unwrap(), (-4, false));
        assert!(normalize_permission(1 << 32).is_err());
        assert!(normalize_permission(-(1 << 31) - 1).is_err());
    }

    #[test]
    fn key_length_out_of_range_is_an_error() {
        let mut e = r3();
        e.l = 256;
        let mut key = Vec::new();
        assert_eq!(
            validate_user_password(&e, &[], &mut key)
                .unwrap_err()
                .class(),
            "panic"
        );
        e.l = -8;
        assert!(validate_owner_password(&e, &[], &[], &mut key).is_err());
    }

    // Port of TestPasswordHashPrefixEqual.
    #[test]
    fn password_hash_prefix_equal() {
        assert!(prefix_equal(b"digest-and-salt", b"digest"));
        assert!(!prefix_equal(b"digesu-and-salt", b"digest"));
        assert!(!prefix_equal(b"short", b"digest"));
    }
}
