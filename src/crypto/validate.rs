//! Encryption dictionary validation and the crypt filter selection (Go: crypto.go,
//! `supportedEncryption` :1687, `validateEncryptionVersion` :1310, `prepareCryptFilters` :1427,
//! `streamCryptMethod` :1816 and the helpers they call).
//!
//! What is fixed, because pdf-rs only reaches one configuration (docs/scope.md section 4):
//!
//! * Validation is relaxed. Where Go's relaxed mode digests a specification violation (a
//!   `specViolation` that only goes to the log or a notice), the violation is accepted and
//!   dropped; where it is an error, it is an error.
//! * The security handler is `Standard`. `Adobe.PubSec` is refused as unsupported, so the
//!   `pubKeySecHandler` branches (Recipients, SubFilter) are dead and not ported.
//!
//! The checks run against the document's [`XRefTable`], as Go's run against its context, and set
//! the `aes4_*` fields of the table where Go sets `ctx.AES4*`.

use super::auth::normalize_permission;
use super::{malformed, malformed_ctx, unsupported};
use crate::error::{Error, Sentinel};
use crate::model::{Enc, XRefTable};
use crate::object::stream::StreamDict;
use crate::object::{Dict, Object};
use std::sync::atomic::{AtomicBool, Ordering};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The Go type name `%T` prints, for messages.
fn kind(o: &Object) -> &'static str {
    match o {
        Object::Null => "nil",
        Object::Bool(_) => "types.Boolean",
        Object::Integer(_) => "types.Integer",
        Object::Float(_) => "types.Float",
        Object::Name(_) => "types.Name",
        Object::String(_) => "types.StringLiteral",
        Object::Hex(_) => "types.HexLiteral",
        Object::Ref(_) => "types.IndirectRef",
        Object::Array(_) => "types.Array",
        Object::Dict(_) => "types.Dict",
        Object::Stream(_) => "types.StreamDict",
    }
}

/// `DereferenceNameEntry`.
fn name_entry(x: &mut XRefTable, d: &Dict, key: &str) -> Result<Option<Vec<u8>>, Error> {
    x.dereference_name_entry(d, key.as_bytes()).map(|(n, _)| n)
}

/// `DereferenceIntegerEntry`.
fn int_entry(x: &mut XRefTable, d: &Dict, key: &str) -> Result<Option<i64>, Error> {
    x.dereference_integer_entry(d, key.as_bytes())
        .map(|(i, _)| i)
}

/// `DereferenceBooleanEntry`.
fn bool_entry(x: &mut XRefTable, d: &Dict, key: &str) -> Result<Option<bool>, Error> {
    x.dereference_boolean_entry(d, key.as_bytes())
        .map(|(b, _)| b)
}

/// `encryptionIntegerEntry`.
fn encryption_integer_entry(
    x: &mut XRefTable,
    d: &Dict,
    key: &str,
    dict_name: &str,
) -> Result<Option<i64>, Error> {
    int_entry(x, d, key).map_err(|e| malformed_ctx(e, format!("{dict_name} entry {key:?}")))
}

/// `attributeEncryptionError`: names the encryption dictionary object in the message.
fn attribute(err: Error, enc_obj_nr: i64) -> Error {
    if enc_obj_nr <= 0 {
        err
    } else {
        err.context(format!("obj#{enc_obj_nr}"))
    }
}

/// `getR`.
fn get_r(x: &mut XRefTable, d: &Dict) -> Result<i64, Error> {
    // Algorithm support is determined before the catalog and effective PDF version are known.
    const MAX_R: i64 = 7;
    let Some(r) = encryption_integer_entry(x, d, "R", "encrypt dict")? else {
        return Err(malformed("required entry \"R\" missing"));
    };
    if !(2..=MAX_R).contains(&r) {
        return Err(malformed(format!("invalid encrypt \"R\" {r}")));
    }
    if r == 7 {
        return Err(unsupported("encrypt \"R\" 7"));
    }
    Ok(r)
}

/// One of `OE`, `UE` or `Perms`, which must have exactly `len` bytes.
fn aes256_entry(d: &Dict, key: &str, len: usize) -> Result<Vec<u8>, Error> {
    let b = d
        .string_entry_bytes(key.as_bytes())
        .map_err(|e| malformed_ctx(e, format!("entry {key:?}")))?
        .unwrap_or_default();
    if b.len() != len {
        return Err(malformed(format!(
            "required entry {key:?} missing or not {len} bytes"
        )));
    }
    Ok(b)
}

/// `validateAES256Parameters`: `OE`, `UE` and `Perms` (empty below revision 5).
pub(super) fn validate_aes256_parameters(
    d: &Dict,
    r: i64,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), Error> {
    if !(5..=7).contains(&r) {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    Ok((
        aes256_entry(d, "OE", 32)?,
        aes256_entry(d, "UE", 32)?,
        aes256_entry(d, "Perms", 16)?,
    ))
}

/// `validatePasswordEntry`. A short entry is an error, unless it is a digested violation.
fn validate_password_entry(
    d: &Dict,
    key: &str,
    min_len: usize,
    digest_short: bool,
) -> Result<Vec<u8>, Error> {
    let b = d
        .string_entry_bytes(key.as_bytes())
        .map_err(|e| malformed_ctx(e, format!("entry {key:?}")))?
        .unwrap_or_default();
    if b.len() < min_len && !digest_short {
        return Err(malformed(format!(
            "required entry {key:?} shorter than {min_len} bytes"
        )));
    }
    Ok(b)
}

/// `validateOAndU` in relaxed mode: revisions 2 to 4 take an `O` or `U` of any length;
/// revisions 5 and 6 need 48 bytes.
pub(super) fn validate_o_and_u(d: &Dict, r: i64) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let (min_len, digest_short) = if r >= 5 { (48, false) } else { (32, true) };
    let o = validate_password_entry(d, "O", min_len, digest_short)?;
    let u = validate_password_entry(d, "U", min_len, digest_short)?;
    Ok((o, u))
}

/// `checkCFLengthV2`.
fn check_cf_length_v2(length: i64, pdf20: bool) -> Result<(), Error> {
    if (5..=16).contains(&length) {
        return Ok(());
    }
    // Legacy bit counts are accepted (as a violation) for earlier PDFs.
    if pdf20 || !(40..=128).contains(&length) || length % 8 != 0 {
        return Err(malformed(format!("invalid CF length: {length}")));
    }
    Ok(())
}

/// `checkCFLengthAESV2`.
fn check_cf_length_aes_v2(length: i64, pdf20: bool) -> Result<(), Error> {
    const AES_V2_KEY_LENGTH: i64 = 16;
    if length == AES_V2_KEY_LENGTH || (!pdf20 && length == AES_V2_KEY_LENGTH * 8) {
        return Ok(());
    }
    Err(malformed(format!(
        "invalid CF length, got {length} want {AES_V2_KEY_LENGTH}"
    )))
}

/// `validateCFLength`. An `AESV3` length other than 32 is only a digested violation.
pub(super) fn validate_cf_length(
    length: Option<i64>,
    cfm: Option<&[u8]>,
    pdf20: bool,
) -> Result<(), Error> {
    let (Some(length), Some(cfm)) = (length, cfm) else {
        return Ok(());
    };
    match cfm {
        b"V2" => check_cf_length_v2(length, pdf20),
        b"AESV2" => check_cf_length_aes_v2(length, pdf20),
        _ => Ok(()),
    }
}

/// `checkCryptFilterCFM`.
fn check_crypt_filter_cfm(cfm: &[u8], v: i64) -> Result<(), Error> {
    let allowed: &[&[u8]] = match v {
        4 => &[&b"V2"[..], &b"AESV2"[..]],
        5 => &[&b"AESV3"[..]],
        6 => &[&b"AESV4"[..]],
        _ => &[],
    };
    if !allowed.is_empty() && !allowed.contains(&cfm) {
        return Err(malformed(format!(
            "crypt filter invalid entry \"CFM\": {}",
            lossy(cfm)
        )));
    }
    Ok(())
}

/// `validateCryptFilterAuthEvent`.
fn validate_crypt_filter_auth_event(
    x: &mut XRefTable,
    d: &Dict,
    allow_ef_open: bool,
) -> Result<(), Error> {
    let n = name_entry(x, d, "AuthEvent")
        .map_err(|e| malformed_ctx(e, "crypt filter entry \"AuthEvent\"".into()))?;
    if let Some(n) = n
        && n != b"DocOpen"
        && (!allow_ef_open || n != b"EFOpen")
    {
        return Err(malformed("crypt filter invalid entry \"AuthEvent\""));
    }
    Ok(())
}

/// `cryptFilterCFM`.
fn crypt_filter_cfm(x: &mut XRefTable, d: &Dict, v: i64) -> Result<Option<Vec<u8>>, Error> {
    let n = name_entry(x, d, "CFM")
        .map_err(|e| malformed_ctx(e, "crypt filter entry \"CFM\"".into()))?;
    if let Some(n) = &n {
        check_crypt_filter_cfm(n, v)?;
    }
    Ok(n)
}

fn is_aes_cfm(cfm: Option<&[u8]>) -> bool {
    matches!(cfm, Some(b"AESV2" | b"AESV3" | b"AESV4"))
}

/// `cryptFilterForDecryption`: whether the filter uses AES.
fn crypt_filter_for_decryption(
    x: &mut XRefTable,
    d: &Dict,
    v: i64,
    allow_ef_open: bool,
) -> Result<bool, Error> {
    let cfm = crypt_filter_cfm(x, d, v)?;
    // The representation is checked now; the version-dependent length rules come later.
    encryption_integer_entry(x, d, "Length", "crypt filter")?;
    validate_crypt_filter_auth_event(x, d, allow_ef_open)?;
    Ok(is_aes_cfm(cfm.as_deref()))
}

/// `validateCryptFilter`.
fn validate_crypt_filter(
    x: &mut XRefTable,
    d: &Dict,
    v: i64,
    allow_ef_open: bool,
    pdf20: bool,
) -> Result<bool, Error> {
    let aes = crypt_filter_for_decryption(x, d, v, allow_ef_open)?;
    let cfm = crypt_filter_cfm(x, d, v)?;
    let length = encryption_integer_entry(x, d, "Length", "crypt filter")?;
    if length.is_none() && pdf20 {
        return Err(malformed("crypt filter missing entry \"Length\""));
    }
    if v == 4 || v == 5 {
        validate_cf_length(length, cfm.as_deref(), pdf20)?;
    }
    Ok(aes)
}

/// The three crypt filter selections of an encryption dictionary: Go's `StmF`, `StrF`, `EFF`.
const SELECTIONS: [&str; 3] = ["StmF", "StrF", "EFF"];

/// Sets the `aes4_*` field of the table for the selection named `key`.
fn set_aes4(x: &mut XRefTable, key: &str, aes: bool) {
    match key {
        "StrF" => x.aes4_strings = aes,
        "StmF" => x.aes4_streams = aes,
        _ => x.aes4_embedded_streams = aes,
    }
}

/// `prepareCryptFilters`: selects the methods before the effective PDF version is known. A
/// selection that is absent or `Identity` leaves its `aes4_*` field as it was.
fn prepare_crypt_filters(x: &mut XRefTable, d: &Dict, v: i64) -> Result<(), Error> {
    let Some(cf) = d.dict_entry(b"CF") else {
        return Err(malformed("encrypt dict, required entry \"CF\" missing"));
    };
    for key in SELECTIONS {
        let name = name_entry(x, d, key)
            .map_err(|e| malformed_ctx(e, format!("encrypt dict entry {key:?}")))?;
        let Some(name) = name.filter(|n| n != b"Identity") else {
            continue;
        };
        let Some(filter) = cf.dict_entry(&name) else {
            return Err(malformed(format!("entry {:?} missing in CF", lossy(&name))));
        };
        let aes = crypt_filter_for_decryption(x, filter, v, key == "EFF")
            .map_err(|e| e.context(format!("encrypt dict entry {key:?}")))?;
        set_aes4(x, key, aes);
    }
    Ok(())
}

/// `validateStmf`, `validateStrf` and `validateEFF`, one call per selection.
fn validate_selection(
    x: &mut XRefTable,
    d: &Dict,
    cf_dict: &Dict,
    v: i64,
    key: &str,
    pdf20: bool,
) -> Result<Option<bool>, Error> {
    let n = name_entry(x, d, key)
        .map_err(|e| malformed_ctx(e, format!("encrypt dict entry {key:?}")))?;
    let Some(n) = n.filter(|n| n != b"Identity") else {
        return Ok(None);
    };
    // `locateCFEntry`.
    let wrap = |e: Error| e.context(format!("encrypt dict entry {key:?}"));
    let Some(d1) = cf_dict.dict_entry(&n) else {
        return Err(wrap(malformed(format!(
            "entry {:?} missing in \"CF\"",
            lossy(&n)
        ))));
    };
    validate_crypt_filter(x, d1, v, key == "EFF", pdf20)
        .map(Some)
        .map_err(wrap)
}

/// `validateCryptFilters`: the methods again, now with the effective version.
fn validate_crypt_filters(x: &mut XRefTable, d: &Dict, v: i64, pdf20: bool) -> Result<(), Error> {
    let Some(cf_dict) = d.dict_entry(b"CF") else {
        return Err(malformed("encrypt dict, required entry \"CF\" missing"));
    };
    for key in SELECTIONS {
        if let Some(aes) = validate_selection(x, d, cf_dict, v, key, pdf20)? {
            set_aes4(x, key, aes);
        }
    }
    Ok(())
}

fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `validateCryptFilterLengths`: PDF 2.0 also requires `Length` for the filters that `StmF`,
/// `StrF` and `EFF` do not select.
fn validate_crypt_filter_lengths(
    x: &mut XRefTable,
    cf: Option<&Dict>,
    v: i64,
    pdf20: bool,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let Some(cf) = cf else {
        return Ok(());
    };
    for (name, obj) in cf.iter() {
        check(cancel)?;
        // The standard Identity filter cannot be redefined by a CF entry (Table 20).
        if name == b"Identity" {
            continue;
        }
        let filter = match x.dereference(obj) {
            Ok(Object::Dict(d)) => d.clone(),
            Ok(Object::Null) => Dict::default(),
            Ok(o) => {
                return Err(malformed(format!(
                    "crypt filter {:?}: expected dict, got {}",
                    lossy(name),
                    kind(o)
                )));
            }
            Err(e) => {
                return Err(malformed_ctx(e, format!("crypt filter {:?}", lossy(name))));
            }
        };
        let length = encryption_integer_entry(x, &filter, "Length", "crypt filter")
            .map_err(|e| e.context(format!("crypt filter {:?}", lossy(name))))?;
        if length.is_none() && pdf20 {
            return Err(malformed(format!(
                "crypt filter {:?} missing entry \"Length\"",
                lossy(name)
            )));
        }
        if v == 5 {
            // Method checks belong to filter selection. Go also checks an `AESV3` length here,
            // and in relaxed mode that is only a digested violation; the lookup stays because it
            // can decode an object stream.
            let _ = name_entry(x, &filter, "CFM");
        }
    }
    Ok(())
}

/// `validateEncryptionVersion`: the crypt filter checks that need the effective PDF version, run
/// after the catalog is known. Does nothing without parameters (`x.e`) or below `V` 4.
///
/// # Errors
/// [`Error::MalformedEncryption`] and [`Error::UnsupportedEncryption`] for a bad dictionary;
/// [`Error::Cancelled`].
pub fn validate_encryption_version(x: &mut XRefTable, cancel: &AtomicBool) -> Result<(), Error> {
    let Some(v) = x.e.as_ref().map(|e| e.v) else {
        return Ok(());
    };
    if v < 4 {
        return Ok(());
    }
    let enc_obj_nr = x.encrypt.map_or(0, |r| r.num);
    let run = |x: &mut XRefTable| -> Result<(), Error> {
        let d = x
            .encrypt_dict()
            .map_err(classify_encryption_dictionary_error)?
            .clone();
        let pdf20 = x.pdf20()?;
        validate_crypt_filters(x, &d, v, pdf20)?;
        if pdf20 || v == 5 {
            // Earlier inputs did not check unselected filters here; their relaxed acceptance
            // is kept.
            if let Err(e) = validate_crypt_filter_lengths(x, d.dict_entry(b"CF"), v, pdf20, cancel)
                && (pdf20 || cancel.load(Ordering::Relaxed))
            {
                return Err(e);
            }
        }
        Ok(())
    };
    run(x).map_err(|e| attribute(e, enc_obj_nr))
}

/// `validateEncryptFilter`.
fn validate_encrypt_filter(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let filter = name_entry(x, d, "Filter")
        .map_err(|e| malformed_ctx(e, "encrypt dict entry \"Filter\"".into()))?;
    let Some(filter) = filter else {
        return Err(malformed("required entry \"Filter\" missing"));
    };
    // Adobe.PubSec is not supported.
    if filter != b"Standard" {
        return Err(unsupported(format!("filter {}", lossy(&filter))));
    }
    Ok(())
}

/// `validateEncryptSubFilter` for the standard handler: only the entry's type is checked.
fn validate_encrypt_sub_filter(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    name_entry(x, d, "SubFilter")
        .map_err(|e| malformed_ctx(e, "encrypt dict entry \"SubFilter\"".into()))?;
    Ok(())
}

/// `validateEncryptV`.
fn validate_encrypt_v(x: &mut XRefTable, d: &Dict) -> Result<i64, Error> {
    let Some(v) = encryption_integer_entry(x, d, "V", "encrypt dict")? else {
        return Err(malformed("required entry \"V\" missing"));
    };
    match v {
        1..=5 => Ok(v),
        6 => Err(unsupported("encrypt \"V\" 6 (AESV4)")),
        _ => Err(malformed(format!("invalid encrypt \"V\" {v}"))),
    }
}

/// `validateEncryptLength`.
fn validate_encrypt_length(x: &mut XRefTable, d: &Dict, v: i64) -> Result<i64, Error> {
    match v {
        1 => Ok(40),
        2 | 4 => {
            let Some(i) = encryption_integer_entry(x, d, "Length", "encrypt dict")? else {
                return Ok(40);
            };
            if !(40..=128).contains(&i) || i % 8 != 0 {
                return Err(malformed(format!("invalid encrypt \"Length\" {i}")));
            }
            Ok(i)
        }
        5 => Ok(256),
        6 => Err(unsupported("encrypt \"V\" 6 (AESV4)")),
        _ => Err(malformed(format!("invalid encrypt \"V\" {v}"))),
    }
}

/// `encryptionVersionParameters`: `(V, L, R)`; selects the crypt filter methods on the way.
fn encryption_version_parameters(x: &mut XRefTable, d: &Dict) -> Result<(i64, i64, i64), Error> {
    let v = validate_encrypt_v(x, d)?;
    let l = validate_encrypt_length(x, d, v)?;
    if (4..=6).contains(&v) {
        prepare_crypt_filters(x, d, v)?;
    }
    let r = get_r(x, d)?;
    // Revisions 5 and 6 need V=5's 256-bit key; older revisions use smaller keys.
    if (v == 5 && r < 5) || (v < 5 && r >= 5) {
        return Err(malformed(format!(
            "encrypt \"R\" {r} inconsistent with \"V\" {v}"
        )));
    }
    Ok((v, l, r))
}

/// `validateEncryptPermissions` for the standard handler: `P` (normalised to a signed 32-bit
/// value) and `EncryptMetadata` (default true).
fn validate_encrypt_permissions(x: &mut XRefTable, d: &Dict) -> Result<(i64, bool), Error> {
    let Some(p) = encryption_integer_entry(x, d, "P", "encrypt dict")? else {
        return Err(malformed("required entry \"P\" missing"));
    };
    let (p, _violation) = normalize_permission(p)?;
    let emd = bool_entry(x, d, "EncryptMetadata")
        .map_err(|e| malformed_ctx(e, "encrypt dict entry \"EncryptMetadata\"".into()))?;
    Ok((p, emd.unwrap_or(true)))
}

/// `supportedEncryption`: validates the encryption dictionary `d` and returns the parameters it
/// describes, and sets the `aes4_*` fields of the table for the crypt filters it selects.
/// `Enc::id` is left empty; the caller sets it from the trailer `ID`. This runs before the PDF
/// version is known, so the `Length` and AES `CFM` rules that depend on it are checked later by
/// [`validate_encryption_version`].
///
/// # Errors
/// [`Error::MalformedEncryption`] and [`Error::UnsupportedEncryption`] (`Adobe.PubSec`, `V` 6,
/// `R` 7); errors of the dereferences that are not about structure pass through.
pub fn supported_encryption(x: &mut XRefTable, d: &Dict) -> Result<Enc, Error> {
    let enc_obj_nr = x.encrypt.map_or(0, |r| r.num);
    let run = |x: &mut XRefTable| -> Result<Enc, Error> {
        validate_encrypt_filter(x, d)?;
        validate_encrypt_sub_filter(x, d)?;
        let (v, l, r) = encryption_version_parameters(x, d)?;
        let (o, u) = validate_o_and_u(d, r)?;
        let (oe, ue, perms) = validate_aes256_parameters(d, r)?;
        let (p, emd) = validate_encrypt_permissions(x, d)?;
        Ok(Enc {
            o,
            oe,
            u,
            ue,
            l,
            p,
            perms,
            r,
            v,
            emd,
            id: Vec::new(),
        })
    };
    run(x).map_err(|e| attribute(e, enc_obj_nr))
}

/// `classifyEncryptionDictionaryError`: an error that says the dictionary is missing, of the
/// wrong type or unresolvable is a malformed encryption dictionary; any other error (a limit, the
/// cancel flag) is not. Go also counts `errCorruptDictObject` and the parser's
/// `ErrDictionaryCorrupt`, which have no sentinel here: the read path that dereferences the
/// dictionary classifies those itself.
pub fn classify_encryption_dictionary_error(err: Error) -> Error {
    const STRUCTURAL: [Sentinel; 5] = [
        Sentinel::UnregisteredObject,
        Sentinel::NilDereferenced,
        Sentinel::ReferenceDoesNotExist,
        Sentinel::MissingEncryptDictObject,
        Sentinel::WrongTypeEncryptDictObject,
    ];
    if STRUCTURAL.iter().any(|s| err.is(*s)) {
        Error::MalformedEncryption(Box::new(err))
    } else {
        err
    }
}

/// `ctx.E.V`. Go dereferences a nil `E` here and panics.
fn enc_v(x: &XRefTable) -> Result<i64, Error> {
    x.e.as_ref()
        .map(|e| e.v)
        .ok_or(Error::Panic("encryption parameters are not set"))
}

/// `defaultCryptFilterEncrypted`: whether the default filter named by `key` (`StmF` or `StrF`)
/// transforms data. Everything is encrypted without parameters or below `V` 4. A missing entry is
/// `Identity` (PDF 2.0, Table 20).
///
/// # Errors
/// [`Error::MalformedEncryption`] for a missing or invalid encryption dictionary or an entry that
/// is not a name.
pub fn default_crypt_filter_encrypted(x: &mut XRefTable, key: &str) -> Result<bool, Error> {
    if x.e.as_ref().is_none_or(|e| e.v < 4) {
        return Ok(true);
    }
    let entry = x
        .encrypt_dict()
        .map_err(classify_encryption_dictionary_error)?
        .find(key.as_bytes())
        .cloned();
    let (n, _) = x
        .name_from_value(key.as_bytes(), entry.as_ref())
        .map_err(|e| malformed_ctx(e, format!("encrypt dict entry {key:?}")))?;
    Ok(n.is_some_and(|n| n != b"Identity"))
}

/// `stringsEncrypted`: whether strings are decrypted at all.
///
/// # Errors
/// As [`default_crypt_filter_encrypted`].
pub fn strings_encrypted(x: &mut XRefTable) -> Result<bool, Error> {
    if x.enc_key.is_empty() {
        return Ok(false);
    }
    default_crypt_filter_encrypted(x, "StrF")
}

/// `streamCryptFilterName`: the name a stream's first `Crypt` filter selects, if it has one.
fn stream_crypt_filter_name(x: &mut XRefTable, sd: &StreamDict) -> Result<Option<Vec<u8>>, Error> {
    let Some(first) = sd.filter_pipeline.first().filter(|f| f.name == b"Crypt") else {
        return Ok(None);
    };
    let Some(dp) = &first.decode_parms else {
        return Ok(Some(b"Identity".to_vec()));
    };
    let n = name_entry(x, dp, "Name")
        .map_err(|e| malformed_ctx(e, "stream Crypt filter Name".into()))?;
    Ok(Some(n.unwrap_or_else(|| b"Identity".to_vec())))
}

/// `streamCryptMethod`: `(encrypted, aes)` for a stream.
///
/// # Errors
/// [`Error::MalformedEncryption`]; [`Error::UnsupportedEncryption`] for a security-handler
/// filter or a method other than `V2`, `AESV2` and `AESV3`.
pub fn stream_crypt_method(x: &mut XRefTable, sd: &StreamDict) -> Result<(bool, bool), Error> {
    let Some(name) = stream_crypt_filter_name(x, sd)? else {
        let encrypted = default_crypt_filter_encrypted(x, "StmF")?;
        return Ok((encrypted, x.aes4_streams));
    };
    if name == b"Identity" {
        return Ok((false, false));
    }
    let shown = lossy(&name);
    let cfd = x
        .encrypt_dict()
        .map_err(classify_encryption_dictionary_error)?
        .dict_entry(b"CF")
        .and_then(|cf| cf.dict_entry(&name))
        .cloned();
    let Some(cfd) = cfd else {
        return Err(malformed(format!(
            "stream crypt filter {shown:?} missing from CF"
        )));
    };
    let cfm = name_entry(x, &cfd, "CFM")
        .map_err(|e| malformed_ctx(e, format!("stream crypt filter {shown:?} CFM")))?;
    let Some(cfm) = cfm.filter(|c| c != b"None") else {
        return Err(unsupported(format!(
            "stream crypt filter {shown:?} uses security-handler decryption"
        )));
    };
    match cfm.as_slice() {
        b"V2" | b"AESV2" | b"AESV3" => {
            // Explicit filters obey the same method restrictions as default filters (7.6.4.1,
            // Table 25).
            check_crypt_filter_cfm(&cfm, enc_v(x)?)
                .map_err(|e| e.context(format!("stream crypt filter {shown:?}")))?;
            Ok((true, cfm != b"V2"))
        }
        _ => Err(unsupported(format!(
            "stream crypt filter {shown:?} CFM {}",
            lossy(&cfm)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Entry, Version};
    use crate::object::Ref;
    use crate::object::stream::PdfFilter;

    /// An empty PDF 1.7 table, Go's `model.NewContext` plus a header version.
    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    fn put(x: &mut XRefTable, nr: i64, o: Object) {
        x.table.insert(nr, Entry::gen0(o));
    }

    fn reference(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn name(s: &str) -> Object {
        Object::name(s)
    }

    fn int(i: i64) -> Object {
        Object::Integer(i)
    }

    fn hex(n: usize) -> Object {
        Object::hex_from_bytes(&vec![0u8; n])
    }

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v);
        }
        d
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// Go's `newEncryptDict` for the cases the tests use.
    fn encrypt_dict(aes: bool, key_length: i64, p: i64) -> Dict {
        let mut d = dict(vec![
            ("Filter", name("Standard")),
            ("P", int(p)),
            ("O", hex(32)),
            ("U", hex(32)),
        ]);
        let cf = |cfm: &str, length: i64| {
            Object::Dict(dict(vec![(
                "StdCF",
                Object::Dict(dict(vec![
                    ("AuthEvent", name("DocOpen")),
                    ("CFM", name(cfm)),
                    ("Length", int(length)),
                ])),
            )]))
        };
        match (aes, key_length) {
            (_, 40) => {
                d.set(b"V", int(1));
                d.set(b"R", int(2));
            }
            (false, 128) => {
                d.set(b"V", int(2));
                d.set(b"R", int(3));
                d.set(b"Length", int(128));
            }
            (true, 128) => {
                d.set(b"V", int(4));
                d.set(b"R", int(4));
                d.set(b"Length", int(128));
                d.set(b"StmF", name("StdCF"));
                d.set(b"StrF", name("StdCF"));
                d.set(b"CF", cf("AESV2", 16));
            }
            _ => {
                d.set(b"V", int(5));
                d.set(b"R", int(6));
                d.set(b"Length", int(256));
                d.set(b"O", hex(48));
                d.set(b"U", hex(48));
                d.set(b"OE", hex(32));
                d.set(b"UE", hex(32));
                d.set(b"Perms", hex(16));
                d.set(b"StmF", name("StdCF"));
                d.set(b"StrF", name("StdCF"));
                d.set(b"CF", cf("AESV3", 32));
            }
        }
        d
    }

    /// A table whose encryption dictionary is object 1 and whose parameters are `v` and `r`.
    fn encrypted(d: Dict, v: i64, r: i64) -> XRefTable {
        let mut x = table();
        put(&mut x, 1, Object::Dict(d));
        x.encrypt = Some(Ref {
            num: 1,
            generation: 0,
        });
        x.e = Some(Enc {
            v,
            r,
            ..Enc::default()
        });
        x
    }

    fn is_malformed(e: &Error) -> bool {
        matches!(e.root(), Error::MalformedEncryption(_))
    }

    fn is_unsupported(e: &Error) -> bool {
        matches!(e.root(), Error::UnsupportedEncryption(_))
    }

    // Port of TestEncryptionDictionaryErrorsClassified.
    #[test]
    fn errors_are_classified() {
        let mut x = table();
        let e = validate_encrypt_filter(&mut x, &Dict::default()).unwrap_err();
        assert!(is_malformed(&e));
        let d = dict(vec![("Filter", name("Adobe.PubSec"))]);
        assert!(is_unsupported(
            &validate_encrypt_filter(&mut x, &d).unwrap_err()
        ));
        assert!(is_malformed(
            &validate_encrypt_v(&mut x, &Dict::default()).unwrap_err()
        ));
        let d = dict(vec![("V", int(9))]);
        assert!(is_malformed(&validate_encrypt_v(&mut x, &d).unwrap_err()));
        let d = dict(vec![("V", int(6))]);
        assert!(is_unsupported(&validate_encrypt_v(&mut x, &d).unwrap_err()));
        let d = dict(vec![("Length", int(44))]);
        assert!(is_malformed(
            &validate_encrypt_length(&mut x, &d, 2).unwrap_err()
        ));
        // Missing P.
        let d = dict(vec![
            ("Filter", name("Standard")),
            ("V", int(1)),
            ("R", int(2)),
            ("O", hex(32)),
            ("U", hex(32)),
        ]);
        let e = supported_encryption(&mut x, &d).unwrap_err();
        assert!(is_malformed(&e));
        assert_eq!(e.class(), "encryption/malformed");
    }

    // Port of TestClassifyEncryptionDictionaryErrorDistinguishesStructuralFailures.
    #[test]
    fn dictionary_lookup_errors_are_classified() {
        for s in [
            Sentinel::UnregisteredObject,
            Sentinel::NilDereferenced,
            Sentinel::ReferenceDoesNotExist,
            Sentinel::MissingEncryptDictObject,
            Sentinel::WrongTypeEncryptDictObject,
        ] {
            let e = classify_encryption_dictionary_error(Error::Sentinel(s).context("lookup"));
            assert!(is_malformed(&e), "{s:?}");
        }
        for e in [Error::Cancelled, Error::OutOfMemory, Error::UnexpectedEof] {
            assert_eq!(classify_encryption_dictionary_error(e.clone()), e);
        }
        // A missing dictionary object is malformed through the whole path.
        let mut x = table();
        x.encrypt = Some(Ref {
            num: 7,
            generation: 0,
        });
        x.e = Some(Enc {
            v: 4,
            ..Enc::default()
        });
        let e = validate_encryption_version(&mut x, &no_cancel()).unwrap_err();
        assert!(is_malformed(&e));
        let e = default_crypt_filter_encrypted(&mut x, "StrF").unwrap_err();
        assert!(is_malformed(&e));
    }

    #[test]
    fn invalid_version_is_malformed() {
        let mut x = table();
        let e = validate_encrypt_length(&mut x, &Dict::default(), 7).unwrap_err();
        assert!(is_malformed(&e));
    }

    #[test]
    fn aes256_parameter_errors_are_malformed() {
        let d = dict(vec![("OE", hex(31)), ("UE", hex(32)), ("Perms", hex(16))]);
        assert!(is_malformed(
            &validate_aes256_parameters(&d, 6).unwrap_err()
        ));
        assert!(validate_aes256_parameters(&d, 4).unwrap().0.is_empty());
    }

    #[test]
    fn o_and_u_lengths() {
        // Relaxed mode digests short entries below revision 5.
        let d = dict(vec![("O", hex(32)), ("U", hex(31))]);
        assert!(validate_o_and_u(&d, 4).is_ok());
        // Revisions 5 and 6 never do.
        for r in [5, 6] {
            for (o, u) in [(47, 48), (48, 47)] {
                let d = dict(vec![("O", hex(o)), ("U", hex(u))]);
                assert!(is_malformed(&validate_o_and_u(&d, r).unwrap_err()), "R{r}");
            }
        }
        let mut d = encrypt_dict(true, 256, 0);
        d.set(b"O", hex(47));
        let e = supported_encryption(&mut table(), &d).unwrap_err();
        assert!(is_malformed(&e));
        assert!(e.to_string().contains("\"O\""));
    }

    #[test]
    fn revision_7_is_unsupported() {
        let mut d = encrypt_dict(true, 256, 0);
        d.set(b"R", int(7));
        let e = supported_encryption(&mut table(), &d).unwrap_err();
        assert!(is_unsupported(&e));
        assert!(e.to_string().contains("encrypt \"R\" 7"));
    }

    // Port of TestSupportedEncryptionValidatesVAndR, TestEncryptionSetupWithoutPDFVersion and
    // TestSetupEncryptionKeyRejectsV5R4 (the relaxed cases).
    #[test]
    fn v_and_r_must_agree() {
        let cases: [(i64, i64, bool); 9] = [
            (256, 2, true),
            (256, 3, true),
            (256, 4, true),
            (128, 5, true),
            (128, 6, true),
            (256, 5, false),
            (256, 6, false),
            (128, 4, false),
            (256, 8, true),
        ];
        for (key_length, r, expect_malformed) in cases {
            let mut d = encrypt_dict(true, key_length, -4);
            d.set(b"R", int(r));
            if key_length == 128 && r >= 5 {
                d.set(b"O", hex(48));
                d.set(b"U", hex(48));
                d.set(b"OE", hex(32));
                d.set(b"UE", hex(32));
                d.set(b"Perms", hex(16));
            }
            // The bootstrap checks must not need the PDF version.
            let mut x = XRefTable::new(Config::default());
            let res = supported_encryption(&mut x, &d);
            if expect_malformed {
                assert!(is_malformed(&res.unwrap_err()), "{key_length} R{r}");
            } else {
                assert!(res.is_ok(), "{key_length} R{r}");
            }
        }
    }

    #[test]
    fn supported_encryption_reads_the_parameters() {
        let mut x = table();
        let e = supported_encryption(&mut x, &encrypt_dict(true, 128, -4)).unwrap();
        assert_eq!((e.v, e.l, e.r, e.p, e.emd), (4, 128, 4, -4, true));
        assert_eq!(e.o.len(), 32);
        assert!(x.aes4_strings && x.aes4_streams && !x.aes4_embedded_streams);

        let mut x = table();
        let e = supported_encryption(&mut x, &encrypt_dict(true, 256, 0)).unwrap();
        assert_eq!(
            (e.v, e.l, e.r, e.oe.len(), e.perms.len()),
            (5, 256, 6, 32, 16)
        );
        assert!(x.aes4_strings && x.aes4_streams);

        let mut x = table();
        let e = supported_encryption(&mut x, &encrypt_dict(false, 40, 0)).unwrap();
        assert_eq!((e.v, e.l, e.r), (1, 40, 2));
        assert!(!x.aes4_strings && !x.aes4_streams && !x.aes4_embedded_streams);
    }

    // Port of TestSupportedEncryptionResolvesIndirectIntegers and
    // TestSupportedEncryptionRejectsInvalidIndirectInteger.
    #[test]
    fn indirect_integers_resolve() {
        let mut d = encrypt_dict(true, 128, -4);
        let mut x = table();
        for (key, nr) in [("V", 7), ("Length", 8), ("R", 9), ("P", 10)] {
            let v = d.find(key.as_bytes()).cloned().unwrap();
            put(&mut x, nr, v);
            d.set(key.as_bytes(), reference(nr));
        }
        let e = supported_encryption(&mut x, &d).unwrap();
        assert_eq!((e.v, e.l, e.r, e.p), (4, 128, 4, -4));

        // Object 7 does not exist and object 8 is a name.
        let mut x = table();
        put(&mut x, 8, name("wrong"));
        for nr in [7, 8] {
            let mut d = encrypt_dict(true, 128, -4);
            d.set(b"V", reference(nr));
            let e = supported_encryption(&mut x, &d).unwrap_err();
            assert!(is_malformed(&e), "object {nr}");
        }
    }

    #[test]
    fn errors_name_the_encryption_object() {
        let mut d = encrypt_dict(true, 128, -4);
        d.set(b"V", int(9));
        let mut x = encrypted(Dict::default(), 4, 4);
        let e = supported_encryption(&mut x, &d).unwrap_err();
        assert!(e.to_string().contains("obj#1"), "{e}");
    }

    // Port of TestValidateEncryptPermissionsResolvesEncryptMetadata and
    // TestValidateEncryptPermissionsRejectsInvalidEncryptMetadata.
    #[test]
    fn encrypt_metadata_resolves() {
        let mut x = table();
        put(&mut x, 1, Object::Bool(true));
        put(&mut x, 2, Object::Bool(false));
        put(&mut x, 3, Object::Null);
        let cases = [
            (None, true),
            (Some(Object::Null), true),
            (Some(reference(3)), true),
            (Some(Object::Bool(true)), true),
            (Some(Object::Bool(false)), false),
            (Some(reference(1)), true),
            (Some(reference(2)), false),
        ];
        for (value, want) in cases {
            let mut d = dict(vec![("P", int(-4))]);
            if let Some(v) = value.clone() {
                d.set(b"EncryptMetadata", v);
            }
            let (_, got) = validate_encrypt_permissions(&mut x, &d).unwrap();
            assert_eq!(got, want, "{value:?}");
        }
        // Object 1 is missing, object 2 has the wrong type.
        for n in [1, 2] {
            let mut x = table();
            if n == 2 {
                put(&mut x, 2, int(1));
            }
            let d = dict(vec![("P", int(-4)), ("EncryptMetadata", reference(n))]);
            let e = validate_encrypt_permissions(&mut x, &d).unwrap_err();
            assert!(is_malformed(&e));
            assert!(e.to_string().contains("EncryptMetadata"));
        }
    }

    #[test]
    fn p_is_normalised() {
        let mut x = table();
        let d = dict(vec![("P", int(4_294_967_292))]);
        assert_eq!(validate_encrypt_permissions(&mut x, &d).unwrap().0, -4);
        let d = dict(vec![("P", int(-(1 << 31) - 1))]);
        assert!(is_malformed(
            &validate_encrypt_permissions(&mut x, &d).unwrap_err()
        ));
    }

    // Port of TestValidateStmfIncludesEncryptEntryContext.
    #[test]
    fn stmf_error_names_the_entry() {
        let d = dict(vec![("StmF", name("StdCF"))]);
        let cf = dict(vec![(
            "StdCF",
            Object::Dict(dict(vec![("CFM", name("bad")), ("Length", int(128))])),
        )]);
        let e = validate_selection(&mut table(), &d, &cf, 4, "StmF", false).unwrap_err();
        assert!(is_malformed(&e));
        assert!(e.to_string().contains("encrypt dict entry \"StmF\""));
    }

    // Port of TestValidateCryptFilterRejectsEFOpenForNonEmbeddedFiles.
    #[test]
    fn ef_open_only_for_embedded_files() {
        let d = dict(vec![
            ("AuthEvent", name("EFOpen")),
            ("CFM", name("AESV3")),
            ("Length", int(32)),
        ]);
        let mut x = table();
        assert!(is_malformed(
            &validate_crypt_filter(&mut x, &d, 5, false, false).unwrap_err()
        ));
        assert!(validate_crypt_filter(&mut x, &d, 5, true, false).unwrap());
    }

    // Port of TestCryptFilterLengthsIgnoresIdentity.
    #[test]
    fn crypt_filter_lengths_ignore_identity() {
        let d = dict(vec![(
            "CF",
            Object::Dict(dict(vec![("Identity", int(42))])),
        )]);
        let mut x = encrypted(d, 5, 6);
        x.header_version = Some(Version::V20);
        // The selections are absent, and Identity is skipped.
        validate_encryption_version(&mut x, &no_cancel()).unwrap();
    }

    #[test]
    fn pdf20_requires_every_filter_length() {
        let d = dict(vec![(
            "CF",
            Object::Dict(dict(vec![(
                "Other",
                Object::Dict(dict(vec![("CFM", name("AESV3"))])),
            )])),
        )]);
        let mut x = encrypted(d.clone(), 5, 6);
        x.header_version = Some(Version::V20);
        let e = validate_encryption_version(&mut x, &no_cancel()).unwrap_err();
        assert!(is_malformed(&e));
        // Before 2.0 a V5 file is checked, and the failure is digested.
        let mut x = encrypted(d, 5, 6);
        validate_encryption_version(&mut x, &no_cancel()).unwrap();
        let cancel = AtomicBool::new(true);
        let e = validate_encryption_version(&mut x, &cancel).unwrap_err();
        assert_eq!(e.root(), &Error::Cancelled);
        // Below V4 nothing is checked, with or without a dictionary.
        let mut x = table();
        validate_encryption_version(&mut x, &cancel).unwrap();
        let mut x = encrypted(Dict::default(), 3, 3);
        validate_encryption_version(&mut x, &cancel).unwrap();
    }

    #[test]
    fn cf_length_rules() {
        assert!(validate_cf_length(Some(16), Some(b"V2"), false).is_ok());
        // Legacy bit counts pass before PDF 2.0.
        assert!(validate_cf_length(Some(128), Some(b"V2"), false).is_ok());
        assert!(validate_cf_length(Some(128), Some(b"V2"), true).is_err());
        assert!(validate_cf_length(Some(44), Some(b"V2"), false).is_err());
        assert!(validate_cf_length(Some(128), Some(b"AESV2"), false).is_ok());
        assert!(validate_cf_length(Some(128), Some(b"AESV2"), true).is_err());
        assert!(validate_cf_length(Some(16), Some(b"AESV2"), true).is_ok());
        assert!(validate_cf_length(Some(16), Some(b"AESV3"), false).is_ok());
        assert!(validate_cf_length(None, Some(b"V2"), true).is_ok());
    }

    fn crypt_stream(parms: Option<Dict>) -> StreamDict {
        StreamDict {
            filter_pipeline: vec![PdfFilter {
                name: b"Crypt".to_vec(),
                decode_parms: parms,
            }],
            ..StreamDict::default()
        }
    }

    #[test]
    fn stream_methods() {
        let enc = encrypt_dict(true, 128, -4);
        let mut x = encrypted(enc.clone(), 4, 4);
        x.aes4_streams = true;
        let plain = StreamDict::default();
        assert_eq!(stream_crypt_method(&mut x, &plain).unwrap(), (true, true));
        // An explicit Crypt filter.
        let named = crypt_stream(Some(dict(vec![("Name", name("StdCF"))])));
        x.aes4_streams = false;
        assert_eq!(stream_crypt_method(&mut x, &named).unwrap(), (true, true));
        x.aes4_streams = true;
        let identity = crypt_stream(None);
        assert_eq!(
            stream_crypt_method(&mut x, &identity).unwrap(),
            (false, false)
        );
        let missing = crypt_stream(Some(dict(vec![("Name", name("Nope"))])));
        assert!(is_malformed(
            &stream_crypt_method(&mut x, &missing).unwrap_err()
        ));

        // No StmF: Identity.
        let mut no_stmf = enc.clone();
        no_stmf.delete(b"StmF");
        let mut x = encrypted(no_stmf, 4, 4);
        assert_eq!(stream_crypt_method(&mut x, &plain).unwrap(), (false, false));
        // Below V4 everything is encrypted, and the dictionary is not looked at.
        let mut x = encrypted(Dict::default(), 2, 3);
        assert_eq!(stream_crypt_method(&mut x, &plain).unwrap(), (true, false));

        // CFM None is the security handler's business.
        let cf = |cfm: &str| {
            let mut d = enc.clone();
            d.set(
                b"CF",
                Object::Dict(dict(vec![(
                    "StdCF",
                    Object::Dict(dict(vec![("CFM", name(cfm))])),
                )])),
            );
            d
        };
        let mut x = encrypted(cf("None"), 4, 4);
        assert!(is_unsupported(
            &stream_crypt_method(&mut x, &named).unwrap_err()
        ));
        // V2 is RC4, AESV3 is not allowed with V4.
        let mut x = encrypted(cf("V2"), 4, 4);
        x.aes4_streams = true;
        assert_eq!(stream_crypt_method(&mut x, &named).unwrap(), (true, false));
        let mut x = encrypted(cf("AESV3"), 4, 4);
        assert!(is_malformed(
            &stream_crypt_method(&mut x, &named).unwrap_err()
        ));
        let mut x = encrypted(cf("AESV4"), 4, 4);
        assert!(is_unsupported(
            &stream_crypt_method(&mut x, &named).unwrap_err()
        ));
    }

    #[test]
    fn strings_follow_strf() {
        let enc = encrypt_dict(true, 128, -4);
        let mut x = encrypted(enc.clone(), 4, 4);
        assert!(!strings_encrypted(&mut x).unwrap(), "no key yet");
        x.enc_key = vec![1; 16];
        assert!(strings_encrypted(&mut x).unwrap());
        let mut identity = enc.clone();
        identity.set(b"StrF", name("Identity"));
        let mut x = encrypted(identity, 4, 4);
        x.enc_key = vec![1; 16];
        assert!(!strings_encrypted(&mut x).unwrap());
        let mut bad = enc.clone();
        bad.set(b"StrF", int(1));
        let mut x = encrypted(bad.clone(), 4, 4);
        x.enc_key = vec![1; 16];
        assert!(is_malformed(&strings_encrypted(&mut x).unwrap_err()));
        // Below V4 the dictionary is not read.
        let mut x = encrypted(bad, 3, 3);
        x.enc_key = vec![1; 16];
        assert!(strings_encrypted(&mut x).unwrap());
    }

    #[test]
    fn missing_indirect_target_is_an_error() {
        let d = dict(vec![("V", reference(99))]);
        let e = encryption_integer_entry(&mut table(), &d, "V", "encrypt dict").unwrap_err();
        assert!(is_malformed(&e));
        assert!(e.to_string().contains("missing indirect target"));
    }
}
