//! PDF encryption: the primitives (MD5, RC4, AES, SHA-2), the keys and password checks, the
//! encryption dictionary validation, and the decryption and encryption of strings and streams
//! (docs/design.md section 15, work package 7; Go: pkg/pdfcpu/crypto.go and
//! crypto_password.go).
//!
//! The primitives are in-crate ports of the Go standard library, so there is no dependency. They
//! are plain safe Rust, not constant time (except the password digest comparison), as Go's
//! generic code is not.
//!
//! This module keeps no state. The document's `E`, `EncKey` and `AES4*` are fields of the
//! [`XRefTable`] (`e`, `enc_key`, `aes4_strings`, `aes4_streams`, `aes4_embedded_streams`); the
//! dictionary checks and the crypt filter selection read and set them as Go does, and the byte
//! level functions take what they need as arguments. Streams are charged to the ledger where Go's
//! allocations scale with the input, and so are the copies a string makes on its way through.
//!
//! Not ported, because pdf-rs never builds an encryption dictionary (docs/scope.md section 3.4):
//! `newEncryptDict`, `o`, `calcOAndU`, `writePermissions`, `validateAlgorithm`,
//! `PermissionsList`, the permission mask table and `hasNeededPermissions`.

mod aes;
mod aes_tables;
mod auth;
mod ct;
mod md5;
mod rc4;
mod sha2;
mod validate;

/// `md5.Sum`, public for callers that build a document `/ID` the way Go does.
pub use md5::md5;
/// `sha256.Sum256`, for the optimizer's hash of image streams.
pub(crate) use sha2::sha256;

pub use auth::{
    authenticate, normalize_permission, permission_bytes, prepare_password,
    validate_owner_password, validate_permissions, validate_user_password,
};
pub use validate::{
    classify_encryption_dictionary_error, default_crypt_filter_encrypted, stream_crypt_method,
    strings_encrypted, supported_encryption, validate_encryption_version,
};

use crate::env::WriteEnv;
use crate::error::{Error, Sentinel};
use crate::gocap::read_from_cap;
use crate::ledger::{Bytes, Held, Mem};
use crate::model::XRefTable;
use crate::object::string::{escape, hex_decode, hex_encode, unescape};
use crate::object::write::Sink;
use crate::object::{Dict, FREE_HEAD_GENERATION, Object};
use aes::{Aes, BLOCK_SIZE};
use md5::Md5;
use rc4::Rc4;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

/// Fills the slice with random bytes: [`WriteEnv::random`].
pub type RandomFn = fn(&mut [u8]) -> Result<(), Error>;

/// `ErrMalformedEncryption` with a message.
fn malformed(msg: impl Into<String>) -> Error {
    Error::MalformedEncryption(Box::new(Error::Corrupt(msg.into())))
}

/// `ErrUnsupportedEncryptionFeature` with a message.
fn unsupported(msg: impl Into<String>) -> Error {
    Error::UnsupportedEncryption(msg.into())
}

/// `fmt.Errorf("%w: ...: %w", ErrMalformedEncryption, err)`: `err` with `msg` in front, marked
/// malformed. An error that is not about the structure of the dictionary (a limit, the cancel
/// flag, an allocation) and one that is already an encryption error keep their own class, as
/// `errors.Is` finds both sentinels in Go.
fn malformed_ctx(err: Error, msg: String) -> Error {
    let err = err.context(msg);
    match err.root() {
        Error::MalformedEncryption(_)
        | Error::UnsupportedEncryption(_)
        | Error::Limit(_)
        | Error::DecodeLimit
        | Error::Alloc { .. }
        | Error::OutOfMemory
        | Error::Cancelled
        | Error::Panic(_) => err,
        _ => Error::MalformedEncryption(Box::new(err)),
    }
}

/// `contextutil.Check`.
fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `decryptKey`: the per-object key of Algorithm 1 (and 1.A for the `sAlT` of AES).
///
/// # Errors
/// [`Error::Corrupt`] for an object number outside `0..=u32::MAX` or a generation outside
/// `0..=65535`.
pub fn decrypt_key(obj_nr: i64, generation: i64, key: &[u8], aes: bool) -> Result<Vec<u8>, Error> {
    const MAX_OBJECT_NUMBER: i64 = u32::MAX as i64;
    if !(0..=MAX_OBJECT_NUMBER).contains(&obj_nr) {
        return Err(Error::Corrupt(format!(
            "decrypt key: object number {obj_nr} out of range [0,{MAX_OBJECT_NUMBER}]"
        )));
    }
    if !(0..=FREE_HEAD_GENERATION).contains(&generation) {
        return Err(Error::Corrupt(format!(
            "decrypt key: generation number {generation} out of range [0,{FREE_HEAD_GENERATION}]"
        )));
    }
    let mut h = Md5::new();
    h.update(key);
    h.update(&obj_nr.to_le_bytes()[..3]);
    h.update(&(generation as u16).to_le_bytes());
    if aes {
        h.update(b"sAlT");
    }
    let mut dk = h.finish().to_vec();
    let l = key.len() + 5;
    if l < 16 {
        dk.truncate(l);
    }
    Ok(dk)
}

/// The key for an AES string or stream: the file key itself for revisions 5 and 6, the object
/// key otherwise.
fn aes_key(obj_nr: i64, gen_nr: i64, enc_key: &[u8], r: i64) -> Result<Cow<'_, [u8]>, Error> {
    if r == 5 || r == 6 {
        Ok(Cow::Borrowed(enc_key))
    } else {
        decrypt_key(obj_nr, gen_nr, enc_key, true).map(Cow::Owned)
    }
}

/// A zeroed buffer of `n` bytes, charged to `mem` when there is one.
fn alloc(n: usize, mem: Option<&Mem>) -> Result<(Vec<u8>, Option<Held>), Error> {
    match mem {
        Some(m) => m.alloc_zeroed(n).map(|(v, h)| (v, Some(h))),
        None => {
            let mut v = Vec::new();
            v.try_reserve_exact(n)?;
            v.resize(n, 0);
            Ok((v, None))
        }
    }
}

/// `decryptAESBytes`, with the buffer from `alloc`.
fn decrypt_aes_with(
    b: &[u8],
    key: &[u8],
    mem: Option<&Mem>,
) -> Result<(Vec<u8>, Option<Held>), Error> {
    if b.len() < 2 * BLOCK_SIZE {
        return Err(Error::Sentinel(Sentinel::AesTooShort));
    }
    if !b.len().is_multiple_of(BLOCK_SIZE) {
        return Err(Error::Sentinel(Sentinel::AesUnaligned));
    }
    let aes = Aes::new(key)?;
    let mut iv = [0u8; BLOCK_SIZE];
    iv.copy_from_slice(&b[..BLOCK_SIZE]);
    let (mut data, held) = alloc(b.len() - BLOCK_SIZE, mem)?;
    data.copy_from_slice(&b[BLOCK_SIZE..]);
    aes.cbc_decrypt(&iv, &mut data);
    // Remove the padding. Not all AES ciphertexts are padded.
    if let Some(&last) = data.last()
        && last <= 0x10
    {
        data.truncate(data.len() - usize::from(last));
    }
    Ok((data, held))
}

/// `decryptAESBytes`: CBC with the IV in the first block, and the padding removed.
///
/// # Errors
/// [`Sentinel::AesTooShort`] below two blocks, [`Sentinel::AesUnaligned`] for a length that is
/// not a multiple of 16, [`Error::Corrupt`] for a key that is not 16, 24 or 32 bytes.
pub(crate) fn decrypt_aes_bytes(b: &[u8], key: &[u8]) -> Result<Vec<u8>, Error> {
    decrypt_aes_with(b, key, None).map(|(v, _)| v)
}

/// `encryptAESBytes`, with the buffer from `alloc`. The result is the IV and the padded
/// ciphertext, exactly as long as Go's `make([]byte, 16+len(padded))`.
fn encrypt_aes_with(
    b: &[u8],
    key: &[u8],
    random: RandomFn,
    mem: Option<&Mem>,
) -> Result<(Vec<u8>, Option<Held>), Error> {
    // Pad to the block size: 1 to 16 bytes, each holding their count.
    let pad = BLOCK_SIZE - b.len() % BLOCK_SIZE;
    let total = b
        .len()
        .checked_add(pad)
        .and_then(|n| n.checked_add(BLOCK_SIZE))
        .ok_or(Error::OutOfMemory)?;
    let (mut data, held) = alloc(total, mem)?;
    random(&mut data[..BLOCK_SIZE])?;
    let mut iv = [0u8; BLOCK_SIZE];
    iv.copy_from_slice(&data[..BLOCK_SIZE]);
    let aes = Aes::new(key)?;
    let body = &mut data[BLOCK_SIZE..];
    body[..b.len()].copy_from_slice(b);
    body[b.len()..].fill(pad as u8);
    aes.cbc_encrypt(&iv, body);
    Ok((data, held))
}

/// `encryptAESBytes`: pads `b` to the block size, picks a random IV with `random`, and returns
/// the IV followed by the CBC ciphertext.
///
/// # Errors
/// [`Error::Corrupt`] for a key that is not 16, 24 or 32 bytes; whatever `random` returns.
pub(crate) fn encrypt_aes_bytes(b: &[u8], key: &[u8], random: RandomFn) -> Result<Vec<u8>, Error> {
    encrypt_aes_with(b, key, random, None).map(|(v, _)| v)
}

/// `applyRC4CipherBytes`: RC4 with the object key, for strings.
fn apply_rc4_cipher_bytes(
    b: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
) -> Result<Vec<u8>, Error> {
    let k = decrypt_key(obj_nr, gen_nr, key, need_aes)?;
    let mut c = Rc4::new(&k)?;
    let (mut out, _) = alloc(b.len(), None)?;
    out.copy_from_slice(b);
    c.apply(&mut out);
    Ok(out)
}

/// `encryptBytes`: a string, with RC4 or AES.
///
/// # Errors
/// As [`decrypt_key`] and [`encrypt_aes_bytes`].
pub(crate) fn encrypt_bytes(
    b: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    enc_key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
) -> Result<Vec<u8>, Error> {
    if need_aes {
        let k = aes_key(obj_nr, gen_nr, enc_key, r)?;
        return encrypt_aes_bytes(b, &k, random);
    }
    apply_rc4_cipher_bytes(b, obj_nr, gen_nr, enc_key, need_aes)
}

/// `decryptBytes`: a string, with RC4 or AES.
///
/// # Errors
/// As [`decrypt_key`] and [`decrypt_aes_bytes`].
pub(crate) fn decrypt_bytes(
    b: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    enc_key: &[u8],
    need_aes: bool,
    r: i64,
) -> Result<Vec<u8>, Error> {
    if need_aes {
        let k = aes_key(obj_nr, gen_nr, enc_key, r)?;
        return decrypt_aes_bytes(b, &k);
    }
    apply_rc4_cipher_bytes(b, obj_nr, gen_nr, enc_key, need_aes)
}

/// The key of a stream: the file key for revisions 5 and 6, else the object key (`encryptStream`
/// and `decryptStream` do this for RC4 too, unlike the string functions).
fn stream_key(
    obj_nr: i64,
    gen_nr: i64,
    enc_key: &[u8],
    need_aes: bool,
    r: i64,
) -> Result<Cow<'_, [u8]>, Error> {
    if r == 5 || r == 6 {
        Ok(Cow::Borrowed(enc_key))
    } else {
        decrypt_key(obj_nr, gen_nr, enc_key, need_aes).map(Cow::Owned)
    }
}

/// `applyRC4Bytes` of a stream: a copy of `buf` through RC4, charged to `mem`. Go reads it into a
/// `bytes.Buffer` with `io.Copy`, so the capacity is `ReadFrom`'s (a separate EOF read).
fn rc4_stream(buf: &[u8], key: &[u8], mem: &Mem) -> Result<Bytes, Error> {
    let mut c = Rc4::new(key)?;
    let go_cap = read_from_cap(buf.len())?;
    let (mut out, held) = alloc(buf.len(), Some(mem))?;
    out.copy_from_slice(buf);
    c.apply(&mut out);
    Ok(Bytes::new(out, go_cap, held))
}

/// `decryptStream`: the decrypted copy of a stream's raw bytes, charged to `mem`. For AES, Go
/// decrypts in place and returns the sub-slice after the IV, so its capacity is the input's minus
/// 16; for RC4 it is a new buffer (see `rc4_stream`).
///
/// # Errors
/// As [`decrypt_key`] and `decrypt_aes_bytes`; [`Error::Alloc`] and [`Error::OutOfMemory`].
pub fn decrypt_stream(
    buf: &Bytes,
    obj_nr: i64,
    gen_nr: i64,
    enc_key: &[u8],
    need_aes: bool,
    r: i64,
    mem: &Mem,
) -> Result<Bytes, Error> {
    let k = stream_key(obj_nr, gen_nr, enc_key, need_aes, r)?;
    if need_aes {
        let (data, held) = decrypt_aes_with(buf, &k, Some(mem))?;
        return Ok(Bytes::new(
            data,
            buf.go_cap().saturating_sub(BLOCK_SIZE),
            held,
        ));
    }
    rc4_stream(buf, &k, mem)
}

/// `encryptStream`: the encrypted copy of a stream's bytes, charged to `mem`. For AES it is the
/// IV and the padded ciphertext, in a buffer of exactly that length.
///
/// # Errors
/// As [`decrypt_key`] and `encrypt_aes_bytes`; [`Error::Alloc`] and [`Error::OutOfMemory`].
pub fn encrypt_stream(
    buf: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    enc_key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
    mem: &Mem,
) -> Result<Bytes, Error> {
    let k = stream_key(obj_nr, gen_nr, enc_key, need_aes, r)?;
    if need_aes {
        let (data, held) = encrypt_aes_with(buf, &k, random, Some(mem))?;
        return Ok(Bytes::new(data, 0, held));
    }
    rc4_stream(buf, &k, mem)
}

/// Whether `d` is a signature or time stamp dictionary, whose `Contents` is not encrypted.
fn is_signature(d: &Dict) -> bool {
    // Go reads `d["FT"]` and `d["Type"]` as plain map keys; a nil value counts as absent.
    let ft = match d.0.get(b"FT".as_slice()) {
        Some(o) if !o.is_null() => Some(o),
        _ => d.0.get(b"Type".as_slice()),
    };
    matches!(ft, Some(Object::Name(n)) if n == b"Sig" || n == b"DocTimeStamp")
}

/// The ledger charge of the copies a string of `n` bytes makes on its way through
/// `decrypt_bytes` or `encrypt_bytes`: the unescaped or hex-decoded input, the cipher output (up
/// to two AES blocks longer) and the escaped or hex-encoded result, each at up to twice its
/// length in capacity.
fn string_copies(n: usize) -> u64 {
    (n as u64).saturating_mul(7).saturating_add(192)
}

/// `decryptStringLiteral`: the empty string stays empty.
fn decrypt_string_literal(
    s: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    mem: &Mem,
) -> Result<Vec<u8>, Error> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let _held = mem.charge(string_copies(s.len()))?;
    let bb = decrypt_bytes(&unescape(s), obj_nr, gen_nr, key, need_aes, r)?;
    Ok(escape(&bb))
}

/// `decryptHexLiteral`: the empty string stays empty.
fn decrypt_hex_literal(
    h: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    mem: &Mem,
) -> Result<Vec<u8>, Error> {
    if h.is_empty() {
        return Ok(Vec::new());
    }
    let _held = mem.charge(string_copies(h.len()))?;
    let bb = decrypt_bytes(&hex_decode(h)?, obj_nr, gen_nr, key, need_aes, r)?;
    Ok(hex_encode(&bb))
}

/// `encryptStringLiteral`.
fn encrypt_string_literal(
    s: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
    mem: &Mem,
) -> Result<Vec<u8>, Error> {
    let _held = mem.charge(string_copies(s.len()))?;
    let bb = encrypt_bytes(&unescape(s), obj_nr, gen_nr, key, need_aes, r, random)?;
    Ok(escape(&bb))
}

/// `encryptHexLiteral`.
fn encrypt_hex_literal(
    h: &[u8],
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
    mem: &Mem,
) -> Result<Vec<u8>, Error> {
    let _held = mem.charge(string_copies(h.len()))?;
    let bb = encrypt_bytes(&hex_decode(h)?, obj_nr, gen_nr, key, need_aes, r, random)?;
    Ok(hex_encode(&bb))
}

/// `decryptDict`: the entries in key order, a signature's `Contents` left alone.
fn decrypt_dict(
    d: &mut Dict,
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    mem: &Mem,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let is_sig = is_signature(d);
    for (k, v) in d.iter_mut() {
        check(cancel)?;
        if is_sig && k == b"Contents" {
            continue;
        }
        decrypt_deep_object(v, obj_nr, gen_nr, key, need_aes, r, mem, grown, cancel)
            .map_err(|e| e.context(format!("decrypt dict entry {}", String::from_utf8_lossy(k))))?;
    }
    Ok(())
}

/// `decryptDeepObject`: decrypts the strings of a direct object tree in place. Indirect
/// references are left as they are, and so are the dictionaries of streams (Go's
/// `decryptObjectStrings` is called on a stream's dictionary itself). The cancel flag is checked
/// at every object; cancellation can leave the tree partly decrypted.
///
/// # Errors
/// As [`decrypt_key`] and `decrypt_bytes`, with the entry named; [`Error::Alloc`] for the copies
/// of a string, charged to `mem`, and for a decrypted string that outgrows the one it replaces,
/// whose growth stays charged to `grown`; [`Error::Cancelled`].
pub fn decrypt_deep_object(
    obj: &mut Object,
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    mem: &Mem,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    match obj {
        Object::Dict(d) => decrypt_dict(d, obj_nr, gen_nr, key, need_aes, r, mem, grown, cancel)?,
        Object::Array(a) => {
            for v in a.iter_mut() {
                decrypt_deep_object(v, obj_nr, gen_nr, key, need_aes, r, mem, grown, cancel)?;
            }
        }
        Object::String(s) => {
            let t = decrypt_string_literal(s, obj_nr, gen_nr, key, need_aes, r, mem)?;
            // The parse charged the old length; escaping can double it.
            grown.grow_by(t.capacity().saturating_sub(s.len()) as u64)?;
            *s = t;
            check(cancel)?;
        }
        Object::Hex(h) => {
            let t = decrypt_hex_literal(h, obj_nr, gen_nr, key, need_aes, r, mem)?;
            grown.grow_by(t.capacity().saturating_sub(h.len()) as u64)?;
            *h = t;
            check(cancel)?;
        }
        _ => {}
    }
    check(cancel)
}

/// `decryptObjectStrings`: decrypts the strings of `obj` in place if strings are encrypted at all
/// ([`strings_encrypted`]), with the key and the methods of the table.
///
/// # Errors
/// As [`strings_encrypted`] and [`decrypt_deep_object`].
pub fn decrypt_object_strings(
    x: &mut XRefTable,
    obj: &mut Object,
    obj_nr: i64,
    gen_nr: i64,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !strings_encrypted(x)? {
        return Ok(());
    }
    // `ctx.E.R`: Go dereferences a nil `E` and panics.
    let Some(r) = x.e.as_ref().map(|e| e.r) else {
        return Err(Error::Panic("encryption parameters are not set"));
    };
    decrypt_deep_object(
        obj,
        obj_nr,
        gen_nr,
        &x.enc_key,
        x.aes4_strings,
        r,
        &x.mem,
        &mut x.held,
        cancel,
    )
}

/// `encryptDict`: the entries in key order (Go visits them in map order), a signature's
/// `Contents` left alone.
fn encrypt_dict(
    d: &mut Dict,
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
    mem: &Mem,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    let is_sig = is_signature(d);
    for (k, v) in d.iter_mut() {
        check(cancel)?;
        if is_sig && k == b"Contents" {
            continue;
        }
        encrypt_deep_object(
            v, obj_nr, gen_nr, key, need_aes, r, random, mem, grown, cancel,
        )?;
    }
    Ok(())
}

/// `encryptDeepObject`: encrypts the strings of a direct object tree in place, the dictionary of
/// a stream included. Indirect references are left as they are. The cancel flag is checked at
/// every object; cancellation can leave the tree partly encrypted.
///
/// # Errors
/// As [`decrypt_key`] and `encrypt_bytes`; [`Error::Alloc`] for the copies of a string, charged
/// to `mem`, and for an encrypted string that outgrows the one it replaces, whose growth stays
/// charged to `grown`; [`Error::Cancelled`].
pub fn encrypt_deep_object(
    obj: &mut Object,
    obj_nr: i64,
    gen_nr: i64,
    key: &[u8],
    need_aes: bool,
    r: i64,
    random: RandomFn,
    mem: &Mem,
    grown: &mut Held,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    match obj {
        Object::Stream(sd) => {
            encrypt_dict(
                &mut sd.dict,
                obj_nr,
                gen_nr,
                key,
                need_aes,
                r,
                random,
                mem,
                grown,
                cancel,
            )?;
        }
        Object::Dict(d) => encrypt_dict(
            d, obj_nr, gen_nr, key, need_aes, r, random, mem, grown, cancel,
        )?,
        Object::Array(a) => {
            for v in a.iter_mut() {
                encrypt_deep_object(
                    v, obj_nr, gen_nr, key, need_aes, r, random, mem, grown, cancel,
                )?;
            }
        }
        Object::String(s) => {
            let t = encrypt_string_literal(s, obj_nr, gen_nr, key, need_aes, r, random, mem)?;
            // The copy was charged at the old length; escaping can double it.
            grown.grow_by(t.capacity().saturating_sub(s.len()) as u64)?;
            *s = t;
            check(cancel)?;
        }
        Object::Hex(h) => {
            let t = encrypt_hex_literal(h, obj_nr, gen_nr, key, need_aes, r, random, mem)?;
            grown.grow_by(t.capacity().saturating_sub(h.len()) as u64)?;
            *h = t;
            check(cancel)?;
        }
        _ => {}
    }
    check(cancel)
}

/// `fileID`: a file identifier, the upper-case hex of an MD5 over the time, the size of the file
/// read and, before PDF 2.0, the `String()` of every value of the info dictionary. The caller
/// passes those values dereferenced and in the key order of the dictionary (the determinism
/// patch sorts Go's map loop); `info_values` is empty for PDF 2.0. `file_size` is
/// `ctx.Read.ReadFileSize()`.
pub fn file_id(env: &WriteEnv, file_size: i64, info_values: &[Object]) -> Object {
    let mut h = Md5::new();
    h.update(env.now.go_string().as_bytes());
    h.update(file_size.to_string().as_bytes());
    struct Hash<'a>(&'a mut Md5);
    impl Sink for Hash<'_> {
        fn put(&mut self, b: &[u8]) {
            self.0.update(b);
        }
    }
    for o in info_values {
        o.write_go(&mut Hash(&mut h));
    }
    let mut digits = hex_encode(&h.finish());
    digits.make_ascii_uppercase();
    Object::Hex(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::parity_random;

    fn unhex(s: &str) -> Vec<u8> {
        hex_decode(s.as_bytes()).unwrap()
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn mem() -> Mem {
        Mem::with_max(1 << 30)
    }

    fn grown() -> Held {
        match mem().charge(0) {
            Ok(h) => h,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn decrypted_string_growth_is_charged() {
        let key = [5u8; 16];
        let plain = escape(&[b'\\'; 100]);
        let mut obj = Object::string(&plain);
        encrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            parity_random,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        let Object::String(s) = &obj else {
            unreachable!()
        };
        let n = s.len() as u64;
        assert!(n < 200, "{n}");
        let mut g = grown();
        decrypt_deep_object(&mut obj, 4, 0, &key, false, 3, &mem(), &mut g, &no_cancel()).unwrap();
        assert_eq!(obj.pdf_string(), Object::string(&plain).pdf_string());
        assert!(g.bytes() >= 200 - n, "{} {n}", g.bytes());
        // Over the limit, the growth fails the decryption.
        let mut obj = Object::string(&plain);
        encrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            parity_random,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        let mut g = Mem::with_max(50).charge(0).unwrap();
        let e = decrypt_deep_object(&mut obj, 4, 0, &key, false, 3, &mem(), &mut g, &no_cancel());
        assert!(matches!(e, Err(Error::Alloc { .. })), "{e:?}");
    }

    #[test]
    fn string_copies_are_charged_to_the_ledger() {
        let key = [5u8; 16];
        let enc = |obj: &mut Object, m: &Mem| {
            encrypt_deep_object(
                obj,
                4,
                0,
                &key,
                false,
                3,
                parity_random,
                m,
                &mut grown(),
                &no_cancel(),
            )
        };
        let mut obj = Object::string(b"title");
        let e = enc(&mut obj, &Mem::with_max(string_copies(5) - 1)).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e}");
        let m = Mem::with_max(string_copies(5));
        enc(&mut obj, &m).unwrap();
        assert_eq!((m.used(), m.peak()), (0, string_copies(5)));
        let Object::String(s) = &obj else {
            unreachable!()
        };
        let n = s.len();
        let small = Mem::with_max(string_copies(n) - 1);
        let e = decrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            &small,
            &mut grown(),
            &no_cancel(),
        );
        assert!(matches!(e, Err(Error::Alloc { .. })));
        decrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(obj.pdf_string(), Object::string(b"title").pdf_string());
    }

    // Port of TestDecryptKeyDoesNotMutateInput.
    #[test]
    fn decrypt_key_does_not_mutate_input() {
        let backing = [0xA5u8; 32];
        let key = &backing[..5];
        decrypt_key(7, 0, key, false).unwrap();
        assert_eq!(backing, [0xA5u8; 32]);
    }

    // Port of TestDecryptKeyObjectAndGenerationEncoding.
    #[test]
    fn decrypt_key_object_and_generation_encoding() {
        let got = decrypt_key(0x010203, 0x0405, &[1, 2, 3, 4, 5], false).unwrap();
        assert_eq!(
            got,
            [0x74, 0x47, 0x46, 0xAB, 0x61, 0xB8, 0xE4, 0xFB, 0x10, 0x56]
        );
        // AES appends "sAlT", and a key of 11 bytes or more gives the whole digest.
        assert_eq!(decrypt_key(1, 0, &[1; 5], true).unwrap().len(), 10);
        assert_eq!(decrypt_key(1, 0, &[1; 11], false).unwrap().len(), 16);
        assert_eq!(decrypt_key(1, 0, &[1; 16], true).unwrap().len(), 16);
        assert_ne!(
            decrypt_key(1, 0, &[1; 16], true).unwrap(),
            decrypt_key(1, 0, &[1; 16], false).unwrap()
        );
    }

    // Port of TestDecryptKeyGenerationRange and TestDecryptKeyObjectNumberRange.
    #[test]
    fn decrypt_key_ranges() {
        for g in [0, 65535] {
            assert!(decrypt_key(7, g, &[0; 5], false).is_ok());
        }
        for g in [-1, 65536] {
            let e = decrypt_key(7, g, &[0; 5], false).unwrap_err();
            assert!(e.to_string().contains("generation number"), "{e}");
        }
        for n in [0, i64::from(u32::MAX)] {
            assert!(decrypt_key(n, 0, &[0; 5], false).is_ok());
        }
        for n in [-1, i64::from(u32::MAX) + 1] {
            let e = decrypt_key(n, 0, &[0; 5], false).unwrap_err();
            assert!(e.to_string().contains("object number"), "{e}");
        }
    }

    // Port of TestDecryptAESBytesRejectsIVOnlyCiphertext.
    #[test]
    fn decrypt_aes_rejects_short_and_unaligned() {
        let e = decrypt_aes_bytes(&[0; 16], &[0; 16]).unwrap_err();
        assert!(e.is(Sentinel::AesTooShort));
        let e = decrypt_aes_bytes(&[0; 33], &[0; 16]).unwrap_err();
        assert!(e.is(Sentinel::AesUnaligned));
        assert!(decrypt_aes_bytes(&[0; 32], &[0; 15]).is_err());
    }

    // Port of TestAESBytesRoundTrip.
    #[test]
    fn aes_bytes_round_trip() {
        let key = [0u8; 16];
        let cases: [&[u8]; 4] = [b"encrypted content", b"", &[7u8; 16], &[9u8; 31]];
        for want in cases {
            let enc = encrypt_aes_bytes(want, &key, parity_random).unwrap();
            assert_eq!(enc.len(), 16 + (want.len() / 16 + 1) * 16);
            // The parity source gives the IV 0, 1, 2, ...
            assert_eq!(&enc[..16], &(0..16).collect::<Vec<u8>>()[..]);
            assert_eq!(decrypt_aes_bytes(&enc, &key).unwrap(), want);
        }
    }

    #[test]
    fn padding_is_removed_only_up_to_16() {
        // A plaintext whose last byte is above 16 is returned whole (not all producers pad).
        let key = [3u8; 16];
        let aes = Aes::new(&key).unwrap();
        let mut ct = vec![0x55u8; 16];
        ct.extend_from_slice(&[0x11; 16]);
        let iv: [u8; 16] = ct[..16].try_into().unwrap();
        aes.cbc_encrypt(&iv, &mut ct[16..]);
        let got = decrypt_aes_bytes(&ct, &key).unwrap();
        assert_eq!(got, [0x11; 16]);
        // 0x10 removes a whole block.
        let mut ct = vec![0u8; 16];
        ct.extend_from_slice(&[0x10; 16]);
        let iv = [0u8; 16];
        aes.cbc_encrypt(&iv, &mut ct[16..]);
        assert!(decrypt_aes_bytes(&ct, &key).unwrap().is_empty());
    }

    // The files below were made with qpdf (see auth.rs): the key is `--show-encryption-key`, the
    // ciphertexts are the stream (object 5) and the /Title string (object 2) of the output,
    // and the plaintexts were decrypted with an independent Python implementation. The stream
    // is the zlib of "Hello stream data".
    const STREAM_PLAIN: &str = "789cf348cdc9c957282e294a4dcc5548492c49040038fc065b";

    #[test]
    fn rc4_strings_and_streams_match_qpdf() {
        let key = unhex("df35d5310ce0c53dd9dd7be2b3337681");
        let title =
            decrypt_bytes(&unhex("b739ceb730b1816babcf8afc"), 2, 0, &key, false, 3).unwrap();
        assert_eq!(title, b"Secret title");
        let mem = Mem::with_max(1 << 20);
        let raw = mem
            .bytes_from(
                &unhex("4b06890a334183bca48ece7e23ef7b3e19b0a498eecbf22cee"),
                25,
            )
            .unwrap();
        let out = decrypt_stream(&raw, 5, 0, &key, false, 3, &mem).unwrap();
        assert_eq!(&out[..], &unhex(STREAM_PLAIN)[..]);
        // Go's io.Copy into a bytes.Buffer: 25 bytes and a separate EOF read grow it to 1024.
        assert_eq!(out.go_cap(), 1024);
        // Encrypting is the same operation.
        let back = encrypt_stream(&out, 5, 0, &key, false, 3, parity_random, &mem).unwrap();
        assert_eq!(&back[..], &raw[..]);
    }

    #[test]
    fn aes128_strings_and_streams_match_qpdf() {
        let key = unhex("e1f426ad7d5ebfd5cf9212832943ff6a");
        let title = decrypt_bytes(
            &unhex("6c0f3733f5c7c9bb4cd61ef7070f6438ea346501bab73afa04fd247a3aacc63f"),
            2,
            0,
            &key,
            true,
            4,
        )
        .unwrap();
        assert_eq!(title, b"Secret title");
        let mem = Mem::with_max(1 << 20);
        let raw = mem
            .bytes_from(
                &unhex("a1c5faa872474169027aa288b032501fe4f661bb3644f54874375ee3f77138d553291691f283c49c26a88da47d8c4b05"),
                64,
            )
            .unwrap();
        let out = decrypt_stream(&raw, 5, 0, &key, true, 4, &mem).unwrap();
        assert_eq!(&out[..], &unhex(STREAM_PLAIN)[..]);
        // Go decrypts in place and slices: the capacity is the input's minus the IV.
        assert_eq!(out.go_cap(), 48);
        assert_eq!(mem.used(), 48 + 32);
        // Round trip through the encrypt side (the IV is the parity one).
        let enc = encrypt_stream(&out, 5, 0, &key, true, 4, parity_random, &mem).unwrap();
        assert_eq!(enc.len(), 16 + 32);
        assert_eq!(enc.go_cap(), 48);
        let again = decrypt_stream(&enc, 5, 0, &key, true, 4, &mem).unwrap();
        assert_eq!(&again[..], &out[..]);
    }

    #[test]
    fn aes256_strings_and_streams_match_qpdf() {
        let key = unhex("330e793f4efcd4796111b906f89abd7caa9ec7cd3b8d4f2b02589ad493fa1904");
        // For revisions 5 and 6 the object number does not matter.
        for nr in [2, 99] {
            let title = decrypt_bytes(
                &unhex("54780314ccff12e572dfbdc1cb6c07c94835f0396191c47f663475206860fc7b"),
                nr,
                0,
                &key,
                true,
                6,
            )
            .unwrap();
            assert_eq!(title, b"Secret title");
        }
        let mem = Mem::with_max(1 << 20);
        let raw = mem
            .bytes_from(
                &unhex("2348ce327cc1fd42a67ac16c5e9fac0a6033c371c6e734f4996a24837fc2ea7717e7fdca00159c4b48c507da0051ab6f"),
                48,
            )
            .unwrap();
        let out = decrypt_stream(&raw, 5, 0, &key, true, 6, &mem).unwrap();
        assert_eq!(&out[..], &unhex(STREAM_PLAIN)[..]);
    }

    #[test]
    fn stream_decrypt_is_charged_and_limited() {
        let key = [1u8; 16];
        let big = Mem::with_max(1 << 20);
        let raw = big.bytes_from(&[0u8; 64], 64).unwrap();
        let tight = Mem::with_max(10);
        let e = decrypt_stream(&raw, 1, 0, &key, false, 3, &tight).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }));
        let e = decrypt_stream(&raw, 1, 0, &key, true, 4, &tight).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }));
        assert_eq!(tight.used(), 0);
        // An AES stream of one block cannot hold an IV and data.
        let one = big.bytes_from(&[0u8; 16], 16).unwrap();
        let e = decrypt_stream(&one, 1, 0, &key, true, 4, &big).unwrap_err();
        assert!(e.is(Sentinel::AesTooShort));
        // An empty RC4 stream has the capacity of an empty ReadFrom.
        let empty = big.bytes_from(&[], 0).unwrap();
        let out = decrypt_stream(&empty, 1, 0, &key, false, 3, &big).unwrap();
        assert_eq!((out.len(), out.go_cap()), (0, 512));
    }

    fn string_dict(strings: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in strings {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    #[test]
    fn deep_objects_round_trip_in_place() {
        let key = [5u8; 16];
        for (need_aes, r) in [(false, 3), (true, 4)] {
            let inner = Object::Dict(string_dict(&[
                ("S", Object::string(b"a \\(paren\\) \\\\ and \\n newline")),
                ("H", Object::hex_from_bytes(b"\x00\x01binary\xff")),
                ("N", Object::name("Name")),
                ("I", Object::Integer(7)),
                (
                    "R",
                    Object::Ref(crate::object::Ref {
                        num: 3,
                        generation: 0,
                    }),
                ),
                ("E", Object::string(b"")),
            ]));
            let original = Object::Array(vec![
                inner,
                Object::string(b"top"),
                Object::Array(vec![Object::hex_from_bytes(b"deep")]),
            ]);
            let mut obj = original.clone();
            encrypt_deep_object(
                &mut obj,
                9,
                0,
                &key,
                need_aes,
                r,
                parity_random,
                &mem(),
                &mut grown(),
                &no_cancel(),
            )
            .unwrap();
            assert_ne!(obj.pdf_string(), original.pdf_string());
            decrypt_deep_object(
                &mut obj,
                9,
                0,
                &key,
                need_aes,
                r,
                &mem(),
                &mut grown(),
                &no_cancel(),
            )
            .unwrap();
            // The empty string is encrypted to an IV and a block under AES (it was not empty any
            // more) and decrypts back to empty; the rest is exact.
            assert_eq!(obj.pdf_string(), original.pdf_string(), "aes={need_aes}");
        }
    }

    #[test]
    fn empty_strings_are_not_decrypted() {
        let original = Object::Array(vec![Object::string(b""), Object::Hex(Vec::new())]);
        let mut o = original.clone();
        decrypt_deep_object(
            &mut o,
            1,
            0,
            &[1; 16],
            true,
            4,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(o.pdf_string(), original.pdf_string());
    }

    #[test]
    fn signature_contents_are_left_alone() {
        let key = [5u8; 16];
        for ty in ["Sig", "DocTimeStamp"] {
            let d = string_dict(&[
                ("Type", Object::name(ty)),
                ("Contents", Object::hex_from_bytes(b"signature")),
                ("Name", Object::string(b"x")),
            ]);
            let mut obj = Object::Dict(d.clone());
            encrypt_deep_object(
                &mut obj,
                4,
                0,
                &key,
                false,
                3,
                parity_random,
                &mem(),
                &mut grown(),
                &no_cancel(),
            )
            .unwrap();
            let Object::Dict(after) = &obj else {
                unreachable!()
            };
            let text = |d: &Dict, k: &[u8]| d.find(k).unwrap().pdf_string();
            assert_eq!(text(after, b"Contents"), text(&d, b"Contents"));
            assert_ne!(text(after, b"Name"), text(&d, b"Name"));
        }
        // FT wins over Type, and a null FT counts as absent.
        let d = string_dict(&[("FT", Object::name("Sig")), ("Type", Object::name("Annot"))]);
        assert!(is_signature(&d));
        let d = string_dict(&[("FT", Object::Null), ("Type", Object::name("Sig"))]);
        assert!(is_signature(&d));
        let d = string_dict(&[("FT", Object::name("Tx")), ("Type", Object::name("Sig"))]);
        assert!(!is_signature(&d));
    }

    #[test]
    fn stream_dictionaries_are_encrypted_but_not_decrypted_deeply() {
        let key = [5u8; 16];
        let mut sd = crate::object::StreamDict::default();
        sd.dict.set(b"T", Object::string(b"title"));
        let mut obj = Object::Stream(Box::new(sd));
        let plain = obj.pdf_string();
        encrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            parity_random,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        let encrypted = obj.pdf_string();
        assert_ne!(encrypted, plain);
        // Go's decryptDeepObject has no StreamDict case: decryptObjectStrings gets the
        // dictionary, not the stream.
        decrypt_deep_object(
            &mut obj,
            4,
            0,
            &key,
            false,
            3,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(obj.pdf_string(), encrypted);
    }

    #[test]
    fn decrypt_errors_name_the_entry() {
        let mut o = Object::Dict(string_dict(&[("Title", Object::string(b"short"))]));
        let e = decrypt_deep_object(
            &mut o,
            1,
            0,
            &[1; 16],
            true,
            4,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(e.is(Sentinel::AesTooShort));
        assert!(e.to_string().contains("decrypt dict entry Title"));
        // A top-level string is not wrapped.
        let mut o = Object::string(b"short");
        let e = decrypt_deep_object(
            &mut o,
            1,
            0,
            &[1; 16],
            true,
            4,
            &mem(),
            &mut grown(),
            &no_cancel(),
        )
        .unwrap_err();
        assert_eq!(e, Error::Sentinel(Sentinel::AesTooShort));
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let cancel = AtomicBool::new(true);
        let mut o = Object::string(b"x");
        let e = decrypt_deep_object(
            &mut o,
            1,
            0,
            &[1; 16],
            false,
            3,
            &mem(),
            &mut grown(),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(e, Error::Cancelled);
        let e = encrypt_deep_object(
            &mut o,
            1,
            0,
            &[1; 16],
            false,
            3,
            parity_random,
            &mem(),
            &mut grown(),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    #[test]
    fn file_id_is_upper_case_hex_of_an_md5() {
        let env = WriteEnv::parity();
        let a = file_id(&env, 100, &[]);
        let Object::Hex(h) = &a else { unreachable!() };
        assert_eq!(h.len(), 32);
        assert!(
            h.iter()
                .all(|c| c.is_ascii_digit() || (b'A'..=b'F').contains(c))
        );
        // MD5("2000-01-01 00:00:00 +0000 UTC" + "100").
        let want = md5::md5(&[&b"2000-01-01 00:00:00 +0000 UTC"[..], &b"100"[..]]);
        let mut digits = hex_encode(&want);
        digits.make_ascii_uppercase();
        assert_eq!(h, &digits);
        let b = file_id(&env, 100, &[Object::string(b"x")]);
        let c = file_id(&env, 101, &[]);
        assert_ne!(a.pdf_string(), b.pdf_string());
        assert_ne!(a.pdf_string(), c.pdf_string());
    }
}
